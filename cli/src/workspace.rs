use crate::channel::RoomInfo;
use crate::config::DigestConfig;
use crate::periodic_log::{self, LogKind};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tokio::sync::Mutex;
use tracing::{debug, info};

/// Maximum characters loaded from a single workspace file (matches openclaw default).
const MAX_FILE_CHARS: usize = 20_000;

// ---------------------------------------------------------------------------
// Workspace file definitions
// ---------------------------------------------------------------------------

/// A single workspace file entry: one or more candidate filenames to try (in
/// order), plus the Markdown heading to use when injecting into the system
/// prompt.
struct WorkspaceFileDef {
    /// Candidate filenames tried in order; the first one found is used.
    candidates: &'static [&'static str],
    /// Heading inserted above the file content (e.g. "# Agent Instructions").
    heading: &'static str,
}

/// Ordered list of workspace files, following openclaw's convention.
/// Files that don't exist are silently skipped. MEMORY.md is **not**
/// included here — it lives under `memory/<namespace>/MEMORY.md` and is
/// assembled per turn from the room's namespace chain.
/// See: https://github.com/openclaw/openclaw (src/agents/workspace.ts)
static WORKSPACE_FILES: &[WorkspaceFileDef] = &[
    // openclaw uses "AGENTS.md" (plural); we also accept "AGENT.md" for
    // users who create the file without the trailing 's'.
    WorkspaceFileDef {
        candidates: &["AGENTS.md", "AGENT.md"],
        heading: "# Agent Instructions",
    },
    WorkspaceFileDef {
        candidates: &["SOUL.md"],
        heading: "# Soul",
    },
    WorkspaceFileDef {
        candidates: &["IDENTITY.md"],
        heading: "# Identity",
    },
    WorkspaceFileDef {
        candidates: &["USER.md"],
        heading: "# User",
    },
    WorkspaceFileDef {
        candidates: &["TOOLS.md"],
        heading: "# Tools",
    },
    WorkspaceFileDef {
        candidates: &["BOOTSTRAP.md"],
        heading: "# Bootstrap",
    },
];

// ---------------------------------------------------------------------------
// Pinned file cache
// ---------------------------------------------------------------------------

// Pinned file contents live in `Workspace::cache`: a path → raw-contents
// map where a `None` value pins a *missing* file, so a file created later
// stays invisible until the pin map is cleared. Workspace files are read
// through `read_file` (which truncates to `MAX_FILE_CHARS` on every read);
// periodic-log files are read through `read_pinned_log` (raw contents —
// their callers strip frontmatter or parse digests out of them). Every
// injected path is distinct, so the two kinds never collide on a key.

// ---------------------------------------------------------------------------
// Workspace
// ---------------------------------------------------------------------------

/// Reads workspace files (AGENTS.md, SOUL.md, IDENTITY.md, USER.md, TOOLS.md,
/// HEARTBEAT.md, BOOTSTRAP.md, MEMORY.md) and assembles them into the system
/// prompt on every turn.
///
/// File contents are **pinned** per path: the first read wins and every
/// later read serves the pinned copy, so an edit on disk no longer changes
/// the prompt bytes (and busts the provider prompt cache) until
/// [`Workspace::clear_pinned_cache`] is called.
pub struct Workspace {
    dir: PathBuf,
    digest_cfg: DigestConfig,
    cache: Mutex<HashMap<PathBuf, Option<String>>>,
}

impl Workspace {
    pub fn new(dir: PathBuf, digest_cfg: DigestConfig) -> Self {
        info!("Workspace dir: {}", dir.display());
        Self {
            dir,
            digest_cfg,
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// Build the full system prompt:
    /// 1. Base system_prompt from config (if any)
    /// 2. Each workspace file that exists, in openclaw order
    /// 3. Chained MEMORY.md from the room's namespace and its includes
    /// 4. Previous day's daily log (if it exists)
    ///
    /// `namespace_chain` is the DFS-pre-order list of namespaces this
    /// room reads from — typically computed via
    /// `Config::resolve_namespace_chain(Config::namespace_for_room(room_id))`.
    /// The first entry is the room's own namespace; later entries are
    /// included parents.
    ///
    /// `cwd`, when present, is this turn's ACP session working directory
    /// (see `TurnHost::cwd` in `serve`): an absolute path on the *client's*
    /// machine, injected verbatim as its own block right after the room
    /// block. It is never canonicalised or existence-checked here — the
    /// server cannot resolve a path that lives on another machine.
    pub async fn build_system_prompt(
        &self,
        base: Option<&str>,
        boundary_hour: u8,
        namespace_chain: &[String],
        room_info: Option<&RoomInfo>,
        cwd: Option<&str>,
    ) -> String {
        let mut parts: Vec<String> = Vec::new();

        if let Some(b) = base.filter(|s| !s.is_empty()) {
            parts.push(b.to_string());
        }

        // Deliberately no current date/time block. It changed on every
        // turn, so the prompt prefix every provider caches never matched
        // the previous one and each request re-processed the whole
        // system prompt. Temporal awareness now comes from the
        // `current_time` tool, which the model calls when it needs it.
        let now_local = chrono::Local::now();

        // Channel-side room metadata (Matrix room.name+topic, Discord
        // channel.name+topic, or device-supplied "voice channel with X").
        // Injected near the top so the model knows where it's speaking
        // before reading any other instructions.
        if let Some(info) = room_info {
            parts.push(render_room_info(info));
        }

        // The ACP session's working directory, right after the room block
        // and before the workspace files: like the room block, it tells the
        // model where it is working, so it belongs ahead of the
        // instructions. Injected verbatim (see this method's doc).
        if let Some(cwd) = cwd {
            parts.push(render_working_directory(cwd));
        }

        for def in WORKSPACE_FILES {
            if let Some((filename, content)) = self.read_first_existing(def.candidates).await {
                debug!("Injecting workspace file: {filename}");
                parts.push(format!("{}\n\n{content}", def.heading));
            }
        }

        // Per-namespace MEMORY.md, chained.
        if let Some(block) = self.build_memory_block(namespace_chain).await {
            parts.push(block);
        }

        // Inject periodic logs (yesterday's full body + digest blocks for
        // this week / month / year / past years).
        let today = crate::session::local_date_for_timestamp(now_local, boundary_hour);
        self.inject_periodic_logs(&mut parts, today, namespace_chain)
            .await;

        parts.join("\n\n---\n\n")
    }

    /// Read MEMORY.md from each namespace in the chain (closest first) and
    /// concatenate as `## <namespace>` subsections under one combined
    /// `# Memory` heading. Namespaces with no MEMORY.md are skipped.
    /// Returns `None` if the entire chain has no MEMORY.md to inject.
    async fn build_memory_block(&self, namespace_chain: &[String]) -> Option<String> {
        let mut subsections = Vec::new();
        for ns in namespace_chain {
            let rel = format!("memory/{ns}/MEMORY.md");
            if let Some(content) = self.read_file(&rel).await
                && !content.trim().is_empty()
            {
                subsections.push(format!("## {ns}\n\n{content}"));
            }
        }
        if subsections.is_empty() {
            None
        } else {
            Some(format!("# Memory\n\n{}", subsections.join("\n\n")))
        }
    }

    /// Append log injection blocks to `parts`: yesterday's full body
    /// (room's own namespace only) plus top-N digest blocks aggregated
    /// across the namespace chain. Log contents are read through the same
    /// pinned map as `read_file`, so the injected bytes stay stable until
    /// the pin map is cleared.
    async fn inject_periodic_logs(
        &self,
        parts: &mut Vec<String>,
        today: chrono::NaiveDate,
        namespace_chain: &[String],
    ) {
        // Yesterday's full body — read only from the room's direct
        // namespace (the first chain entry). Reading from the chain would
        // balloon the body verbatim; parents' yesterday context is
        // conveyed through digest items below.
        if let Some(direct_ns) = namespace_chain.first()
            && let Some(yesterday) = today.pred_opt()
        {
            let stem = periodic_log::daily_stem(yesterday);
            let body = self
                .read_pinned_log(direct_ns, LogKind::Daily, &stem)
                .await
                .map(|raw| periodic_log::body_without_frontmatter(&raw));
            if let Some(body) = body.filter(|b| !b.trim().is_empty()) {
                let truncated = truncate_chars(&body, MAX_FILE_CHARS);
                debug!("Injecting yesterday's daily log from '{direct_ns}': {yesterday}");
                parts.push(format!("# Yesterday's Log\n\n{truncated}"));
            }
        }

        // "This Week's Digests" — daily files in `[iso_week_start, yesterday)`.
        if self.digest_cfg.daily_items > 0 {
            let stems = periodic_log::daily_stems_in_current_iso_week_before(today);
            if let Some(b) = self
                .build_chained_digest_block(
                    "# This Week's Digests",
                    namespace_chain,
                    LogKind::Daily,
                    &stems,
                    self.digest_cfg.daily_items,
                )
                .await
            {
                parts.push(b);
            }
        }

        // "This Month's Digests" — weekly files whose Monday is in this
        // calendar month, excluding the current ISO week.
        if self.digest_cfg.weekly_items > 0 {
            let stems = periodic_log::week_stems_in_month_before(today);
            if let Some(b) = self
                .build_chained_digest_block(
                    "# This Month's Digests",
                    namespace_chain,
                    LogKind::Weekly,
                    &stems,
                    self.digest_cfg.weekly_items,
                )
                .await
            {
                parts.push(b);
            }
        }

        // "This Year's Digests" — monthly files Jan..(current month - 1).
        if self.digest_cfg.monthly_items > 0 {
            let stems = periodic_log::month_stems_in_year_before(today);
            if let Some(b) = self
                .build_chained_digest_block(
                    "# This Year's Digests",
                    namespace_chain,
                    LogKind::Monthly,
                    &stems,
                    self.digest_cfg.monthly_items,
                )
                .await
            {
                parts.push(b);
            }
        }

        // "Past Years' Digests" — every yearly file on disk for any
        // namespace in the chain. We compute a per-namespace stem list
        // since each namespace can have its own yearly files.
        if self.digest_cfg.yearly_items > 0 {
            let mut subsections: Vec<String> = Vec::new();
            for ns in namespace_chain {
                let stems = periodic_log::existing_yearly_stems(&self.dir, ns);
                for stem in &stems {
                    if let Some(items) = self
                        .read_digest_top_n(ns, LogKind::Yearly, stem, self.digest_cfg.yearly_items)
                        .await
                    {
                        if items.is_empty() {
                            continue;
                        }
                        let bullets: Vec<String> =
                            items.into_iter().map(|i| format!("- {i}")).collect();
                        subsections.push(format!("## {ns}/{stem}\n\n{}", bullets.join("\n")));
                    }
                }
            }
            if !subsections.is_empty() {
                parts.push(format!(
                    "# Past Years' Digests\n\n{}",
                    subsections.join("\n\n")
                ));
            }
        }
    }

    /// Assemble a heading + per-`(namespace, stem)` bulleted subsections of
    /// top-N digest items, walking each stem against every namespace in the
    /// chain. Each subsection is introduced by `## {namespace}/{stem}`.
    /// `(namespace, stem)` pairs whose pinned file is missing or has an
    /// empty digest are skipped. Returns `None` if no pair produced a
    /// subsection.
    async fn build_chained_digest_block(
        &self,
        heading: &str,
        namespace_chain: &[String],
        kind: LogKind,
        stems: &[String],
        n: usize,
    ) -> Option<String> {
        let mut subsections = Vec::new();
        for ns in namespace_chain {
            for stem in stems {
                let items = self
                    .read_digest_top_n(ns, kind, stem, n)
                    .await
                    .unwrap_or_default();
                if items.is_empty() {
                    continue;
                }
                let bullets: Vec<String> = items.into_iter().map(|i| format!("- {i}")).collect();
                subsections.push(format!("## {ns}/{stem}\n\n{}", bullets.join("\n")));
            }
        }
        if subsections.is_empty() {
            None
        } else {
            debug!("Injecting {heading} ({} subsection(s))", subsections.len());
            Some(format!("{heading}\n\n{}", subsections.join("\n\n")))
        }
    }

    /// Drop every pinned file so the next read re-reads from disk. Called by
    /// `Agent::invalidate_system_prompts` after the daily log is regenerated
    /// or when the agent calls the `refresh_system_prompt` tool.
    pub async fn clear_pinned_cache(&self) {
        self.cache.lock().await.clear();
    }

    /// Try each candidate filename in order; return the first one found.
    async fn read_first_existing(&self, candidates: &[&str]) -> Option<(String, String)> {
        for &filename in candidates {
            if let Some(content) = self.read_file(filename).await {
                return Some((filename.to_string(), content));
            }
        }
        None
    }

    /// Read a workspace file from the pinned cache. Once a path has been
    /// read it is pinned until `clear_pinned_cache` — edits on disk are NOT
    /// reflected until then. A missing file is also pinned (as `None`) so a
    /// file created later is invisible until the next clear. Pinned because
    /// the system prompt is rebuilt per turn and a byte change busts the
    /// provider prompt cache.
    async fn read_file(&self, filename: &str) -> Option<String> {
        let path = self.dir.join(filename);
        let content = self.read_pinned(&path).await;
        content.map(|raw| truncate_chars(&raw, MAX_FILE_CHARS))
    }

    /// Return the pinned contents of `path`, reading it from disk on first
    /// access and pinning the result — including a missing file, pinned as
    /// `None` — in the map shared by `read_file` and the periodic-log
    /// injection reads.
    async fn read_pinned(&self, path: &Path) -> Option<String> {
        let mut cache = self.cache.lock().await;
        if let Some(entry) = cache.get(path) {
            return entry.clone();
        }
        let content = std::fs::read_to_string(path).ok();
        cache.insert(path.to_path_buf(), content.clone());
        content
    }

    /// Pinned raw read of one periodic-log file at
    /// `periodic_log::log_abs_path(&self.dir, namespace, kind, stem)`.
    /// Shares the map `read_file` pins into; unlike `read_file` the stored
    /// contents are raw (frontmatter included) — callers strip or parse.
    async fn read_pinned_log(&self, namespace: &str, kind: LogKind, stem: &str) -> Option<String> {
        let path = periodic_log::log_abs_path(&self.dir, namespace, kind, stem);
        self.read_pinned(&path).await
    }

    /// First `n` items of the `digest:` array in a pinned log file's
    /// frontmatter. Returns `None` when the file or its digest is missing;
    /// returns `Some(vec![])` when `digest: []` is explicitly empty.
    async fn read_digest_top_n(
        &self,
        namespace: &str,
        kind: LogKind,
        stem: &str,
        n: usize,
    ) -> Option<Vec<String>> {
        let raw = self.read_pinned_log(namespace, kind, stem).await?;
        periodic_log::digest_items(&raw, n)
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Render an ACP session's working directory into a Markdown block
/// injected into the system prompt. The path is inserted **verbatim**: it
/// names a location on the *client's* machine, so canonicalising it or
/// checking it against this server's filesystem would be wrong here (see
/// `AcpSession::cwd` in `serve::acp`). Kept free-standing for the same
/// reason as `render_room_info`.
fn render_working_directory(cwd: &str) -> String {
    format!("# Current Workspace\n\n- Working directory: {cwd}")
}

/// Render `RoomInfo` into a Markdown block injected into the system prompt.
/// Kept free-standing (not a method on `RoomInfo`) so the channel module
/// stays unaware of system-prompt formatting.
fn render_room_info(info: &RoomInfo) -> String {
    let mut body = format!("- Channel: {}\n- Name: {}", info.kind, info.name);
    if let Some(desc) = info.description.as_ref().filter(|s| !s.trim().is_empty()) {
        body.push_str(&format!("\n- Description: {}", desc.trim()));
    }
    format!("# Current Room\n\n{body}")
}

/// Truncate `s` to at most `max_chars` Unicode scalar values.
fn truncate_chars(s: &str, max_chars: usize) -> String {
    let mut chars = s.chars();
    let truncated: String = (&mut chars).take(max_chars).collect();
    if chars.next().is_some() {
        format!("{truncated}\n\n[... truncated to {max_chars} characters ...]")
    } else {
        truncated
    }
}

impl Workspace {
    /// The workspace root `build_system_prompt` reads `AGENTS.md`,
    /// `SOUL.md`, etc. from. Also the workspace-relative path base the
    /// autonomous loop names session files by. Tests use it to write a
    /// workspace file before asserting on the rendered prompt.
    pub(crate) fn dir(&self) -> &Path {
        &self.dir
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace_with_agents_md(dir: &tempfile::TempDir) -> Workspace {
        std::fs::write(
            dir.path().join("AGENTS.md"),
            "# how this workspace works\n\nread the files.\n",
        )
        .unwrap();
        Workspace::new(dir.path().to_path_buf(), DigestConfig::default())
    }

    /// The property the prompt cache depends on: two turns a second
    /// apart must produce the *same bytes*.
    ///
    /// A clock in the system prompt broke this — every turn changed the
    /// cached prefix, so a provider re-processed the whole prompt (and,
    /// behind it, the whole tool-result-laden history) instead of
    /// hitting its cache. `current_time` answers the same question
    /// without moving the prefix.
    #[tokio::test]
    async fn the_system_prompt_is_byte_identical_across_turns() {
        let dir = tempfile::TempDir::new().unwrap();
        let ws = workspace_with_agents_md(&dir);
        let chain = ["default".to_string()];

        let first = ws
            .build_system_prompt(Some("base"), 4, &chain, None, None)
            .await;
        let second = ws
            .build_system_prompt(Some("base"), 4, &chain, None, None)
            .await;

        assert_eq!(first, second);
        assert!(
            !first.contains("Current Date and Time"),
            "the clock belongs in `current_time`, not in the prompt: {first}"
        );
        assert!(first.contains("read the files."), "{first}");
    }

    /// ピン留めの本体性質：編集してもクリアするまでバイト列は不変。
    #[tokio::test]
    async fn the_system_prompt_pins_edits_until_cleared() {
        let dir = tempfile::TempDir::new().unwrap();
        let ws = workspace_with_agents_md(&dir);
        let chain = ["default".to_string()];

        let before = ws
            .build_system_prompt(Some("base"), 4, &chain, None, None)
            .await;

        // 編集する（mtime が変わる）
        std::fs::write(
            dir.path().join("AGENTS.md"),
            "# how this works\n\nedited content.\n",
        )
        .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;

        let pinned = ws
            .build_system_prompt(Some("base"), 4, &chain, None, None)
            .await;
        assert_eq!(before, pinned, "ピン留め中は編集が反映されない");
        assert!(!pinned.contains("edited content."));

        ws.clear_pinned_cache().await;
        let refreshed = ws
            .build_system_prompt(Some("base"), 4, &chain, None, None)
            .await;
        assert!(
            refreshed.contains("edited content."),
            "クリア後は反映される"
        );
    }

    /// ピン留め中に新規作成されたファイルは、クリアするまで見えない。
    #[tokio::test]
    async fn newly_created_files_are_invisible_until_cleared() {
        let dir = tempfile::TempDir::new().unwrap();
        let ws = Workspace::new(dir.path().to_path_buf(), DigestConfig::default());
        let chain = ["default".to_string()];

        let before = ws
            .build_system_prompt(Some("base"), 4, &chain, None, None)
            .await;
        std::fs::write(dir.path().join("SOUL.md"), "# Soul\n\nnew soul.\n").unwrap();

        let pinned = ws
            .build_system_prompt(Some("base"), 4, &chain, None, None)
            .await;
        assert_eq!(before, pinned, "新規ファイルはクリアまで見えない");

        ws.clear_pinned_cache().await;
        let refreshed = ws
            .build_system_prompt(Some("base"), 4, &chain, None, None)
            .await;
        assert!(refreshed.contains("new soul."));
    }

    #[tokio::test]
    async fn a_cwd_reaches_the_prompt_as_its_own_block() {
        let dir = tempfile::TempDir::new().unwrap();
        let ws = Workspace::new(dir.path().to_path_buf(), DigestConfig::default());
        let chain = ["default".to_string()];
        let prompt = ws
            .build_system_prompt(Some("base"), 4, &chain, None, Some("/work/proj"))
            .await;
        assert!(
            prompt.contains("# Current Workspace\n\n- Working directory: /work/proj"),
            "{prompt}"
        );
    }

    #[tokio::test]
    async fn the_cwd_block_precedes_the_workspace_files() {
        let dir = tempfile::TempDir::new().unwrap();
        let ws = workspace_with_agents_md(&dir);
        let chain = ["default".to_string()];
        let prompt = ws
            .build_system_prompt(Some("base"), 4, &chain, None, Some("/work/proj"))
            .await;
        let cwd_at = prompt.find("# Current Workspace").expect("cwd block");
        let files_at = prompt
            .find("# Agent Instructions")
            .expect("workspace file block");
        assert!(
            cwd_at < files_at,
            "cwd @{cwd_at}, files @{files_at}: {prompt}"
        );
    }

    #[tokio::test]
    async fn no_cwd_adds_no_block() {
        let dir = tempfile::TempDir::new().unwrap();
        let ws = Workspace::new(dir.path().to_path_buf(), DigestConfig::default());
        let chain = ["default".to_string()];
        let prompt = ws
            .build_system_prompt(Some("base"), 4, &chain, None, None)
            .await;
        assert!(!prompt.contains("# Current Workspace"), "{prompt}");
    }
}
