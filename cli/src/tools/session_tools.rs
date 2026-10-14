//! Tools that answer the two questions the system prompt used to answer
//! by growing: "what time is it?" and "what else is going on?".
//!
//! Both used to be injected text — a `# Current Date and Time` block
//! rebuilt every turn, and a `# Today's Cross-Session Notes` block
//! rebuilt every time a background summariser ran. Injected text sits at
//! the front of the prompt, so each rewrite invalidated the provider's
//! prompt cache for the *whole* system prompt and every turn paid to
//! re-process it. On a long ACP session with tool traffic behind it,
//! that is the difference between an instant reply and one that takes
//! minutes.
//!
//! As tools, the same facts cost a round trip only when the model
//! actually wants them, and the prompt prefix stays byte-identical
//! across turns.

use crate::acp_session::{AcpSessionStore, EventBody, StoredPart};
use crate::config::Config;
use crate::provider::{ContentPart, Role, ToolSpec};
use crate::session::{SessionMeta, SessionRow, SessionStore, StoredMessage, day_window};
use crate::tools::{Tool, ToolKind};
use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, Local, Utc};
use serde_json::json;
use std::sync::Arc;

// ---------------------------------------------------------------------------
// current_time
// ---------------------------------------------------------------------------

/// The clock, on demand.
pub struct CurrentTimeTool {
    boundary_hour: u8,
    spec: ToolSpec,
}

impl CurrentTimeTool {
    pub fn new(boundary_hour: u8) -> Self {
        Self {
            boundary_hour,
            spec: ToolSpec {
                name: "current_time".into(),
                description: "Current date and time on the agent host, in local time, \
                    UTC, and as the agent's logical day (which starts at the configured \
                    day-boundary hour, not at midnight). \
                    The system prompt carries no clock — call this whenever the answer \
                    depends on the date or time, including before writing anything \
                    date-stamped (memory entries, logs, notes)."
                    .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                }),
            },
        }
    }
}

#[async_trait]
impl Tool for CurrentTimeTool {
    fn kind(&self) -> ToolKind {
        ToolKind::Read
    }

    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn execute(&self, _input: &serde_json::Value) -> Result<String> {
        let now = Local::now();
        let logical = crate::session::local_date_for_timestamp(now, self.boundary_hour);
        Ok(format!(
            "Local: {} ({})\nUTC:   {}\nAgent day: {} (day boundary {:02}:00 local)",
            now.format("%Y-%m-%d %H:%M:%S %z"),
            now.format("%A"),
            now.with_timezone(&Utc).format("%Y-%m-%dT%H:%M:%SZ"),
            logical,
            self.boundary_hour,
        ))
    }
}

// ---------------------------------------------------------------------------
// Shared session sources
// ---------------------------------------------------------------------------

/// Every store the two session tools read, plus the config that maps a
/// session to a memory namespace.
///
/// Assembled in `main.rs` once the stores exist — which is after
/// `default_tool_set` runs, hence the late `register_tool` rather than a
/// constructor argument.
pub struct SessionSources {
    config: Config,
    /// `None` when no chat channel is configured, so no channel store
    /// was ever built.
    channel: Option<Arc<SessionStore>>,
    cross_device: Arc<SessionStore>,
    device_default: Arc<SessionStore>,
    /// The autonomous store (kind = `"autonomous"`). Included for the same
    /// reason the others are: the loop's sessions are ordinary sessions, and
    /// `session_list` is where a human finds out what happened overnight —
    /// nothing is posted to a chat (design decision 11).
    autonomous: Arc<SessionStore>,
    acp: Arc<AcpSessionStore>,
}

impl SessionSources {
    pub fn new(
        config: Config,
        channel: Option<Arc<SessionStore>>,
        cross_device: Arc<SessionStore>,
        device_default: Arc<SessionStore>,
        autonomous: Arc<SessionStore>,
        acp: Arc<AcpSessionStore>,
    ) -> Self {
        Self {
            config,
            channel,
            cross_device,
            device_default,
            autonomous,
            acp,
        }
    }

    /// The memory namespace a session belongs to.
    ///
    /// `meta.namespace` is pinned at creation for every store that has
    /// no room id to derive one from (`/rpc`, device-default, ACP); the
    /// room-id derivation is the fallback for chat sessions and for
    /// files written before that field existed.
    fn namespace_of(&self, meta: &SessionMeta) -> String {
        meta.namespace
            .clone()
            .unwrap_or_else(|| self.config.namespace_for_room(&meta.room_id).to_string())
    }

    /// Every row from every store, unfiltered.
    fn all_rows(&self) -> Vec<SessionRow> {
        let mut rows = Vec::new();
        if let Some(channel) = &self.channel {
            rows.extend(channel.session_rows());
        }
        rows.extend(self.cross_device.session_rows());
        rows.extend(self.device_default.session_rows());
        rows.extend(self.autonomous.session_rows());
        rows.extend(self.acp.session_rows());
        rows
    }

    /// The namespaces this turn may see: the caller's own namespace and
    /// the ones it includes. Same chain the system prompt's `# Memory`
    /// block is assembled from, so a tool cannot reach further than the
    /// prompt already does.
    fn visible_namespaces(&self) -> Vec<String> {
        let ns = crate::tools::workspace_tools::current_memory_namespace();
        self.config.resolve_namespace_chain(&ns)
    }
}

/// How a session is named in a listing: its generated title when it has
/// one, otherwise where it is happening.
fn session_label(meta: &SessionMeta) -> String {
    if let Some(title) = meta.title.as_ref().filter(|t| !t.trim().is_empty()) {
        return format!("\"{}\"", title.trim());
    }
    if !meta.room_id.is_empty() {
        return format!("{}/{}", meta.channel, meta.room_id);
    }
    format!("{} session", meta.channel)
}

fn local_hm(at: DateTime<Utc>) -> String {
    at.with_timezone(&Local).format("%m-%d %H:%M").to_string()
}

// ---------------------------------------------------------------------------
// session_list
// ---------------------------------------------------------------------------

/// What other conversations are open right now.
pub struct SessionListTool {
    sources: Arc<SessionSources>,
    spec: ToolSpec,
}

impl SessionListTool {
    pub fn new(sources: Arc<SessionSources>) -> Self {
        Self {
            sources,
            spec: ToolSpec {
                name: "session_list".into(),
                description: "List the agent's own conversation sessions — chat rooms, \
                    voice satellites, API clients and editor (ACP) sessions — that have \
                    been active recently and are not closed. \
                    Each row carries the session_id to pass to `session_read`, where the \
                    conversation itself can be read. \
                    Titles are the ones already recorded for each session; nothing is \
                    summarised here, so this is cheap to call."
                    .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "days": {
                            "type": "integer",
                            "description": "How many agent-days back to look, counting today \
                                as 1 (default: 1 — today only).",
                            "default": 1,
                            "minimum": 1,
                            "maximum": 30
                        },
                        "include_closed": {
                            "type": "boolean",
                            "description": "Also list sessions that have been closed \
                                (default: false).",
                            "default": false
                        }
                    }
                }),
            },
        }
    }
}

#[async_trait]
impl Tool for SessionListTool {
    fn kind(&self) -> ToolKind {
        ToolKind::Read
    }

    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn execute(&self, input: &serde_json::Value) -> Result<String> {
        let days = input["days"].as_u64().unwrap_or(1).clamp(1, 30);
        let include_closed = input["include_closed"].as_bool().unwrap_or(false);

        let boundary = self.sources.config.day_boundary_hour;
        let today = crate::session::local_date_for_timestamp(Local::now(), boundary);
        let first_day = today - chrono::Duration::days(days as i64 - 1);
        let (since, _) = day_window(first_day, boundary);

        let visible = self.sources.visible_namespaces();
        let mut rows: Vec<SessionRow> = self
            .sources
            .all_rows()
            .into_iter()
            .filter(|r| visible.contains(&self.sources.namespace_of(&r.meta)))
            .filter(|r| include_closed || !r.is_closed)
            .filter(|r| r.message_count > 0)
            .filter(|r| r.last_at.is_some_and(|at| at >= since))
            .collect();
        rows.sort_by_key(|r| r.last_at);

        if rows.is_empty() {
            return Ok(format!(
                "No session active since {} in namespace(s): {}.",
                first_day,
                visible.join(", ")
            ));
        }

        let mut out = format!(
            "{} session(s) active since {} in namespace(s): {}. Oldest first.\n\n",
            rows.len(),
            first_day,
            visible.join(", ")
        );
        for row in &rows {
            let ns = self.sources.namespace_of(&row.meta);
            out.push_str(&format!(
                "- {} · {} · {} · {} msg{} · id={}\n",
                row.last_at.map(local_hm).unwrap_or_else(|| "??".into()),
                row.meta.channel,
                session_label(&row.meta),
                row.message_count,
                if row.is_closed { " · closed" } else { "" },
                row.meta.session_id,
            ));
            if ns != visible[0] {
                out.push_str(&format!("    (namespace: {ns})\n"));
            }
        }
        out.push_str("\nUse session_read with one of these ids to read what was said.");
        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// session_read
// ---------------------------------------------------------------------------

/// One session's conversation, paginated by line like `file_read`.
pub struct SessionReadTool {
    sources: Arc<SessionSources>,
    spec: ToolSpec,
}

impl SessionReadTool {
    pub fn new(sources: Arc<SessionSources>) -> Self {
        Self {
            sources,
            spec: ToolSpec {
                name: "session_read".into(),
                description: "Read what was said in one of the agent's own sessions \
                    (find its session_id with `session_list`). \
                    Returns the transcript with line-based pagination, in the same \
                    'N|content' shape as `file_read`; omit `offset` to get the most \
                    recent lines. \
                    Tool results are never included — only what was said, plus a one-line \
                    marker naming each tool that was called."
                    .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "session_id": {
                            "type": "string",
                            "description": "Session id, as printed by `session_list`."
                        },
                        "offset": {
                            "type": "integer",
                            "description": "1-indexed transcript line to start at. \
                                Omit to read the end of the session — the default is the \
                                last `limit` lines.",
                            "minimum": 1
                        },
                        "limit": {
                            "type": "integer",
                            "description": "Maximum number of lines to return (default: 200, max: 2000).",
                            "default": 200,
                            "maximum": 2000
                        }
                    },
                    "required": ["session_id"]
                }),
            },
        }
    }

    /// The transcript for `session_id`, as lines, plus its listing row.
    ///
    /// Searches every store the listing draws from, so an id copied out
    /// of `session_list` always resolves.
    fn transcript(&self, session_id: &str) -> Option<(SessionRow, Vec<String>)> {
        let row = self
            .sources
            .all_rows()
            .into_iter()
            .find(|r| r.meta.session_id == session_id)?;

        let lines = if row.meta.channel == "acp" {
            acp_transcript(&self.sources.acp, session_id)?
        } else {
            let stores = [
                self.sources.channel.as_ref(),
                Some(&self.sources.cross_device),
                Some(&self.sources.device_default),
                Some(&self.sources.autonomous),
            ];
            stores
                .into_iter()
                .flatten()
                .find_map(|s| s.load_session_full(session_id))
                .map(|(messages, _)| store_transcript(&messages))?
        };
        Some((row, lines))
    }
}

/// Render `StoredMessage`s the way the model should read them back.
///
/// Tool *results* are dropped outright — that is what a session's bytes
/// are mostly made of, and pulling them back into a tool result here
/// would re-inflate the very prompt this feature exists to keep small.
/// A tool *call* stays as one short line, because "what was done" is
/// most of what makes a transcript worth reading.
fn store_transcript(messages: &[StoredMessage]) -> Vec<String> {
    let mut out = Vec::new();
    for msg in messages {
        let role = match msg.role {
            Role::User => "user",
            Role::Assistant => "assistant",
        };
        let stamp = msg.timestamp.with_timezone(&Local).format("%m-%d %H:%M");
        for part in &msg.parts {
            match part {
                ContentPart::Text(t) if !t.trim().is_empty() => {
                    push_block(&mut out, &format!("[{stamp} {role}]"), t);
                }
                ContentPart::ToolUse { name, .. } | ContentPart::ToolUseRef { name, .. } => {
                    out.push(format!("[{stamp} {role}] (tool: {name})"));
                }
                ContentPart::Image { .. } | ContentPart::ImageRef { .. } => {
                    out.push(format!("[{stamp} {role}] (image)"));
                }
                _ => {}
            }
        }
    }
    out
}

/// The ACP store's own event shape, rendered identically.
fn acp_transcript(store: &AcpSessionStore, session_id: &str) -> Option<Vec<String>> {
    let events = store.events(session_id)?;
    let mut out = Vec::new();
    for event in events {
        let stamp = event.at.with_timezone(&Local).format("%m-%d %H:%M");
        match &event.body {
            EventBody::Message { role, parts } => {
                let role = match role {
                    Role::User => "user",
                    Role::Assistant => "assistant",
                };
                for part in parts {
                    match part {
                        StoredPart::Text(t) if !t.trim().is_empty() => {
                            push_block(&mut out, &format!("[{stamp} {role}]"), t);
                        }
                        StoredPart::ToolUse { name, .. } | StoredPart::ToolUseRef { name, .. } => {
                            out.push(format!("[{stamp} {role}] (tool: {name})"));
                        }
                        StoredPart::Other => {
                            out.push(format!("[{stamp} {role}] (attachment)"));
                        }
                        _ => {}
                    }
                }
            }
            EventBody::Summary { summary, .. } => {
                push_block(&mut out, &format!("[{stamp} compaction summary]"), summary);
            }
            EventBody::Title { .. } | EventBody::Closed => {}
        }
    }
    Some(out)
}

/// Append `text` under `prefix`, one output line per input line so the
/// caller's `offset`/`limit` mean the same thing they mean in
/// `file_read`.
fn push_block(out: &mut Vec<String>, prefix: &str, text: &str) {
    let mut lines = text.trim_end().lines();
    match lines.next() {
        Some(first) => out.push(format!("{prefix} {first}")),
        None => return,
    }
    for line in lines {
        out.push(line.to_string());
    }
}

#[async_trait]
impl Tool for SessionReadTool {
    fn kind(&self) -> ToolKind {
        ToolKind::Read
    }

    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn execute(&self, input: &serde_json::Value) -> Result<String> {
        let session_id = input["session_id"]
            .as_str()
            .context("missing 'session_id'")?
            .trim();
        let limit = input["limit"].as_u64().unwrap_or(200).clamp(1, 2000) as usize;

        let (row, lines) = self
            .transcript(session_id)
            .with_context(|| format!("no session with id '{session_id}'"))?;

        // Namespace containment, checked after the lookup so the message
        // can say which namespace it belongs to. A session outside the
        // caller's chain is not this turn's to read — the same boundary
        // the system prompt's memory block draws.
        let ns = self.sources.namespace_of(&row.meta);
        let visible = self.sources.visible_namespaces();
        if !visible.contains(&ns) {
            anyhow::bail!(
                "session '{session_id}' belongs to memory namespace '{ns}', which this \
                 conversation does not read from"
            );
        }

        let total = lines.len();
        if total == 0 {
            return Ok(format!(
                "Session {session_id} ({}, {}) has nothing to read.",
                row.meta.channel,
                session_label(&row.meta)
            ));
        }
        // No `offset` means "the end": a caller asking about an active
        // session wants where it got to, not where it started.
        let start = match input["offset"].as_u64() {
            Some(o) => (o.max(1) as usize) - 1,
            None => total.saturating_sub(limit),
        };
        if start >= total {
            anyhow::bail!(
                "offset {} exceeds the transcript ({total} lines)",
                start + 1
            );
        }
        let end = (start + limit).min(total);

        let mut out = format!(
            "Session {session_id} · {} · {} · namespace {ns} · {} msg{} · {total} line(s)\n\
             Tool results are omitted.\n\n",
            row.meta.channel,
            session_label(&row.meta),
            row.message_count,
            if row.is_closed { " · closed" } else { "" },
        );
        if start > 0 {
            out.push_str(&format!(
                "[{start} earlier line(s) — use offset=1 to read from the start]\n"
            ));
        }
        for (i, line) in lines[start..end].iter().enumerate() {
            out.push_str(&format!("{}|{}\n", start + i + 1, line));
        }
        if end < total {
            out.push_str(&format!(
                "[{} more line(s) — use offset={} to continue]\n",
                total - end,
                end + 1
            ));
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::ChatMessage;
    use crate::session::ConversationKey;

    fn config() -> Config {
        Config::parse_for_test(
            r#"
[anthropic]
api_key = "test"

[memory_namespace.work]
include = []

[room_profile.work]
profile = "default"
memory_namespace = "work"
rooms = ["!work:example.org"]

[profiles.default]
provider = "anthropic"
"#,
        )
    }

    /// A `SessionSources` over one tempdir, laid out the way `main.rs`
    /// lays the real one out: every store shares `sessions/`, each in
    /// its own `kind` subtree.
    fn sources(dir: &tempfile::TempDir) -> Arc<SessionSources> {
        let base = dir.path().join("sessions");
        Arc::new(SessionSources::new(
            config(),
            Some(Arc::new(SessionStore::new(base.clone(), "channel", None))),
            Arc::new(SessionStore::new(base.clone(), "cross-device", None)),
            Arc::new(SessionStore::new(base.clone(), "device-default", None)),
            Arc::new(SessionStore::new(base.clone(), "autonomous", None)),
            Arc::new(AcpSessionStore::new(base, None)),
        ))
    }

    fn channel_store(dir: &tempfile::TempDir) -> SessionStore {
        SessionStore::new(dir.path().join("sessions"), "channel", None)
    }

    /// One chat session with `n` exchanges, titled `title`.
    fn seeded_channel_session(
        store: &SessionStore,
        room_id: &str,
        namespace: &str,
        title: &str,
        n: usize,
    ) -> String {
        let key: ConversationKey = (room_id.to_string(), None);
        let sid = store.create_session(&key, "matrix", namespace).unwrap();
        for i in 0..n {
            store
                .append(&sid, &ChatMessage::user(format!("question {i}")))
                .unwrap();
            store
                .append(&sid, &ChatMessage::assistant(format!("answer {i}")))
                .unwrap();
        }
        store.set_title(&sid, title).unwrap();
        sid
    }

    #[tokio::test]
    async fn current_time_reports_the_agents_logical_day() {
        let tool = CurrentTimeTool::new(4);
        let out = tool.execute(&json!({})).await.unwrap();
        let expected = crate::session::local_date_for_timestamp(Local::now(), 4);
        assert!(
            out.contains(&format!("Agent day: {expected}")),
            "the logical day must be the day-boundary one, not the naive date: {out}"
        );
        assert!(out.contains("day boundary 04:00 local"), "{out}");
    }

    /// The listing is built from the title line each session already
    /// carries. Nothing is summarised, so nothing needs a model call.
    #[tokio::test]
    async fn session_list_names_sessions_by_their_recorded_title() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = channel_store(&dir);
        let sid = seeded_channel_session(&store, "!a:example.org", "default", "parser hunt", 2);

        let out = SessionListTool::new(sources(&dir))
            .execute(&json!({}))
            .await
            .unwrap();
        assert!(out.contains("parser hunt"), "{out}");
        assert!(
            out.contains(&sid),
            "the id must be callable into session_read: {out}"
        );
        assert!(out.contains("4 msg"), "{out}");
    }

    /// A closed session is history, not "what is going on now" — unless
    /// the caller says otherwise.
    #[tokio::test]
    async fn session_list_hides_closed_sessions_unless_asked() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = channel_store(&dir);
        let sid = seeded_channel_session(&store, "!a:example.org", "default", "finished", 1);
        store.close_session(&sid).unwrap();

        let tool = SessionListTool::new(sources(&dir));
        let hidden = tool.execute(&json!({})).await.unwrap();
        assert!(!hidden.contains(&sid), "{hidden}");

        let shown = tool
            .execute(&json!({"include_closed": true}))
            .await
            .unwrap();
        assert!(shown.contains(&sid), "{shown}");
        assert!(shown.contains("closed"), "{shown}");
    }

    /// The listing draws the same namespace boundary the system prompt's
    /// memory block does: a room in `work` is not the default
    /// namespace's business.
    #[tokio::test]
    async fn session_list_stays_inside_the_callers_namespace_chain() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = channel_store(&dir);
        let work = seeded_channel_session(&store, "!work:example.org", "work", "work thread", 1);

        let tool = SessionListTool::new(sources(&dir));
        let from_default = tool.execute(&json!({})).await.unwrap();
        assert!(
            !from_default.contains(&work),
            "a 'work' session must not appear in a 'default' turn: {from_default}"
        );

        let from_work = crate::tools::workspace_tools::scope_memory_namespace(
            "work".to_string(),
            tool.execute(&json!({})),
        )
        .await
        .unwrap();
        assert!(from_work.contains(&work), "{from_work}");
    }

    /// The point of the whole change: a transcript must not drag tool
    /// results back into the prompt. The call keeps a one-line marker,
    /// because what was done is most of what makes a session readable.
    #[tokio::test]
    async fn session_read_omits_tool_results_but_names_the_call() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = channel_store(&dir);
        let key: ConversationKey = ("!a:example.org".to_string(), None);
        let sid = store.create_session(&key, "matrix", "default").unwrap();
        store.append(&sid, &ChatMessage::user("read it")).unwrap();
        store
            .append(
                &sid,
                &ChatMessage {
                    role: Role::Assistant,
                    parts: vec![ContentPart::ToolUse {
                        id: "t1".into(),
                        name: "file_read".into(),
                        input: json!({"path": "/etc/hosts"}),
                    }],
                    input_kind: None,
                    user_id: None,
                },
            )
            .unwrap();
        store
            .append(
                &sid,
                &ChatMessage {
                    role: Role::User,
                    parts: vec![ContentPart::ToolResult {
                        tool_use_id: "t1".into(),
                        content: "SECRET-PAYLOAD".repeat(10),
                    }],
                    input_kind: None,
                    user_id: None,
                },
            )
            .unwrap();
        store.append(&sid, &ChatMessage::assistant("done")).unwrap();

        let out = SessionReadTool::new(sources(&dir))
            .execute(&json!({"session_id": sid}))
            .await
            .unwrap();
        assert!(
            !out.contains("SECRET-PAYLOAD"),
            "a tool result must never come back through session_read: {out}"
        );
        assert!(out.contains("(tool: file_read)"), "{out}");
        assert!(out.contains("read it") && out.contains("done"), "{out}");
    }

    /// Omitting `offset` reads the end — an active session's recent
    /// state is what the caller is after — and `offset` reads from
    /// there, `file_read`-style.
    #[tokio::test]
    async fn session_read_defaults_to_the_tail_and_paginates_by_line() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = channel_store(&dir);
        let sid = seeded_channel_session(&store, "!a:example.org", "default", "long one", 20);

        let tool = SessionReadTool::new(sources(&dir));
        let tail = tool
            .execute(&json!({"session_id": sid, "limit": 4}))
            .await
            .unwrap();
        assert!(
            tail.contains("answer 19"),
            "the tail must be the default: {tail}"
        );
        assert!(!tail.contains("question 0"), "{tail}");
        assert!(tail.contains("earlier line(s)"), "{tail}");

        let head = tool
            .execute(&json!({"session_id": sid, "offset": 1, "limit": 4}))
            .await
            .unwrap();
        assert!(
            head.contains("1|"),
            "lines are numbered like file_read: {head}"
        );
        assert!(head.contains("question 0"), "{head}");
        assert!(head.contains("more line(s) — use offset=5"), "{head}");
    }

    /// Reading across the namespace boundary is refused, not silently
    /// answered — the caller asked for a specific id, so it gets a
    /// reason rather than an empty transcript.
    #[tokio::test]
    async fn session_read_refuses_a_session_outside_the_namespace_chain() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = channel_store(&dir);
        let work = seeded_channel_session(&store, "!work:example.org", "work", "work thread", 1);

        let err = SessionReadTool::new(sources(&dir))
            .execute(&json!({"session_id": work}))
            .await
            .expect_err("a 'work' session must not be readable from a 'default' turn");
        assert!(format!("{err}").contains("work"), "{err}");
    }

    /// ACP sessions are the ones this was built for; they live in a
    /// different store with a different on-disk shape, and must list and
    /// read through the same two tools.
    #[tokio::test]
    async fn an_acp_session_lists_and_reads_like_any_other() {
        let dir = tempfile::TempDir::new().unwrap();
        let acp = AcpSessionStore::new(dir.path().join("sessions"), None);
        acp.create("s-acp", "default", "/repo").unwrap();
        acp.append_message("s-acp", &ChatMessage::user("port the parser"))
            .unwrap();
        acp.append_message("s-acp", &ChatMessage::assistant("on it"))
            .unwrap();
        acp.append_title("s-acp", "parser port").unwrap();

        let listed = SessionListTool::new(sources(&dir))
            .execute(&json!({}))
            .await
            .unwrap();
        assert!(listed.contains("parser port"), "{listed}");
        assert!(listed.contains("acp"), "{listed}");

        let read = SessionReadTool::new(sources(&dir))
            .execute(&json!({"session_id": "s-acp"}))
            .await
            .unwrap();
        assert!(
            read.contains("port the parser") && read.contains("on it"),
            "{read}"
        );
    }

    /// Autonomous sessions are ordinary sessions as far as reading back
    /// goes: last night's work is listed and readable from a chat, which
    /// is the *only* way a human sees it (nothing is posted to a channel).
    #[tokio::test]
    async fn an_autonomous_session_is_listed_and_readable() {
        let d = tempfile::tempdir().unwrap();
        let sources = sources(&d);
        let sid = sources
            .autonomous
            .create_autonomous_session("journal", "default")
            .unwrap();
        sources
            .autonomous
            .append(
                &sid,
                &ChatMessage::user("[Autonomous: journal]\n\nSummarise the day."),
            )
            .unwrap();
        sources
            .autonomous
            .append(&sid, &ChatMessage::assistant("Done in three lines."))
            .unwrap();

        let listed = SessionListTool::new(Arc::clone(&sources))
            .execute(&json!({}))
            .await
            .unwrap();
        assert!(listed.contains(&sid), "listing was {listed}");
        assert!(listed.contains("server/journal"), "listing was {listed}");

        let read = SessionReadTool::new(sources)
            .execute(&json!({"session_id": sid}))
            .await
            .unwrap();
        assert!(read.contains("Done in three lines."), "read was {read}");
    }
}
