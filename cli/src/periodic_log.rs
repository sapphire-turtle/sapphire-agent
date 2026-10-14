//! Periodic log (daily / weekly / monthly / yearly) generation, digest
//! catch-up, and system-prompt injection.
//!
//! Each log file lives under `memory/{daily,weekly,monthly,yearly}/{stem}.md`
//! and carries a YAML frontmatter `digest:` array of importance-ordered
//! bullets. The top-N items per kind are injected into the system prompt
//! (see `workspace.rs`) so the agent retains long-horizon context without
//! paying full-body token cost.
//!
//! Input strategy (calendar-aligned, not chained):
//! - Daily ← sessions for that local date.
//! - Weekly ← the 7 daily bodies of that ISO week (Mon–Sun).
//! - Monthly ← all daily bodies whose local date falls in that calendar
//!   month. Monthlies intentionally do **not** chain through weeklies:
//!   ISO weeks straddle month boundaries, so chaining would leak days
//!   across the boundary.
//! - Yearly ← the 12 monthly bodies of that calendar year.

use crate::acp_session::AcpSessionStore;
use crate::provider::{ChatMessage, ContentPart, Provider, Role};
use crate::session::{SessionMeta, SessionStore, StoredMessage};
use chrono::{Datelike, Duration, Local, NaiveDate, Weekday};
use sapphire_framework::workspace::WorkspaceState;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing::{info, warn};

// ---------------------------------------------------------------------------
// LogKind + path layout
// ---------------------------------------------------------------------------

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum LogKind {
    Daily,
    Weekly,
    Monthly,
    Yearly,
}

impl LogKind {
    /// Directory name under `memory/` for this kind.
    pub fn dir(self) -> &'static str {
        match self {
            LogKind::Daily => "daily",
            LogKind::Weekly => "weekly",
            LogKind::Monthly => "monthly",
            LogKind::Yearly => "yearly",
        }
    }

    /// Label used in generated file headings and LLM prompts.
    pub fn heading_label(self) -> &'static str {
        match self {
            LogKind::Daily => "Daily Log",
            LogKind::Weekly => "Weekly Log",
            LogKind::Monthly => "Monthly Log",
            LogKind::Yearly => "Yearly Log",
        }
    }
}

/// `memory/{namespace}/{dir}/{stem}.md` — workspace-relative path.
pub fn log_rel_path(namespace: &str, kind: LogKind, stem: &str) -> PathBuf {
    Path::new("memory")
        .join(namespace)
        .join(kind.dir())
        .join(format!("{stem}.md"))
}

/// Absolute path to the log file under `workspace_dir`.
pub(crate) fn log_abs_path(
    workspace_dir: &Path,
    namespace: &str,
    kind: LogKind,
    stem: &str,
) -> PathBuf {
    workspace_dir.join(log_rel_path(namespace, kind, stem))
}

// ---------------------------------------------------------------------------
// Stem helpers
// ---------------------------------------------------------------------------

/// `"YYYY-MM-DD"`.
pub fn daily_stem(date: NaiveDate) -> String {
    date.format("%Y-%m-%d").to_string()
}

/// `"YYYY-Www"` using the **ISO week-year** (which may differ from the
/// calendar year at year boundaries — e.g. 2023-01-01 → `"2022-W52"`).
pub fn weekly_stem(iso_year: i32, iso_week: u32) -> String {
    format!("{iso_year:04}-W{iso_week:02}")
}

/// `"YYYY-MM"`.
pub fn monthly_stem(year: i32, month: u32) -> String {
    format!("{year:04}-{month:02}")
}

/// `"YYYY"`.
pub fn yearly_stem(year: i32) -> String {
    format!("{year:04}")
}

// ---------------------------------------------------------------------------
// Date-range helpers (used for weekly/monthly inputs and injection ranges)
// ---------------------------------------------------------------------------

/// The 7 consecutive local dates of ISO week `(iso_year, iso_week)`,
/// ordered Monday → Sunday. Returns an empty vec if the (year, week) pair
/// is not a valid ISO date (e.g. week 53 in a short year).
pub fn days_of_iso_week(iso_year: i32, iso_week: u32) -> Vec<NaiveDate> {
    let Some(monday) = NaiveDate::from_isoywd_opt(iso_year, iso_week, Weekday::Mon) else {
        return Vec::new();
    };
    (0..7)
        .map(|offset| monday + Duration::days(offset))
        .collect()
}

/// Monthly stems for every completed month of `today`'s calendar year
/// (`{year}-01` .. `{year}-(today.month-1)`). Empty in January.
pub fn month_stems_in_year_before(today: NaiveDate) -> Vec<String> {
    let year = today.year();
    (1..today.month()).map(|m| monthly_stem(year, m)).collect()
}

/// Stems of every file in `memory/<namespace>/yearly/*.md`, sorted ascending.
pub fn existing_yearly_stems(workspace_dir: &Path, namespace: &str) -> Vec<String> {
    let dir = workspace_dir.join("memory").join(namespace).join("yearly");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out: Vec<String> = entries
        .flatten()
        .filter_map(|e| {
            let path = e.path();
            if path.extension().and_then(|s| s.to_str()) != Some("md") {
                return None;
            }
            path.file_stem()
                .and_then(|s| s.to_str())
                .map(str::to_string)
        })
        .collect();
    out.sort();
    out
}

/// Every local date in the given calendar month.
pub fn days_of_month(year: i32, month: u32) -> Vec<NaiveDate> {
    let Some(start) = NaiveDate::from_ymd_opt(year, month, 1) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut d = start;
    while d.month() == month {
        out.push(d);
        d += Duration::days(1);
    }
    out
}

/// ISO-week stems whose Monday falls in the calendar month of `today`,
/// excluding `today`'s own ISO week. Used for the "This Month's Digests"
/// injection block.
///
/// A week is included only when its Monday lies inside this month — this
/// way ISO weeks that straddle month boundaries aren't double-counted
/// across "# This Month's Digests" and the previous month's block.
pub fn week_stems_in_month_before(today: NaiveDate) -> Vec<String> {
    let current_iso = today.iso_week();
    let month = today.month();
    let Some(start) = NaiveDate::from_ymd_opt(today.year(), month, 1) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut d = start;
    while d.month() == month {
        if d.weekday() == Weekday::Mon {
            let iso = d.iso_week();
            if iso.year() != current_iso.year() || iso.week() != current_iso.week() {
                out.push(weekly_stem(iso.year(), iso.week()));
            }
        }
        d += Duration::days(1);
    }
    out
}

/// Daily stems within the *current* ISO week of `today` but strictly before
/// `today` itself. Used for the "This Week's Digests" injection block —
/// yesterday (which is `today - 1`) is intentionally excluded because its
/// full body is injected separately as the "Yesterday's Log" block.
///
/// Note: the range excludes `today`, so the caller passes `today` (the
/// current local date), not `yesterday`. On Monday the range collapses to
/// empty (only Monday itself is in the current ISO week, and it's excluded).
pub fn daily_stems_in_current_iso_week_before(today: NaiveDate) -> Vec<String> {
    let iso = today.iso_week();
    let Some(monday) = NaiveDate::from_isoywd_opt(iso.year(), iso.week(), Weekday::Mon) else {
        return Vec::new();
    };
    // yesterday is today - 1; we want [monday, yesterday) which equals
    // [monday, today - 1). On Monday, today-1 = Sunday of last week, which
    // isn't in the current ISO week — so we clamp to <= today-1.
    let yesterday = today - Duration::days(1);
    let mut out = Vec::new();
    let mut d = monday;
    while d < yesterday && d.iso_week() == iso {
        out.push(daily_stem(d));
        d += Duration::days(1);
    }
    out
}

// ---------------------------------------------------------------------------
// Daily log generation (moved from daily_log.rs)
// ---------------------------------------------------------------------------

/// Returns dates that have at least one session message in `namespace`'s
/// rooms but no corresponding daily log file under
/// `memory/<namespace>/daily/`, up to and including yesterday (local time).
///
/// `room_predicate` decides which sessions count toward this namespace.
/// Callers compute it from `Config::namespace_for_room`.
pub fn pending_daily_dates<F>(
    session_store: &SessionStore,
    acp_store: Option<&AcpSessionStore>,
    workspace_dir: &Path,
    namespace: &str,
    boundary_hour: u8,
    room_predicate: F,
) -> Vec<NaiveDate>
where
    F: Fn(&crate::session::SessionMeta) -> bool,
{
    let today = crate::session::local_date_for_timestamp(Local::now(), boundary_hour);
    let mut dates = Vec::new();
    // Borrow `room_predicate` for the ACP side first — `&F` also
    // satisfies `Fn(&SessionMeta) -> bool` — then move it into the
    // channel-store call below. Without this filter, every namespace's
    // catch-up would see every other namespace's ACP dates as pending
    // (see `AcpSessionStore::session_dates`'s doc).
    if let Some(acp) = acp_store {
        dates.extend(acp.session_dates(boundary_hour, &room_predicate));
    }
    dates.extend(session_store.all_session_dates_filtered(boundary_hour, room_predicate));
    dates.sort();
    dates.dedup();
    dates.retain(|&date| {
        date < today
            && !log_abs_path(workspace_dir, namespace, LogKind::Daily, &daily_stem(date)).exists()
    });
    dates
}

/// Merge `session_store`'s sessions for `date` with `acp_store`'s (when
/// present), filtered by `room_predicate` on both sides and ordered by
/// `created_at` — the exact input `format_sessions` reads.
///
/// Pure and read-only (no LLM call), so this is the seam the two-source
/// merge itself is tested through: `generate_daily_log` cannot be driven
/// from a test without a real provider, but this can.
fn sessions_for_day_merged<F>(
    session_store: &SessionStore,
    acp_store: Option<&AcpSessionStore>,
    date: NaiveDate,
    boundary_hour: u8,
    room_predicate: &F,
) -> Vec<(SessionMeta, Vec<StoredMessage>)>
where
    F: Fn(&crate::session::SessionMeta) -> bool,
{
    let mut sessions = session_store.sessions_for_day_filtered(date, boundary_hour, room_predicate);
    if let Some(acp) = acp_store {
        sessions.extend(
            acp.sessions_for_day(date, boundary_hour)
                .into_iter()
                .filter(|(meta, _)| room_predicate(meta)),
        );
    }
    sessions.sort_by_key(|(meta, _)| meta.created_at);
    sessions
}

/// Generate a daily log for `date` and write it to `memory/daily/YYYY-MM-DD.md`.
/// Returns `Ok(true)` if a log was written, `Ok(false)` if there were no
/// sessions for that day *and* no pre-existing draft at the target path.
///
/// Writes a YAML frontmatter `digest:` array (top-10 importance-ordered
/// bullets) alongside the Markdown summary body.
#[allow(clippy::too_many_arguments)]
pub async fn generate_daily_log<F>(
    session_store: &SessionStore,
    acp_store: Option<&AcpSessionStore>,
    provider: &dyn Provider,
    ws_state: &Arc<WorkspaceState>,
    workspace_dir: &Path,
    namespace: &str,
    date: NaiveDate,
    boundary_hour: u8,
    room_predicate: F,
) -> anyhow::Result<bool>
where
    F: Fn(&crate::session::SessionMeta) -> bool,
{
    let sessions = sessions_for_day_merged(
        session_store,
        acp_store,
        date,
        boundary_hour,
        &room_predicate,
    );
    let stem = daily_stem(date);
    let has_existing = read_body(workspace_dir, namespace, LogKind::Daily, &stem)
        .is_some_and(|b| !b.trim().is_empty());
    if sessions.is_empty() && !has_existing {
        info!("No sessions found for {date} in namespace '{namespace}', skipping daily log");
        return Ok(false);
    }

    let transcript = if sessions.is_empty() {
        String::new()
    } else {
        format_sessions(&sessions, date)
    };
    write_log_with_digest(
        provider,
        ws_state,
        workspace_dir,
        namespace,
        LogKind::Daily,
        &stem,
        "conversation transcripts",
        &transcript,
    )
    .await?;

    Ok(true)
}

// ---------------------------------------------------------------------------
// Weekly log generation
// ---------------------------------------------------------------------------

/// Generate the weekly log for ISO week `(iso_year, iso_week)` from the 7
/// daily bodies of that week. Returns `Ok(false)` if none of the dailies
/// exist (no work to do).
pub async fn generate_weekly_log(
    provider: &dyn Provider,
    ws_state: &Arc<WorkspaceState>,
    workspace_dir: &Path,
    namespace: &str,
    iso_year: i32,
    iso_week: u32,
) -> anyhow::Result<bool> {
    let stem = weekly_stem(iso_year, iso_week);
    let days = days_of_iso_week(iso_year, iso_week);
    let mut sections = Vec::new();
    for day in &days {
        if let Some(body) = read_body(workspace_dir, namespace, LogKind::Daily, &daily_stem(*day)) {
            sections.push(body);
        }
    }
    if sections.is_empty() {
        info!(
            "No daily logs found for ISO week {stem} in namespace '{namespace}', skipping weekly log"
        );
        return Ok(false);
    }
    let input = sections.join("\n\n---\n\n");
    let description = format!("daily logs for ISO week {stem}");
    write_log_with_digest(
        provider,
        ws_state,
        workspace_dir,
        namespace,
        LogKind::Weekly,
        &stem,
        &description,
        &input,
    )
    .await?;
    Ok(true)
}

// ---------------------------------------------------------------------------
// Monthly log generation
// ---------------------------------------------------------------------------

/// Generate the monthly log for `(year, month)` from *all* daily bodies in
/// that calendar month. Monthlies deliberately read dailies directly rather
/// than chaining through weeklies — ISO weeks straddle month boundaries,
/// so a weekly-based chain would leak days across the boundary. Returns
/// `Ok(false)` if no dailies exist for that month.
pub async fn generate_monthly_log(
    provider: &dyn Provider,
    ws_state: &Arc<WorkspaceState>,
    workspace_dir: &Path,
    namespace: &str,
    year: i32,
    month: u32,
) -> anyhow::Result<bool> {
    let stem = monthly_stem(year, month);
    let days = days_of_month(year, month);
    let mut sections = Vec::new();
    for day in &days {
        if let Some(body) = read_body(workspace_dir, namespace, LogKind::Daily, &daily_stem(*day)) {
            sections.push(body);
        }
    }
    if sections.is_empty() {
        info!(
            "No daily logs found for month {stem} in namespace '{namespace}', skipping monthly log"
        );
        return Ok(false);
    }
    let input = sections.join("\n\n---\n\n");
    let description = format!("daily logs for {stem}");
    write_log_with_digest(
        provider,
        ws_state,
        workspace_dir,
        namespace,
        LogKind::Monthly,
        &stem,
        &description,
        &input,
    )
    .await?;
    Ok(true)
}

// ---------------------------------------------------------------------------
// Yearly log generation
// ---------------------------------------------------------------------------

/// Generate the yearly log for `year` from the 12 monthly bodies of that
/// calendar year. Calendar month boundaries align with calendar year
/// boundaries, so this chain is clean. Returns `Ok(false)` if no monthly
/// logs exist for the target year.
pub async fn generate_yearly_log(
    provider: &dyn Provider,
    ws_state: &Arc<WorkspaceState>,
    workspace_dir: &Path,
    namespace: &str,
    year: i32,
) -> anyhow::Result<bool> {
    let stem = yearly_stem(year);
    let mut sections = Vec::new();
    for month in 1..=12 {
        let m_stem = monthly_stem(year, month);
        if let Some(body) = read_body(workspace_dir, namespace, LogKind::Monthly, &m_stem) {
            sections.push(body);
        }
    }
    if sections.is_empty() {
        info!(
            "No monthly logs found for year {stem} in namespace '{namespace}', skipping yearly log"
        );
        return Ok(false);
    }
    let input = sections.join("\n\n---\n\n");
    let description = format!("monthly logs for {stem}");
    write_log_with_digest(
        provider,
        ws_state,
        workspace_dir,
        namespace,
        LogKind::Yearly,
        &stem,
        &description,
        &input,
    )
    .await?;
    Ok(true)
}

// ---------------------------------------------------------------------------
// Read helpers (used both for upper-level log inputs and injection)
// ---------------------------------------------------------------------------

/// Return the Markdown body of `memory/{kind}/{stem}.md` with any YAML
/// frontmatter stripped. Returns `None` when the file does not exist or
/// cannot be read.
pub fn read_body(
    workspace_dir: &Path,
    namespace: &str,
    kind: LogKind,
    stem: &str,
) -> Option<String> {
    let raw = std::fs::read_to_string(log_abs_path(workspace_dir, namespace, kind, stem)).ok()?;
    Some(match crate::frontmatter::split(&raw) {
        Some((_, body)) => body.trim_start_matches('\n').to_string(),
        None => raw,
    })
}

/// Parse the first `n` items of the `digest:` array out of an already-read
/// log file's frontmatter. Split out so `Workspace`'s pinned digest reads
/// (which read the raw text through the pin map, not here) can share it.
/// Returns `None` when the frontmatter or digest is missing; returns
/// `Some(vec![])` when `digest: []` is explicitly empty.
pub fn digest_items(raw: &str, n: usize) -> Option<Vec<String>> {
    let (fm, _) = crate::frontmatter::split(raw)?;
    let mapping = crate::frontmatter::parse_mapping(fm);
    let seq = mapping.get("digest")?.as_sequence()?;
    Some(
        seq.iter()
            .filter_map(|v| v.as_str().map(|s| s.trim().to_string()))
            .take(n)
            .collect(),
    )
}

// ---------------------------------------------------------------------------
// Shared LLM-backed writer
// ---------------------------------------------------------------------------

/// Drive one LLM call that produces both a body summary and a YAML
/// frontmatter `digest:` array, then write `memory/{kind}/{stem}.md`.
///
/// The LLM is instructed to return a Markdown file shaped as:
/// ```text
/// ---
/// digest:
///   - …
/// ---
///
/// …body…
/// ```
///
/// `input_description` is a short phrase like `"conversation transcripts"`
/// or `"daily logs for ISO week 2026-W16"` used in the system prompt.
///
/// `input_body` is the concatenated source material (transcript, daily
/// bodies, etc.) passed as the user message.
#[allow(clippy::too_many_arguments)]
async fn write_log_with_digest(
    provider: &dyn Provider,
    ws_state: &Arc<WorkspaceState>,
    workspace_dir: &Path,
    namespace: &str,
    kind: LogKind,
    stem: &str,
    input_description: &str,
    input_body: &str,
) -> anyhow::Result<()> {
    let label = kind.heading_label();

    // Pre-existing draft at the target path — e.g. the agent pre-wrote
    // today's daily log with plans, or sketched a weekly entry ahead of
    // Monday. Feed it into the summariser so its facts aren't erased by
    // the overwrite.
    let existing_body =
        read_body(workspace_dir, namespace, kind, stem).filter(|b| !b.trim().is_empty());
    let has_existing = existing_body.is_some();

    let combined_input = match &existing_body {
        Some(existing) => format!(
            "## Existing draft at `memory/{}/{}/{}.md` — preserve its plans, commitments, and unresolved items\n\n{}\n\n---\n\n## New source material ({})\n\n{}",
            namespace,
            kind.dir(),
            stem,
            existing.trim_end(),
            input_description,
            input_body,
        ),
        None => input_body.to_string(),
    };

    let merge_note = if has_existing {
        "\n\nNote: the input begins with an existing draft of this log — content the user or agent pre-wrote (plans, appointments, notes). Merge its facts, commitments, and unresolved items into both the digest and the body. Do not drop draft content; only deduplicate when the new source material already covers the same point."
    } else {
        ""
    };

    let system = format!(
        "You are generating a concise {label} entry from {input_description}.{merge_note}

Output a Markdown file with this exact structure:

1. A YAML frontmatter block delimited by `---` lines, containing exactly one key:
   `digest`: an array of 5–10 importance-ordered bullet points (strings). Each
   bullet is a single short sentence capturing a key decision, topic, task
   completed, or unresolved item. Order: most important first.
2. After the closing `---`, a blank line, then the Markdown summary body.
   The body is free-form but should cover: key topics discussed, decisions
   made, tasks completed, and unresolved items.

Do NOT include a top-level `#` heading — one will be added automatically.
Write in the same language as the source material (Japanese if it is in
Japanese). Emit raw Markdown, not a fenced code block."
    );

    let user_msg = ChatMessage::user(combined_input);
    let response = provider.chat(Some(&system), &[user_msg], None).await?;
    let raw = response
        .text
        .unwrap_or_else(|| String::from("(no summary generated)"));

    let (digest, body) = parse_digest_response(&raw);
    if digest.is_empty() {
        warn!(
            "{} for {stem}: LLM response had no digest array; writing with empty digest",
            label
        );
    }
    let heading = format!("# {label}: {stem}\n\n");
    let file = serialize_log_file(&digest, &format!("{heading}{}\n", body.trim_end()))?;

    let rel = log_rel_path(namespace, kind, stem);
    ws_state.write_file(&rel, &file)?;
    info!("{} written: {}", label, rel.display());
    Ok(())
}

/// Parse an LLM response shaped as `--- digest:[…] --- body`. On malformed
/// input returns `(vec![], raw_body_with_wrappers_stripped)` so a best-effort
/// body still gets written.
fn parse_digest_response(raw: &str) -> (Vec<String>, String) {
    let cleaned = strip_code_fence(raw);
    match crate::frontmatter::split(&cleaned) {
        Some((fm, body)) => {
            let items = crate::frontmatter::parse_mapping(fm)
                .get("digest")
                .and_then(yaml_value_to_string_vec)
                .unwrap_or_default();
            (items, body.trim_start_matches('\n').to_string())
        }
        None => (Vec::new(), cleaned.trim_start_matches('\n').to_string()),
    }
}

/// Some models wrap everything in ` ```markdown … ``` `. Strip that.
fn strip_code_fence(raw: &str) -> String {
    let trimmed = raw.trim();
    let open_tags: &[&str] = &["```markdown\n", "```md\n", "```\n"];
    for tag in open_tags {
        if let Some(rest) = trimmed.strip_prefix(tag)
            && let Some(inner) = rest
                .strip_suffix("```")
                .or_else(|| rest.strip_suffix("```\n"))
        {
            return inner.trim_end().to_string();
        }
    }
    trimmed.to_string()
}

/// Coerce a `serde_norway::Value` into `Vec<String>`, dropping non-string items
/// with a warn log (non-strings indicate the LLM drifted from the format).
fn yaml_value_to_string_vec(v: &serde_norway::Value) -> Option<Vec<String>> {
    let seq = v.as_sequence()?;
    let mut out = Vec::with_capacity(seq.len());
    for item in seq {
        match item.as_str() {
            Some(s) => out.push(s.trim().to_string()),
            None => warn!("digest item is not a string: {item:?} — dropping"),
        }
    }
    Some(out)
}

/// Build a log file string from a digest list and body. Uses the shared
/// `frontmatter::serialize` helper so the formatting is consistent.
fn serialize_log_file(digest: &[String], body: &str) -> anyhow::Result<String> {
    let mut mapping = serde_norway::Mapping::new();
    let seq = digest
        .iter()
        .map(|s| serde_norway::Value::String(s.clone()))
        .collect::<Vec<_>>();
    mapping.insert(
        serde_norway::Value::String("digest".to_string()),
        serde_norway::Value::Sequence(seq),
    );
    crate::frontmatter::serialize(&mapping, body)
}

// ---------------------------------------------------------------------------
// Session → transcript formatting
// ---------------------------------------------------------------------------

/// Format sessions into a transcript suitable for LLM summarisation. Tool
/// use / tool result parts are skipped to keep context concise.
fn format_sessions(sessions: &[(SessionMeta, Vec<StoredMessage>)], date: NaiveDate) -> String {
    let mut parts = vec![format!("Conversations for {date}:\n")];

    for (meta, messages) in sessions {
        let thread = meta.thread_id.as_deref().unwrap_or("main");
        parts.push(format!(
            "## Session {} (thread: {})\n",
            meta.session_id, thread
        ));

        for msg in messages {
            let text: Vec<&str> = msg
                .parts
                .iter()
                .filter_map(|p| {
                    if let ContentPart::Text(t) = p {
                        Some(t.as_str())
                    } else {
                        None
                    }
                })
                .collect();

            if text.is_empty() {
                continue;
            }

            let role_label = match msg.role {
                Role::User => "User",
                Role::Assistant => "Assistant",
            };
            let local_ts = msg.timestamp.with_timezone(&Local);
            parts.push(format!(
                "[{}] {}: {}",
                local_ts.format("%H:%M"),
                role_label,
                text.join(" ")
            ));
        }

        parts.push(String::new());
    }

    parts.join("\n")
}

// ---------------------------------------------------------------------------
// Digest catch-up (back-fill existing dailies that lack a `digest:` array)
// ---------------------------------------------------------------------------

/// True if the file either has no frontmatter or is missing a non-empty
/// `digest:` sequence. Used by `catchup_missing_daily_digests` to decide
/// which existing dailies need an LLM-driven back-fill pass.
fn needs_digest_catchup(raw: &str) -> bool {
    match crate::frontmatter::split(raw) {
        None => true,
        Some((fm, _)) => {
            let mapping = crate::frontmatter::parse_mapping(fm);
            match mapping.get("digest").and_then(|v| v.as_sequence()) {
                Some(seq) => seq.is_empty(),
                None => true,
            }
        }
    }
}

/// Insert/replace the `digest:` key on a file's frontmatter while preserving
/// every other key and the body. If the file has no frontmatter at all,
/// synthesise one wrapping the existing content.
fn upsert_digest(raw: &str, digest: &[String]) -> anyhow::Result<String> {
    let (mut mapping, body) = match crate::frontmatter::split(raw) {
        Some((fm, body)) => (
            crate::frontmatter::parse_mapping(fm),
            body.trim_start_matches('\n').to_string(),
        ),
        None => (serde_norway::Mapping::new(), raw.to_string()),
    };
    let seq = digest
        .iter()
        .map(|s| serde_norway::Value::String(s.clone()))
        .collect::<Vec<_>>();
    mapping.insert(
        serde_norway::Value::String("digest".to_string()),
        serde_norway::Value::Sequence(seq),
    );
    crate::frontmatter::serialize(&mapping, &body)
}

/// Ask the LLM to emit a `digest:` frontmatter block for an existing log
/// body. Returns the parsed bullet list (empty `Vec` if parsing failed).
async fn extract_digest_from_body(
    provider: &dyn Provider,
    kind: LogKind,
    body: &str,
) -> anyhow::Result<Vec<String>> {
    let label = kind.heading_label();
    let system = format!(
        "You are extracting a concise digest from an existing {label} entry.

Output ONLY a YAML frontmatter block, exactly:

---
digest:
  - …
  - …
---

The `digest` array contains 5–10 importance-ordered bullet points (strings).
Each bullet is a single short sentence capturing a key decision, topic,
task completed, or unresolved item. Most important first. Write in the
same language as the source. Do NOT emit any text outside the frontmatter
block."
    );
    let user = ChatMessage::user(body);
    let resp = provider.chat(Some(&system), &[user], None).await?;
    let raw = resp.text.unwrap_or_default();
    let (items, _) = parse_digest_response(&raw);
    Ok(items)
}

/// Strip any leading frontmatter from `raw` and return the body. Used as
/// LLM input during digest back-fill so the model sees only the summary,
/// and by `Workspace`'s pinned yesterday-log injection (the raw text
/// arrives via the Workspace pin map, stripped here).
pub fn body_without_frontmatter(raw: &str) -> String {
    match crate::frontmatter::split(raw) {
        Some((_, body)) => body.trim_start_matches('\n').to_string(),
        None => raw.to_string(),
    }
}

/// Scan `memory/daily/*.md` and, for every file that lacks a non-empty
/// `digest:` array, call the LLM to produce one and merge it into the
/// file's frontmatter (preserving every other key — crucial for the
/// memory tool's `last_read_at` / `read_count` keys).
///
/// Errors are logged per-file, not propagated, so a single bad file does
/// not stall the pass. Returns the number of files successfully updated.
pub async fn catchup_missing_daily_digests(
    provider: &dyn Provider,
    ws_state: &Arc<WorkspaceState>,
    workspace_dir: &Path,
    namespace: &str,
) -> usize {
    let dir = workspace_dir.join("memory").join(namespace).join("daily");
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return 0,
    };

    let mut pending = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("md") {
            continue;
        }
        let Ok(raw) = std::fs::read_to_string(&path) else {
            continue;
        };
        if needs_digest_catchup(&raw) {
            pending.push((path, raw));
        }
    }

    if pending.is_empty() {
        return 0;
    }
    info!("Back-filling digest for {} daily log(s)…", pending.len());

    let mut filled = 0;
    for (path, raw) in pending {
        let display_path = path.display().to_string();
        let body = body_without_frontmatter(&raw);
        let items = match extract_digest_from_body(provider, LogKind::Daily, &body).await {
            Ok(v) if !v.is_empty() => v,
            Ok(_) => {
                warn!("Empty digest extracted from {display_path} — skipping");
                continue;
            }
            Err(e) => {
                warn!("Failed to extract digest from {display_path}: {e:#}");
                continue;
            }
        };
        let new_raw = match upsert_digest(&raw, &items) {
            Ok(s) => s,
            Err(e) => {
                warn!("Failed to merge digest for {display_path}: {e:#}");
                continue;
            }
        };
        let rel = path
            .strip_prefix(workspace_dir)
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|_| path.clone());
        match ws_state.write_file(&rel, &new_raw) {
            Ok(()) => {
                filled += 1;
                info!("Digest added: {}", rel.display());
            }
            Err(e) => warn!("Failed to write digest for {display_path}: {e:#}"),
        }
    }
    filled
}

// ---------------------------------------------------------------------------
// Startup catch-up
// ---------------------------------------------------------------------------

/// Generate all pending daily logs (e.g. from days the agent was offline).
/// Errors are logged but not propagated so startup is not blocked.
/// Returns the number of logs successfully generated, so callers can decide
/// whether to invalidate downstream caches.
#[allow(clippy::too_many_arguments)]
pub async fn catchup_pending_daily_logs<F>(
    session_store: &SessionStore,
    acp_store: Option<&AcpSessionStore>,
    provider: &dyn Provider,
    ws_state: &Arc<WorkspaceState>,
    workspace_dir: &Path,
    namespace: &str,
    boundary_hour: u8,
    room_predicate: F,
) -> usize
where
    F: Fn(&crate::session::SessionMeta) -> bool + Copy,
{
    let pending = pending_daily_dates(
        session_store,
        acp_store,
        workspace_dir,
        namespace,
        boundary_hour,
        room_predicate,
    );
    if pending.is_empty() {
        return 0;
    }
    info!(
        "Generating {} pending daily log(s) for namespace '{}'…",
        pending.len(),
        namespace
    );
    let mut generated = 0;
    for date in pending {
        match generate_daily_log(
            session_store,
            acp_store,
            provider,
            ws_state,
            workspace_dir,
            namespace,
            date,
            boundary_hour,
            room_predicate,
        )
        .await
        {
            Ok(true) => generated += 1,
            Ok(false) => {}
            Err(e) => warn!("Failed to generate daily log for {date} in '{namespace}': {e:#}"),
        }
    }
    generated
}

// ---------------------------------------------------------------------------
// Weekly / monthly / yearly catch-up
// ---------------------------------------------------------------------------

/// Distinct daily log dates on disk for `namespace` (i.e. stems in
/// `memory/<namespace>/daily/*.md` that parse as `YYYY-MM-DD`). Used as
/// the seed for weekly/monthly catch-up.
fn existing_daily_dates(workspace_dir: &Path, namespace: &str) -> Vec<NaiveDate> {
    let dir = workspace_dir.join("memory").join(namespace).join("daily");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("md") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if let Ok(date) = NaiveDate::parse_from_str(stem, "%Y-%m-%d") {
            out.push(date);
        }
    }
    out
}

/// Distinct monthly log stems on disk for `namespace` (i.e. files in
/// `memory/<namespace>/monthly/*.md` that parse as `YYYY-MM`). Used as
/// the seed for yearly catch-up.
fn existing_monthly_year_months(workspace_dir: &Path, namespace: &str) -> Vec<(i32, u32)> {
    let dir = workspace_dir.join("memory").join(namespace).join("monthly");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("md") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let parts: Vec<&str> = stem.split('-').collect();
        if parts.len() != 2 {
            continue;
        }
        if let (Ok(y), Ok(m)) = (parts[0].parse::<i32>(), parts[1].parse::<u32>())
            && (1..=12).contains(&m)
        {
            out.push((y, m));
        }
    }
    out
}

/// Returns ISO (year, week) pairs that have at least one daily log on disk
/// but no weekly log file yet, excluding `today`'s own ISO week. Sorted
/// ascending by (year, week).
pub fn pending_iso_weeks(
    workspace_dir: &Path,
    namespace: &str,
    today: NaiveDate,
) -> Vec<(i32, u32)> {
    use std::collections::BTreeSet;
    let current_iso = today.iso_week();
    let current_key = (current_iso.year(), current_iso.week());
    let mut weeks: BTreeSet<(i32, u32)> = BTreeSet::new();
    for date in existing_daily_dates(workspace_dir, namespace) {
        let iso = date.iso_week();
        let key = (iso.year(), iso.week());
        if key >= current_key {
            continue;
        }
        weeks.insert(key);
    }
    weeks
        .into_iter()
        .filter(|(y, w)| {
            !log_abs_path(
                workspace_dir,
                namespace,
                LogKind::Weekly,
                &weekly_stem(*y, *w),
            )
            .exists()
        })
        .collect()
}

/// Returns calendar (year, month) pairs that have at least one daily log on
/// disk but no monthly log file yet, excluding `today`'s own month. Sorted
/// ascending.
pub fn pending_months(workspace_dir: &Path, namespace: &str, today: NaiveDate) -> Vec<(i32, u32)> {
    use std::collections::BTreeSet;
    let current_key = (today.year(), today.month());
    let mut months: BTreeSet<(i32, u32)> = BTreeSet::new();
    for date in existing_daily_dates(workspace_dir, namespace) {
        let key = (date.year(), date.month());
        if key >= current_key {
            continue;
        }
        months.insert(key);
    }
    months
        .into_iter()
        .filter(|(y, m)| {
            !log_abs_path(
                workspace_dir,
                namespace,
                LogKind::Monthly,
                &monthly_stem(*y, *m),
            )
            .exists()
        })
        .collect()
}

/// Returns calendar years that have at least one monthly log on disk but no
/// yearly log file yet, excluding `today`'s own year. Sorted ascending.
pub fn pending_years(workspace_dir: &Path, namespace: &str, today: NaiveDate) -> Vec<i32> {
    use std::collections::BTreeSet;
    let current_year = today.year();
    let mut years: BTreeSet<i32> = BTreeSet::new();
    for (y, _m) in existing_monthly_year_months(workspace_dir, namespace) {
        if y >= current_year {
            continue;
        }
        years.insert(y);
    }
    years
        .into_iter()
        .filter(|y| {
            !log_abs_path(workspace_dir, namespace, LogKind::Yearly, &yearly_stem(*y)).exists()
        })
        .collect()
}

/// Generate every weekly log that is "pending" — i.e. has dailies on disk but
/// no weekly file, strictly before the current ISO week. Errors per stem are
/// logged but not propagated. Returns the count successfully generated.
pub async fn catchup_pending_weekly_logs(
    provider: &dyn Provider,
    ws_state: &Arc<WorkspaceState>,
    workspace_dir: &Path,
    namespace: &str,
    today: NaiveDate,
) -> usize {
    let pending = pending_iso_weeks(workspace_dir, namespace, today);
    if pending.is_empty() {
        return 0;
    }
    info!(
        "Generating {} pending weekly log(s) for namespace '{}'…",
        pending.len(),
        namespace
    );
    let mut generated = 0;
    for (iso_year, iso_week) in pending {
        match generate_weekly_log(
            provider,
            ws_state,
            workspace_dir,
            namespace,
            iso_year,
            iso_week,
        )
        .await
        {
            Ok(true) => generated += 1,
            Ok(false) => {}
            Err(e) => warn!(
                "Failed to generate weekly log for {}-W{:02} in '{}': {e:#}",
                iso_year, iso_week, namespace
            ),
        }
    }
    generated
}

/// Generate every monthly log that is "pending" — i.e. has dailies on disk
/// but no monthly file, strictly before the current calendar month.
pub async fn catchup_pending_monthly_logs(
    provider: &dyn Provider,
    ws_state: &Arc<WorkspaceState>,
    workspace_dir: &Path,
    namespace: &str,
    today: NaiveDate,
) -> usize {
    let pending = pending_months(workspace_dir, namespace, today);
    if pending.is_empty() {
        return 0;
    }
    info!(
        "Generating {} pending monthly log(s) for namespace '{}'…",
        pending.len(),
        namespace
    );
    let mut generated = 0;
    for (year, month) in pending {
        match generate_monthly_log(provider, ws_state, workspace_dir, namespace, year, month).await
        {
            Ok(true) => generated += 1,
            Ok(false) => {}
            Err(e) => warn!(
                "Failed to generate monthly log for {:04}-{:02} in '{}': {e:#}",
                year, month, namespace
            ),
        }
    }
    generated
}

/// Generate every yearly log that is "pending" — i.e. has monthlies on disk
/// but no yearly file, strictly before the current calendar year.
pub async fn catchup_pending_yearly_logs(
    provider: &dyn Provider,
    ws_state: &Arc<WorkspaceState>,
    workspace_dir: &Path,
    namespace: &str,
    today: NaiveDate,
) -> usize {
    let pending = pending_years(workspace_dir, namespace, today);
    if pending.is_empty() {
        return 0;
    }
    info!(
        "Generating {} pending yearly log(s) for namespace '{}'…",
        pending.len(),
        namespace
    );
    let mut generated = 0;
    for year in pending {
        match generate_yearly_log(provider, ws_state, workspace_dir, namespace, year).await {
            Ok(true) => generated += 1,
            Ok(false) => {}
            Err(e) => warn!(
                "Failed to generate yearly log for {year:04} in '{}': {e:#}",
                namespace
            ),
        }
    }
    generated
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// The local date "now" belongs to under `boundary_hour`.
    ///
    /// Deliberately not `Local::now().date_naive()`. A timestamp before
    /// the boundary hour belongs to the *previous* local day, so the
    /// naive date and a day window disagree for those hours — which made
    /// these tests pass all day and fail only when CI happened to run
    /// between midnight and 04:00.
    fn today(boundary_hour: u8) -> NaiveDate {
        crate::session::local_date_for_timestamp(chrono::Local::now(), boundary_hour)
    }

    #[test]
    fn stems_basic() {
        let d = NaiveDate::from_ymd_opt(2026, 4, 15).unwrap();
        assert_eq!(daily_stem(d), "2026-04-15");
        assert_eq!(monthly_stem(2026, 4), "2026-04");
        assert_eq!(yearly_stem(2026), "2026");
    }

    #[test]
    fn weekly_stem_iso_year_may_differ_from_calendar_year() {
        // 2023-01-01 is Sunday → ISO week 52 of 2022.
        let d = NaiveDate::from_ymd_opt(2023, 1, 1).unwrap();
        let iso = d.iso_week();
        assert_eq!(weekly_stem(iso.year(), iso.week()), "2022-W52");
        // 2024-12-30 is Monday → ISO week 1 of 2025.
        let d = NaiveDate::from_ymd_opt(2024, 12, 30).unwrap();
        let iso = d.iso_week();
        assert_eq!(weekly_stem(iso.year(), iso.week()), "2025-W01");
    }

    #[test]
    fn weekly_stem_zero_padded() {
        assert_eq!(weekly_stem(2026, 3), "2026-W03");
        assert_eq!(weekly_stem(2026, 16), "2026-W16");
    }

    #[test]
    fn paths() {
        let abs = log_abs_path(Path::new("/ws"), "default", LogKind::Weekly, "2026-W16");
        assert_eq!(abs, PathBuf::from("/ws/memory/default/weekly/2026-W16.md"));
        let rel = log_rel_path("default", LogKind::Monthly, "2026-04");
        assert_eq!(rel, PathBuf::from("memory/default/monthly/2026-04.md"));
        // Non-default namespace.
        let abs = log_abs_path(Path::new("/ws"), "user_nsfw", LogKind::Daily, "2026-05-05");
        assert_eq!(
            abs,
            PathBuf::from("/ws/memory/user_nsfw/daily/2026-05-05.md")
        );
    }

    #[test]
    fn parse_digest_response_happy_path() {
        let raw = "---\ndigest:\n  - item A\n  - item B\n  - item C\n---\n\n# body line\n";
        let (items, body) = parse_digest_response(raw);
        assert_eq!(items, vec!["item A", "item B", "item C"]);
        assert_eq!(body.trim(), "# body line");
    }

    #[test]
    fn parse_digest_response_missing_frontmatter() {
        let raw = "# Just a body\n\nno digest here\n";
        let (items, body) = parse_digest_response(raw);
        assert!(items.is_empty());
        assert!(body.contains("no digest here"));
    }

    #[test]
    fn parse_digest_response_strips_code_fence() {
        let raw = "```markdown\n---\ndigest:\n  - x\n---\n\nbody\n```";
        let (items, body) = parse_digest_response(raw);
        assert_eq!(items, vec!["x"]);
        assert_eq!(body.trim(), "body");
    }

    #[test]
    fn parse_digest_response_drops_non_string_items() {
        let raw = "---\ndigest:\n  - ok\n  - 42\n  - also ok\n---\n\nbody\n";
        let (items, _) = parse_digest_response(raw);
        assert_eq!(items, vec!["ok", "also ok"]);
    }

    #[test]
    fn serialize_log_file_includes_digest() {
        let out = serialize_log_file(&["a".into(), "b".into()], "# H\n\nbody\n").unwrap();
        assert!(out.starts_with("---\n"));
        assert!(out.contains("digest:"));
        assert!(out.contains("- a"));
        assert!(out.contains("- b"));
        assert!(out.contains("# H"));
        assert!(out.contains("body"));
    }

    #[test]
    fn needs_digest_catchup_cases() {
        assert!(needs_digest_catchup("# body only\n"));
        assert!(needs_digest_catchup("---\nother: 1\n---\n# body\n"));
        assert!(needs_digest_catchup("---\ndigest: []\n---\n# body\n"));
        assert!(!needs_digest_catchup(
            "---\ndigest:\n  - one\n---\n# body\n"
        ));
    }

    #[test]
    fn upsert_digest_preserves_unrelated_keys() {
        let raw = "---\nlast_read_at: 2026-04-15T21:57:41.312Z\nread_count: 3\n---\n\n# Daily Log: 2026-04-15\n\nbody\n";
        let new = upsert_digest(raw, &["one".into(), "two".into()]).unwrap();
        let (fm, body) = crate::frontmatter::split(&new).unwrap();
        let mapping = crate::frontmatter::parse_mapping(fm);
        assert!(mapping.get("last_read_at").is_some());
        assert_eq!(mapping.get("read_count").and_then(|v| v.as_u64()), Some(3));
        let digest: Vec<String> = mapping
            .get("digest")
            .and_then(|v| v.as_sequence())
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert_eq!(digest, vec!["one", "two"]);
        assert!(body.contains("# Daily Log: 2026-04-15"));
        assert!(body.contains("body"));
    }

    #[test]
    fn upsert_digest_synthesises_frontmatter_when_missing() {
        let raw = "# Daily Log: 2026-04-15\n\nbody\n";
        let new = upsert_digest(raw, &["only".into()]).unwrap();
        assert!(new.starts_with("---\n"));
        let (fm, body) = crate::frontmatter::split(&new).unwrap();
        let mapping = crate::frontmatter::parse_mapping(fm);
        let digest: Vec<String> = mapping
            .get("digest")
            .and_then(|v| v.as_sequence())
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert_eq!(digest, vec!["only"]);
        assert!(body.contains("# Daily Log: 2026-04-15"));
        assert!(body.contains("body"));
    }

    #[test]
    fn days_of_iso_week_returns_seven_ordered_days() {
        // 2026-W16: Monday 2026-04-13 .. Sunday 2026-04-19.
        let days = days_of_iso_week(2026, 16);
        assert_eq!(days.len(), 7);
        assert_eq!(days[0], NaiveDate::from_ymd_opt(2026, 4, 13).unwrap());
        assert_eq!(days[6], NaiveDate::from_ymd_opt(2026, 4, 19).unwrap());
        assert_eq!(days[0].weekday(), Weekday::Mon);
        assert_eq!(days[6].weekday(), Weekday::Sun);
    }

    #[test]
    fn daily_stems_this_week_on_thursday() {
        // Today = Thursday 2026-04-16; yesterday = Wed 2026-04-15.
        // Current ISO week starts Mon 2026-04-13.
        // Expected range [Mon, Wed) = [Mon, Tue].
        let thu = NaiveDate::from_ymd_opt(2026, 4, 16).unwrap();
        let stems = daily_stems_in_current_iso_week_before(thu);
        assert_eq!(stems, vec!["2026-04-13", "2026-04-14"]);
    }

    #[test]
    fn daily_stems_this_week_on_monday_is_empty() {
        // Today = Mon 2026-04-13; yesterday = Sun of *last* ISO week.
        // The current ISO week has nothing before "yesterday".
        let mon = NaiveDate::from_ymd_opt(2026, 4, 13).unwrap();
        assert!(daily_stems_in_current_iso_week_before(mon).is_empty());
    }

    #[test]
    fn daily_stems_this_week_on_tuesday_is_empty() {
        // Today = Tue 2026-04-14; yesterday = Mon 2026-04-13 (same week).
        // Range [Mon, Mon) is empty.
        let tue = NaiveDate::from_ymd_opt(2026, 4, 14).unwrap();
        assert!(daily_stems_in_current_iso_week_before(tue).is_empty());
    }

    #[test]
    fn daily_stems_this_week_on_sunday_gives_four_days() {
        // Today = Sun 2026-04-19; yesterday = Sat 2026-04-18.
        // Range [Mon 2026-04-13, Sat 2026-04-18) = Mon..Fri = 5 days.
        let sun = NaiveDate::from_ymd_opt(2026, 4, 19).unwrap();
        let stems = daily_stems_in_current_iso_week_before(sun);
        assert_eq!(
            stems,
            vec![
                "2026-04-13",
                "2026-04-14",
                "2026-04-15",
                "2026-04-16",
                "2026-04-17"
            ]
        );
    }

    #[test]
    fn days_of_month_april_2026_is_30_days() {
        let days = days_of_month(2026, 4);
        assert_eq!(days.len(), 30);
        assert_eq!(days[0], NaiveDate::from_ymd_opt(2026, 4, 1).unwrap());
        assert_eq!(days[29], NaiveDate::from_ymd_opt(2026, 4, 30).unwrap());
    }

    #[test]
    fn days_of_month_december_is_31_days() {
        assert_eq!(days_of_month(2026, 12).len(), 31);
    }

    #[test]
    fn days_of_month_february_leap_year() {
        assert_eq!(days_of_month(2024, 2).len(), 29);
        assert_eq!(days_of_month(2025, 2).len(), 28);
    }

    #[test]
    fn week_stems_in_month_before_april_16_2026() {
        // April 2026 has Mondays on 6, 13, 20, 27. Today is Thu 04-16 (ISO
        // week 16). Exclude week 16; include weeks with Mondays 04-06, 04-20,
        // 04-27 — but only Mondays that are _before_ today shouldn't matter
        // for this helper: it returns every in-month Monday except the
        // current ISO week. So expected: 15, 17, 18.
        let thu = NaiveDate::from_ymd_opt(2026, 4, 16).unwrap();
        let stems = week_stems_in_month_before(thu);
        assert_eq!(stems, vec!["2026-W15", "2026-W17", "2026-W18"]);
    }

    #[test]
    fn week_stems_in_month_before_may_1_2026() {
        // May 1 2026 is a Friday in ISO week 18 (whose Monday Apr 27 is in
        // April, not May — so no May Monday is in the current ISO week).
        // May's Mondays: 4, 11, 18, 25 → weeks 19, 20, 21, 22.
        let fri = NaiveDate::from_ymd_opt(2026, 5, 1).unwrap();
        let stems = week_stems_in_month_before(fri);
        assert_eq!(stems, vec!["2026-W19", "2026-W20", "2026-W21", "2026-W22"]);
    }

    #[test]
    fn month_stems_in_year_before_april() {
        let apr = NaiveDate::from_ymd_opt(2026, 4, 16).unwrap();
        assert_eq!(
            month_stems_in_year_before(apr),
            vec!["2026-01", "2026-02", "2026-03"]
        );
    }

    #[test]
    fn month_stems_in_year_before_january_is_empty() {
        let jan = NaiveDate::from_ymd_opt(2026, 1, 1).unwrap();
        assert!(month_stems_in_year_before(jan).is_empty());
    }

    #[test]
    fn month_stems_in_year_before_december() {
        let dec = NaiveDate::from_ymd_opt(2026, 12, 31).unwrap();
        let stems = month_stems_in_year_before(dec);
        assert_eq!(stems.len(), 11);
        assert_eq!(stems.first().map(String::as_str), Some("2026-01"));
        assert_eq!(stems.last().map(String::as_str), Some("2026-11"));
    }

    fn make_tempdir() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    fn write_stub(path: &Path, body: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    #[test]
    fn pending_iso_weeks_excludes_current_week_and_existing_files() {
        let td = make_tempdir();
        let root = td.path();
        // Daily logs across three ISO weeks: W15, W16 (current), W14.
        // Today = Thursday 2026-04-16 (ISO week 16).
        for d in [
            "2026-04-06", // Mon W15
            "2026-04-09", // Thu W15
            "2026-04-13", // Mon W16 (current)
            "2026-04-14", // Tue W16 (current)
            "2026-03-30", // Mon W14
        ] {
            write_stub(
                &root.join("memory/default/daily").join(format!("{d}.md")),
                "# body\n",
            );
        }
        // An existing weekly file for W14 — should be filtered out.
        write_stub(
            &root.join("memory/default/weekly/2026-W14.md"),
            "---\ndigest: []\n---\nbody\n",
        );

        let today = NaiveDate::from_ymd_opt(2026, 4, 16).unwrap();
        let pending = pending_iso_weeks(root, "default", today);
        assert_eq!(pending, vec![(2026, 15)]);
    }

    #[test]
    fn pending_months_excludes_current_month_and_existing_files() {
        let td = make_tempdir();
        let root = td.path();
        for d in [
            "2026-02-01",
            "2026-02-15", // Feb
            "2026-03-05", // Mar
            "2026-04-02",
            "2026-04-16", // Apr (current)
        ] {
            write_stub(
                &root.join("memory/default/daily").join(format!("{d}.md")),
                "# body\n",
            );
        }
        write_stub(
            &root.join("memory/default/monthly/2026-02.md"),
            "---\ndigest: []\n---\nbody\n",
        );

        let today = NaiveDate::from_ymd_opt(2026, 4, 16).unwrap();
        let pending = pending_months(root, "default", today);
        assert_eq!(pending, vec![(2026, 3)]);
    }

    #[test]
    fn pending_years_from_existing_monthlies() {
        let td = make_tempdir();
        let root = td.path();
        for stem in ["2024-06", "2024-12", "2025-01", "2026-03"] {
            write_stub(
                &root
                    .join("memory/default/monthly")
                    .join(format!("{stem}.md")),
                "# body\n",
            );
        }
        // Existing yearly for 2024 — should be excluded.
        write_stub(
            &root.join("memory/default/yearly/2024.md"),
            "---\ndigest: []\n---\nbody\n",
        );

        let today = NaiveDate::from_ymd_opt(2026, 4, 16).unwrap();
        let pending = pending_years(root, "default", today);
        // 2024 exists → skipped. 2025 missing → pending. 2026 = current → skipped.
        assert_eq!(pending, vec![2025]);
    }

    #[test]
    fn pending_enumerations_handle_missing_directories() {
        let td = make_tempdir();
        let today = NaiveDate::from_ymd_opt(2026, 4, 16).unwrap();
        assert!(pending_iso_weeks(td.path(), "default", today).is_empty());
        assert!(pending_months(td.path(), "default", today).is_empty());
        assert!(pending_years(td.path(), "default", today).is_empty());
    }

    #[test]
    fn upsert_digest_replaces_existing_digest() {
        let raw = "---\ndigest:\n  - stale\nother: keep\n---\n\nbody\n";
        let new = upsert_digest(raw, &["fresh".into()]).unwrap();
        let (fm, _) = crate::frontmatter::split(&new).unwrap();
        let mapping = crate::frontmatter::parse_mapping(fm);
        assert_eq!(mapping.get("other").and_then(|v| v.as_str()), Some("keep"));
        let digest: Vec<String> = mapping
            .get("digest")
            .and_then(|v| v.as_sequence())
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert_eq!(digest, vec!["fresh"]);
    }

    // ── the two-source merge (`sessions_for_day_merged` / `pending_daily_dates`) ──
    //
    // `generate_daily_log` can't be driven from a test without a real LLM
    // provider (`write_log_with_digest` calls one), so the merge it relies
    // on is exercised directly through the pure helper instead.

    /// Build a fresh channel-store + ACP-store pair under one temp dir,
    /// namespaced under `sessions/{channel,acp}` the way `main.rs` lays
    /// them out.
    fn merge_test_stores(td: &tempfile::TempDir) -> (SessionStore, AcpSessionStore) {
        let sessions_base = td.path().join("sessions");
        let channel_store = SessionStore::new(sessions_base.join("channel"), "channel", None);
        let tool_payload_cache =
            crate::tool_payload_cache::ToolPayloadCache::open(td.path().join("tool-payloads"))
                .unwrap();
        let acp_store = AcpSessionStore::new(sessions_base.join("acp"), Some(tool_payload_cache));
        (channel_store, acp_store)
    }

    /// Append a message to an existing channel-store session with an
    /// explicit (possibly backdated) timestamp, bypassing `append()`
    /// (which always stamps `Utc::now()`). Mirrors the file format
    /// documented at the top of `session.rs` and the hand-crafted-line
    /// pattern `session.rs`'s own tests already use for backdating.
    fn backdate_channel_message(
        store: &SessionStore,
        session_id: &str,
        at: chrono::DateTime<chrono::Utc>,
        text: &str,
    ) {
        store
            .append_at_for_test(session_id, at, &crate::provider::ChatMessage::user(text))
            .unwrap();
    }

    /// Same idea for the ACP store: a raw `Line::Message` written
    /// directly to the file so `at` can be backdated. Mirrors the
    /// forged-event pattern `acp_session.rs`'s own tests use.
    fn backdate_acp_message(
        store: &AcpSessionStore,
        session_id: &str,
        at: chrono::DateTime<chrono::Utc>,
        text: &str,
    ) {
        let line = serde_json::json!({
            "kind": "message",
            "id": uuid::Uuid::now_v7(),
            "parent": serde_json::Value::Null,
            "at": at,
            "role": "user",
            "parts": [{"text": text}],
        });
        store.write_line_for_test(session_id, &line);
    }

    /// Both sources reach the transcript input, interleaved by
    /// `created_at` rather than one block after the other. The ACP
    /// session is created *first* (so it must sort first); the merge
    /// pushes channel-store results first and extends with the ACP
    /// results after, so this only passes if the trailing
    /// `sort_by_key(created_at)` actually re-orders rather than leaving
    /// push order intact.
    #[test]
    fn merged_sessions_for_day_interleaves_both_stores_by_created_at() {
        let td = make_tempdir();
        let (channel_store, acp_store) = merge_test_stores(&td);

        acp_store.create("acp1", "default", "/p").unwrap();
        acp_store
            .append_message("acp1", &ChatMessage::user("acp said this first"))
            .unwrap();

        let chan_id = channel_store
            .create_session(&("room1".to_string(), None), "matrix", "default")
            .unwrap();
        channel_store
            .append(&chan_id, &ChatMessage::user("channel said this second"))
            .unwrap();

        let today = today(4);
        let predicate = |_: &SessionMeta| true;
        let sessions =
            sessions_for_day_merged(&channel_store, Some(&acp_store), today, 4, &predicate);

        assert_eq!(
            sessions.len(),
            2,
            "both sources must contribute: {sessions:?}"
        );
        assert!(
            sessions[0].0.created_at <= sessions[1].0.created_at,
            "result must be ordered by created_at: {:?}",
            sessions
                .iter()
                .map(|(m, _)| (&m.session_id, m.created_at))
                .collect::<Vec<_>>()
        );
        assert_eq!(
            sessions[0].0.session_id, "acp1",
            "the ACP session was created first and must sort first — proves the merge \
             re-sorts by created_at instead of leaving the channel-store half \
             (pushed first) ahead of the ACP half (extended in after) regardless of \
             actual timing"
        );
    }

    /// The room predicate is applied to the ACP side too, not just the
    /// channel-store side — the property `Heartbeat::namespace_for_session`'s
    /// `"acp"` arm exists to make true. An ACP session's own header
    /// namespace decides which namespace's log it belongs to.
    #[test]
    fn merged_sessions_for_day_applies_the_room_predicate_to_acp_too() {
        let td = make_tempdir();
        let (channel_store, acp_store) = merge_test_stores(&td);

        acp_store.create("work-session", "work", "/p").unwrap();
        acp_store
            .append_message("work-session", &ChatMessage::user("work stuff"))
            .unwrap();
        acp_store
            .create("default-session", "default", "/p")
            .unwrap();
        acp_store
            .append_message("default-session", &ChatMessage::user("default stuff"))
            .unwrap();

        let today = today(4);

        let work_predicate = |meta: &SessionMeta| meta.namespace.as_deref() == Some("work");
        let work_sessions =
            sessions_for_day_merged(&channel_store, Some(&acp_store), today, 4, &work_predicate);
        assert_eq!(
            work_sessions
                .iter()
                .map(|(m, _)| m.session_id.as_str())
                .collect::<Vec<_>>(),
            vec!["work-session"],
            "only the 'work'-namespace ACP session should pass a 'work' predicate: \
             {work_sessions:?}"
        );

        let default_predicate = |meta: &SessionMeta| meta.namespace.as_deref() == Some("default");
        let default_sessions = sessions_for_day_merged(
            &channel_store,
            Some(&acp_store),
            today,
            4,
            &default_predicate,
        );
        assert_eq!(
            default_sessions
                .iter()
                .map(|(m, _)| m.session_id.as_str())
                .collect::<Vec<_>>(),
            vec!["default-session"],
            "only the 'default'-namespace ACP session should pass a 'default' \
             predicate: {default_sessions:?}"
        );
    }

    /// `pending_daily_dates` merges the two stores' date sets and dedups
    /// them: a date only the ACP store knows about must still surface as
    /// pending, and a date both stores know about must appear exactly
    /// once, not twice.
    #[test]
    fn pending_daily_dates_merges_and_dedups_across_stores() {
        let td = make_tempdir();
        let (channel_store, acp_store) = merge_test_stores(&td);
        let boundary_hour = 4u8;

        let today = crate::session::local_date_for_timestamp(Local::now(), boundary_hour);
        let channel_only_day = today - Duration::days(4);
        let acp_only_day = today - Duration::days(3);
        let shared_day = today - Duration::days(2);

        let (channel_only_start, _) = crate::session::day_window(channel_only_day, boundary_hour);
        let (acp_only_start, _) = crate::session::day_window(acp_only_day, boundary_hour);
        let (shared_start, _) = crate::session::day_window(shared_day, boundary_hour);

        // Channel store knows channel_only_day and shared_day.
        let chan1 = channel_store
            .create_session(&("room1".to_string(), None), "matrix", "default")
            .unwrap();
        backdate_channel_message(
            &channel_store,
            &chan1,
            channel_only_start + Duration::hours(1),
            "channel only",
        );
        let chan2 = channel_store
            .create_session(&("room1".to_string(), None), "matrix", "default")
            .unwrap();
        backdate_channel_message(
            &channel_store,
            &chan2,
            shared_start + Duration::hours(1),
            "channel side of the shared day",
        );

        // ACP store knows acp_only_day and shared_day.
        acp_store.create("acp-only", "default", "/p").unwrap();
        backdate_acp_message(
            &acp_store,
            "acp-only",
            acp_only_start + Duration::hours(1),
            "acp only",
        );
        acp_store.create("acp-shared", "default", "/p").unwrap();
        backdate_acp_message(
            &acp_store,
            "acp-shared",
            shared_start + Duration::hours(2),
            "acp side of the shared day",
        );

        let predicate = |_: &SessionMeta| true;
        let pending = pending_daily_dates(
            &channel_store,
            Some(&acp_store),
            td.path(),
            "default",
            boundary_hour,
            predicate,
        );

        assert!(
            pending.contains(&channel_only_day),
            "a channel-only date must still be pending: {pending:?}"
        );
        assert!(
            pending.contains(&acp_only_day),
            "a date only the ACP store knows about must be reported as pending: {pending:?}"
        );
        assert_eq!(
            pending.iter().filter(|&&d| d == shared_day).count(),
            1,
            "a date both stores know about must appear exactly once, not twice: {pending:?}"
        );
    }

    /// A date that only has messages in another namespace's ACP session
    /// must not be reported pending for this namespace. Before this was
    /// fixed, `pending_daily_dates` pulled every ACP date unfiltered, so
    /// `generate_daily_log` would find no in-namespace sessions for that
    /// date, write nothing, and the phantom date would stay pending
    /// forever — re-walked on every startup and catch-up tick.
    #[test]
    fn pending_daily_dates_does_not_leak_another_namespaces_acp_date() {
        let td = make_tempdir();
        let (channel_store, acp_store) = merge_test_stores(&td);
        let boundary_hour = 4u8;

        // `pending_daily_dates` only reports dates strictly before
        // today (today's own log isn't due yet), so the message must
        // be backdated for this to exercise the "pending" path at all.
        let today = crate::session::local_date_for_timestamp(Local::now(), boundary_hour);
        let (yesterday_start, _) =
            crate::session::day_window(today - Duration::days(1), boundary_hour);
        acp_store.create("work-only", "work", "/p").unwrap();
        backdate_acp_message(
            &acp_store,
            "work-only",
            yesterday_start + Duration::hours(1),
            "work stuff",
        );

        let default_predicate = |meta: &SessionMeta| meta.namespace.as_deref() == Some("default");
        let pending = pending_daily_dates(
            &channel_store,
            Some(&acp_store),
            td.path(),
            "default",
            boundary_hour,
            default_predicate,
        );

        assert!(
            pending.is_empty(),
            "a date belonging only to the 'work' namespace's ACP session must not be \
             pending for 'default': {pending:?}"
        );

        let work_predicate = |meta: &SessionMeta| meta.namespace.as_deref() == Some("work");
        let work_pending = pending_daily_dates(
            &channel_store,
            Some(&acp_store),
            td.path(),
            "work",
            boundary_hour,
            work_predicate,
        );
        assert_eq!(
            work_pending.len(),
            1,
            "the 'work' namespace's own predicate must still see its date: {work_pending:?}"
        );
    }
}
