//! Autonomous sessions: the third loop.
//!
//! `heartbeat.rs` runs two loops, both on a clock. This one is not: it
//! wakes, asks whether anything else is happening, and if not picks the
//! highest-priority due task out of `<workspace>/autonomous/*.md` and
//! runs it as an ordinary session, until that task's `max_turns` is spent
//! or the model says it is done.
//!
//! It is a *poll*, not a cadence. Every cycle re-reads the task
//! directory, so editing a task file takes effect within `poll_seconds`
//! and without a restart. A cycle that finds nothing costs one directory
//! read and one meta-line scan per store.
//!
//! Reusing `run_llm_turn` is the whole trick: an autonomous turn is a
//! normal turn, so the system prompt, the tools, the persistence and the
//! compression all behave as they do for any other session. The only new
//! thing is which store it lands in and which row of the permission table
//! judges it.
//!
//! Nothing here is persisted except the status file: which session a task
//! is in, when it last ran, and how many turns it has spent are all
//! derived from the session store (design decisions 3 and 5), because a
//! second copy of that state is a second thing to keep true.

use crate::autonomous_config::{AutonomousTask, load_autonomous_dir};
use crate::config::{AutonomousOrigin, DEFAULT_NAMESPACE_NAME};
use crate::provider::ChatMessage;
use crate::serve::{AutonomousHost, ServeState};
use crate::session::{SessionRow, SessionStore};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing::{info, warn};

/// The whole instruction for every turn after the first.
///
/// The body is deliberately not repeated — it is already in the session's
/// history, and re-sending it every turn invalidates the provider's
/// prompt cache for nothing.
///
/// The last line is a protocol rather than politeness: the loop closes the
/// session the moment a turn's final text is exactly `DONE`, so a task
/// that finishes early does not spend its remaining turns announcing it.
pub const CONTINUE_PROMPT: &str =
    "Continue the task. When it is finished, reply with exactly:\nDONE";

/// The first line of every user message this loop sends.
///
/// Not a search key — the session's `room_id` is (design decision 3) —
/// but the model has to be able to tell a system-fired start from the user
/// speaking.
pub fn marker(task: &str) -> String {
    format!("[Autonomous: {task}]")
}

/// `<workspace>/state/autonomous.json`, contents only.
///
/// `status` is `"idle"` or `"running"`, `reason` is the one-line human
/// answer to "why", and `updated_at` is how a human tells a live loop from
/// a stopped one. Nothing machine-reads this file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutonomousState {
    pub status: String,
    pub reason: String,
    pub updated_at: DateTime<Utc>,
}

/// Write the status file. Best-effort: a cycle that cannot write its own
/// status has still done (or not done) its work, and failing the cycle
/// over a bookkeeping file would be the tail wagging the dog.
fn write_state(workspace_dir: &Path, status: &str, reason: String) {
    let dir = workspace_dir.join("state");
    if let Err(e) = std::fs::create_dir_all(&dir) {
        warn!("autonomous: cannot create {}: {e}", dir.display());
        return;
    }
    let state = AutonomousState {
        status: status.to_string(),
        reason,
        updated_at: Utc::now(),
    };
    let path = dir.join("autonomous.json");
    let tmp = dir.join(".autonomous.json.tmp");
    let Ok(raw) = serde_json::to_string_pretty(&state) else {
        return;
    };
    // tmp + rename: a reader that catches the loop mid-write must never
    // see half a JSON document. Not routed through the framework
    // WorkspaceState — this is not a workspace document, and it is not
    // indexed.
    if std::fs::write(&tmp, raw)
        .and_then(|_| std::fs::rename(&tmp, &path))
        .is_err()
    {
        warn!("autonomous: cannot write {}", path.display());
    }
}

/// Idle iff the newest activity in the *other* stores is at least `idle`
/// old. Rows with no messages do not count as activity — an empty session
/// is not somebody talking (design decision 9).
pub fn is_idle(rows: &[SessionRow], now: DateTime<Utc>, idle: Duration) -> bool {
    match rows.iter().filter_map(|r| r.last_at).max() {
        Some(last) => now - last >= idle,
        None => true,
    }
}

/// The first enabled, due task. The order is the loader's (`priority`,
/// then `name`); this only filters, so exactly one place decides what
/// "first" means.
pub fn next_task<F>(tasks: &[AutonomousTask], mut due: F) -> Option<&AutonomousTask>
where
    F: FnMut(&AutonomousTask) -> bool,
{
    tasks.iter().find(|t| t.enabled && due(t))
}

/// May `task` start now?
///
/// The anchor is the task's *latest* session activity, closed or not. A
/// closed session is the common case — `max_turns` closes one — and
/// exactly the case a cooldown has to remember, because the file is still
/// there to be found (design decision 5).
///
/// A session whose `room_id` is `task.name` is the only thing counted,
/// which is also why a test run must **not** reuse that name: the
/// `task_test` tool creates its sessions under `test:<kind>:<name>`.
fn is_due(
    task: &AutonomousTask,
    rows: &[SessionRow],
    not_before: &HashMap<String, DateTime<Utc>>,
    now: DateTime<Utc>,
) -> bool {
    if let Some(at) = not_before.get(&task.name)
        && *at > now
    {
        return false;
    }
    let anchor = rows
        .iter()
        .filter(|r| r.meta.room_id == task.name)
        .map(|r| r.last_at.unwrap_or(r.meta.created_at))
        .max();
    match anchor {
        Some(a) => now - a >= Duration::days(task.cooldown_days as i64),
        None => true,
    }
}

// There is no resume: every run starts a fresh session and runs it to the
// end of the cycle (design decisions 5 and 13).

/// The idle loop.
pub struct AutonomousLoop {
    state: Arc<ServeState>,
    /// `None` when this deployment has no chat channel configured — in
    /// which case there is no channel traffic to be idle *from*. Held here
    /// rather than on `ServeState` because only this loop wants it, and
    /// `ServeState` is built before the channel store exists.
    channel_session_store: Option<Arc<SessionStore>>,
    workspace_dir: PathBuf,
    idle: Duration,
    poll: Duration,
    /// In-process back-off: task name → the instant it may run again.
    ///
    /// The store's anchor is what survives a restart; this is what keeps
    /// `cooldown_days = 0` from meaning "a new session every cycle" within
    /// one run (design decision 12).
    not_before: HashMap<String, DateTime<Utc>>,
    /// Which host runs autonomous sessions once the workspace is synced:
    /// the primary device only (`placement`).
    placement: Arc<crate::placement::Placement>,
}

impl AutonomousLoop {
    pub fn new(state: Arc<ServeState>, channel_session_store: Option<Arc<SessionStore>>) -> Self {
        let workspace_dir = state.workspace.dir().to_path_buf();
        // Read the two knobs before the `state` move below: `cfg` is a
        // borrow of the same `Arc` that is about to be stored.
        let idle = Duration::minutes(state.config.autonomous.idle_minutes as i64);
        let poll = Duration::seconds(state.config.autonomous.poll_seconds.max(1) as i64);
        Self {
            state,
            channel_session_store,
            workspace_dir,
            idle,
            poll,
            not_before: HashMap::new(),
            placement: crate::placement::Placement::alone(),
        }
    }

    /// Run only where `placement` says background work runs.
    pub fn with_placement(mut self, placement: Arc<crate::placement::Placement>) -> Self {
        self.placement = placement;
        self
    }

    /// Which row of the permission table this loop's turns are judged by.
    fn origin(&self) -> crate::tools::policy::Origin {
        match self.state.config.autonomous.origin {
            AutonomousOrigin::Channel => crate::tools::policy::Origin::Channel,
            AutonomousOrigin::Trusted => crate::tools::policy::Origin::Trusted,
        }
    }

    /// Every session this loop must treat as "somebody might be talking" —
    /// *except* its own store's. An autonomous session is not activity
    /// that should keep autonomous sessions from starting.
    fn busy_rows(&self) -> Vec<SessionRow> {
        let mut rows = Vec::new();
        if let Some(channel) = &self.channel_session_store {
            rows.extend(channel.session_rows());
        }
        rows.extend(self.state.cross_device_session_store.session_rows());
        rows.extend(self.state.device_default_session_store.session_rows());
        rows.extend(self.state.acp_session_store.session_rows());
        rows
    }

    pub fn spawn(self) {
        tokio::spawn(self.run());
    }

    pub async fn run(mut self) {
        // `interval` panics on a zero period; `new` already floors it at a
        // second, and the first tick is discarded so startup does not
        // immediately start working (same as both heartbeat loops).
        // `poll` is a `chrono::Duration` (the back-off arithmetic wants
        // one); tokio wants a `std::time::Duration`. `new` floors it at a
        // second, so `to_std` cannot fail in practice — the fallback keeps
        // the loop alive rather than panicking in a task if it somehow did.
        let period = self
            .poll
            .to_std()
            .unwrap_or(std::time::Duration::from_secs(1));
        let mut tick = tokio::time::interval(period);
        tick.tick().await;
        loop {
            tick.tick().await;
            // Before the cycle, not inside it: an idle cycle still writes
            // `state/autonomous.json`, which every host would otherwise
            // keep rewriting in the synced workspace.
            if !self.placement.runs_background().await {
                continue;
            }
            if let Err(e) = self.run_cycle().await {
                warn!("autonomous cycle failed: {e:#}");
            }
        }
    }

    /// One pass: idle? task? run it. Separated from `run` so a test can
    /// drive a cycle with no clock in the way.
    pub async fn run_cycle(&mut self) -> anyhow::Result<()> {
        let now = Utc::now();
        let tasks = load_autonomous_dir(&self.workspace_dir.join("autonomous"));
        if tasks.iter().all(|t| !t.enabled) {
            write_state(&self.workspace_dir, "idle", "no tasks".to_string());
            return Ok(());
        }

        if !is_idle(&self.busy_rows(), now, self.idle) {
            write_state(
                &self.workspace_dir,
                "idle",
                "busy: another session is active".to_string(),
            );
            return Ok(());
        }

        let store = Arc::clone(&self.state.autonomous_session_store);
        let rows = store.session_rows();
        let picked = next_task(&tasks, |t| is_due(t, &rows, &self.not_before, now)).cloned();
        let Some(task) = picked else {
            write_state(&self.workspace_dir, "idle", "nothing due".to_string());
            return Ok(());
        };

        self.run_task(&task, &store).await
    }

    /// The session-level unit of work: create a fresh session for the
    /// task, then turn until done or capped. There is no resume — every
    /// run starts a new session and closes it at the end (design
    /// decisions 5 and 13).
    async fn run_task(
        &mut self,
        task: &AutonomousTask,
        store: &Arc<SessionStore>,
    ) -> anyhow::Result<()> {
        let namespace = DEFAULT_NAMESPACE_NAME;
        let session_id = store.create_autonomous_session(&task.name, namespace)?;
        let mut turns = 0;

        // The session's own path, workspace-relative: the second line of
        // the first message, so a task that keeps a journal can name the
        // file it is writing (design decision 11). Derived from the store,
        // never from a template, so it cannot drift from the real name.
        let session_rel = store
            .absolute_path_for(&session_id)
            .and_then(|p| {
                p.strip_prefix(&self.workspace_dir)
                    .ok()
                    .map(Path::to_path_buf)
            })
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| format!("sessions/{namespace}/autonomous/{session_id}"));

        loop {
            let text = if turns == 0 {
                format!(
                    "{}\nSession: {session_rel}\n\n{}",
                    marker(&task.name),
                    task.body
                )
            } else {
                format!("{}\n{CONTINUE_PROMPT}", marker(&task.name))
            };
            write_state(
                &self.workspace_dir,
                "running",
                format!("task: {}, turn {}/{}", task.name, turns + 1, task.max_turns),
            );

            let outcome = crate::serve::run_llm_turn(
                Arc::clone(&self.state),
                session_id.clone(),
                ChatMessage::user(text),
                Arc::new(AutonomousHost {
                    origin: self.origin(),
                }),
                None,
            )
            .await;

            turns += 1;
            let said_done = outcome.text.as_deref().is_some_and(|t| t.trim() == "DONE");
            // `text == None` means the provider failed or the tool-round
            // budget ran out. Either way the turn produced no answer, so
            // the session is spent; closing it makes the next cycle start
            // clean instead of appending to a conversation that already
            // went wrong.
            let spent = outcome.text.is_none();
            let capped = turns >= task.max_turns;

            // Every run runs the session to the end of the cycle: done or
            // capped closes it; there is no paused state to fall back to
            // (design decision 13).
            if said_done || spent || capped {
                if let Err(e) = store.close_session(&session_id) {
                    warn!("autonomous: cannot close {}: {e}", task.name);
                }
                // Hold the task off even at `cooldown_days = 0`: the store
                // anchor clears instantly when the cooldown is zero, and
                // "as soon as the previous session ended" must not mean
                // "again in this same cycle".
                let backoff = Duration::days(task.cooldown_days as i64).max(self.poll);
                self.not_before
                    .insert(task.name.clone(), Utc::now() + backoff);
                info!(
                    "autonomous: {} finished after {turns} turn(s) (done={said_done}, capped={capped})",
                    task.name
                );
                write_state(
                    &self.workspace_dir,
                    "idle",
                    format!("task: {} finished after {turns} turn(s)", task.name),
                );
                return Ok(());
            }
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::ChatResponse;

    fn response(text: &str) -> ChatResponse {
        ChatResponse {
            prompt_usage: None,
            text: Some(text.to_string()),
            tool_calls: Vec::new(),
            stop_reason: None,
        }
    }

    fn row(room_id: &str, last_at: Option<DateTime<Utc>>, is_closed: bool) -> SessionRow {
        SessionRow {
            meta: crate::session::SessionMeta {
                session_id: "01920f00-0000-7000-8000-000000000000".to_string(),
                room_id: room_id.to_string(),
                thread_id: None,
                channel: "server".to_string(),
                created_at: Utc::now() - Duration::hours(5),
                public_id: None,
                namespace: Some("default".to_string()),
                device_id: None,
                room_profile: None,
                title: None,
            },
            message_count: 2,
            last_at,
            is_closed,
        }
    }

    fn task(name: &str) -> AutonomousTask {
        AutonomousTask {
            name: name.to_string(),
            enabled: true,
            priority: 100,
            cooldown_days: 0,
            max_turns: 1,
            body: "Do the thing.\n".to_string(),
        }
    }

    /// The whole idle rule: the newest activity in any *other* store.
    #[test]
    fn idle_is_true_only_when_nothing_else_moved_recently() {
        let now = Utc::now();
        let idle = Duration::minutes(30);

        assert!(is_idle(&[], now, idle), "no sessions at all is idle");
        assert!(
            is_idle(
                &[row("chat", Some(now - Duration::minutes(31)), false)],
                now,
                idle
            ),
            "older than idle_minutes is idle"
        );
        assert!(
            !is_idle(
                &[row("chat", Some(now - Duration::minutes(29)), false)],
                now,
                idle
            ),
            "inside idle_minutes is busy"
        );
        assert!(
            !is_idle(
                &[
                    row("old", Some(now - Duration::hours(4)), true),
                    row("new", Some(now - Duration::seconds(5)), false),
                ],
                now,
                idle
            ),
            "the max over every row decides, not the first one"
        );
        assert!(
            is_idle(&[row("never-written", None, false)], now, idle),
            "a session with no messages yet does not count as activity"
        );
    }

    #[test]
    fn only_the_first_due_enabled_task_is_picked() {
        let mut off = task("off");
        off.enabled = false;
        let mut later = task("later");
        later.cooldown_days = 7;
        // The loader's order is the priority order; `next_task` must not
        // reorder, only filter.
        let tasks = vec![off, later, task("due")];

        let picked = next_task(&tasks, |t| t.name == "due").expect("the due task");
        assert_eq!(picked.name, "due");

        assert!(next_task(&tasks, |_| false).is_none());
    }

    /// A task that just ran is not due; the same task with
    /// `cooldown_days = 0` is. Read through the real store, so the anchor
    /// rule (closed sessions included) is what is exercised.
    #[test]
    fn the_store_anchor_is_what_makes_a_task_due() {
        let state = crate::serve::ServeState::for_test(false);
        let store = &state.autonomous_session_store;
        let sid = store
            .create_autonomous_session("refactor", "default")
            .unwrap();
        store.append(&sid, &ChatMessage::user("hello")).unwrap();

        let rows = store.session_rows();
        let now = Utc::now();

        let mut fresh = task("refactor");
        fresh.cooldown_days = 0;
        assert!(is_due(&fresh, &rows, &HashMap::new(), now));

        let mut cooled = task("refactor");
        cooled.cooldown_days = 7;
        assert!(!is_due(&cooled, &rows, &HashMap::new(), now));

        // And the in-process back-off wins over the store even at
        // `cooldown_days = 0`.
        let mut backoff = HashMap::new();
        backoff.insert("refactor".to_string(), now + Duration::minutes(10));
        assert!(!is_due(&fresh, &rows, &backoff, now));
    }

    /// The end-to-end shape of one cycle: a session is created in the
    /// autonomous store, the task's body is the first user message with
    /// the marker and the session path in front of it, and `max_turns`
    /// closes it.
    #[tokio::test]
    async fn one_cycle_runs_the_task_and_closes_the_session() {
        let state = crate::serve::ServeState::for_test_scripted(false, vec![response("ok")]);
        let ws = state.workspace.dir().to_path_buf();
        std::fs::create_dir_all(ws.join("autonomous")).unwrap();
        std::fs::write(
            ws.join("autonomous/journal.md"),
            "---\nmax_turns: 1\n---\nSummarise the day.\n",
        )
        .unwrap();

        let mut lp = AutonomousLoop::new(Arc::clone(&state), None);
        lp.run_cycle().await.unwrap();

        let rows = state.autonomous_session_store.session_rows();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].meta.room_id, "journal");
        assert_eq!(rows[0].meta.channel, "server");
        assert!(rows[0].is_closed, "max_turns = 1 must close the session");

        let sid = rows[0].meta.session_id.clone();
        let history = state.autonomous_session_store.load_session(&sid).unwrap();
        let first = match &history[0].parts[0] {
            crate::provider::ContentPart::Text(t) => t.clone(),
            other => panic!("expected text, got {other:?}"),
        };
        assert!(first.starts_with("[Autonomous: journal]\n"));
        // The second line is the session's own path, workspace-relative and
        // dated — derived from the store, so this asserts exactly what
        // `absolute_path_for` resolves to, prefix and file name together.
        let path_line = store_path(&state, &sid);
        assert!(
            first.contains(&format!("Session: {path_line}")),
            "first: {first:?}"
        );
        assert!(
            path_line.starts_with("sessions/default/autonomous/")
                && path_line.ends_with(&format!("-{sid}")),
            "path should be dated and workspace-relative: {path_line}"
        );
        assert!(first.ends_with("Summarise the day.\n"));

        // The status file says what happened, and names the task.
        let raw = std::fs::read_to_string(ws.join("state/autonomous.json")).unwrap();
        let st: AutonomousState = serde_json::from_str(&raw).unwrap();
        assert!(st.reason.contains("journal"), "reason was {:?}", st.reason);
        assert!(st.status == "idle" || st.status == "running");
    }

    /// The workspace-relative path the loop should name in its first
    /// message — computed from the store, not from a format template.
    fn store_path(state: &crate::serve::ServeState, sid: &str) -> String {
        state
            .autonomous_session_store
            .absolute_path_for(sid)
            .and_then(|p| {
                p.strip_prefix(state.workspace.dir())
                    .ok()
                    .map(std::path::PathBuf::from)
            })
            .map(|p| p.display().to_string())
            .unwrap()
    }

    /// The protocol the loop exists for, pinned: the second message of a
    /// task that is not finished is the marker plus `CONTINUE_PROMPT`, and
    /// a turn that replies exactly `DONE` closes the session *before* its
    /// `max_turns` cap is reached.
    #[tokio::test]
    async fn a_done_reply_closes_early_and_the_second_turn_continues() {
        let state = crate::serve::ServeState::for_test_scripted(
            false,
            vec![response("working"), response("DONE")],
        );
        let ws = state.workspace.dir().to_path_buf();
        std::fs::create_dir_all(ws.join("autonomous")).unwrap();
        std::fs::write(
            ws.join("autonomous/journal.md"),
            "---\nmax_turns: 3\n---\nKeep going.\n",
        )
        .unwrap();

        let mut lp = AutonomousLoop::new(Arc::clone(&state), None);
        lp.run_cycle().await.unwrap();

        let rows = state.autonomous_session_store.session_rows();
        assert_eq!(rows.len(), 1);
        assert!(
            rows[0].is_closed,
            "a DONE reply must close the session before the cap"
        );
        let sid = rows[0].meta.session_id.clone();
        let history = state.autonomous_session_store.load_session(&sid).unwrap();
        assert_eq!(history.len(), 4, "two turns: user + assistant each");
        let second = match &history[2].parts[0] {
            crate::provider::ContentPart::Text(t) => t.clone(),
            other => panic!("expected text, got {other:?}"),
        };
        assert_eq!(second, format!("[Autonomous: journal]\n{CONTINUE_PROMPT}"));
    }

    /// A cycle for a task inside its cooldown: nothing is created, and the
    /// status file says why nothing happened.
    #[tokio::test]
    async fn a_cooled_down_task_starts_nothing() {
        let state = crate::serve::ServeState::for_test(false);
        let ws = state.workspace.dir().to_path_buf();
        std::fs::create_dir_all(ws.join("autonomous")).unwrap();
        std::fs::write(
            ws.join("autonomous/refactor.md"),
            "---\ncooldown_days: 7\n---\nRefactor something.\n",
        )
        .unwrap();
        let sid = state
            .autonomous_session_store
            .create_autonomous_session("refactor", "default")
            .unwrap();
        state
            .autonomous_session_store
            .append(&sid, &ChatMessage::user("earlier run"))
            .unwrap();

        let mut lp = AutonomousLoop::new(Arc::clone(&state), None);
        lp.run_cycle().await.unwrap();

        assert_eq!(state.autonomous_session_store.session_rows().len(), 1);
        let raw = std::fs::read_to_string(ws.join("state/autonomous.json")).unwrap();
        let st: AutonomousState = serde_json::from_str(&raw).unwrap();
        assert_eq!(st.status, "idle");
        assert!(
            st.reason.contains("nothing due"),
            "reason was {:?}",
            st.reason
        );
    }

    /// A busy store means the cycle does not start a thing — and says so.
    #[tokio::test]
    async fn a_busy_agent_starts_nothing() {
        let state = crate::serve::ServeState::for_test(false);
        let ws = state.workspace.dir().to_path_buf();
        std::fs::create_dir_all(ws.join("autonomous")).unwrap();
        std::fs::write(ws.join("autonomous/journal.md"), "---\n---\nWork.\n").unwrap();

        // A freshly-touched cross-device session, i.e. somebody is talking.
        let chat = state
            .cross_device_session_store
            .create_session(&("room".to_string(), None), "rpc", "default")
            .unwrap();
        state
            .cross_device_session_store
            .append(&chat, &ChatMessage::user("hello"))
            .unwrap();

        let mut lp = AutonomousLoop::new(Arc::clone(&state), None);
        lp.run_cycle().await.unwrap();

        assert!(state.autonomous_session_store.session_rows().is_empty());
        let raw = std::fs::read_to_string(ws.join("state/autonomous.json")).unwrap();
        let st: AutonomousState = serde_json::from_str(&raw).unwrap();
        assert_eq!(st.status, "idle");
        assert!(st.reason.contains("busy"), "reason was {:?}", st.reason);
    }
}
