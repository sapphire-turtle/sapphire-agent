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
//!
//! A task that is only waiting on background tasks replies `WAIT` and is
//! *suspended*: its session stays open and the loop moves on to other
//! tasks. "Suspended" has no state file either — it is an open session of
//! the task whose background registry still holds something
//! (`has_pending_or_inbox`). Once a completion lands in its inbox, the
//! next cycle resumes that same session ahead of any new start, and the
//! completion is delivered by `run_llm_turn` like any other. This revises
//! design decision 13, which had no paused state.

use crate::autonomous_config::{AutonomousTask, load_autonomous_dir};
use crate::background::BackgroundTasks;
use crate::config::{AutonomousOrigin, DEFAULT_NAMESPACE_NAME};
use crate::provider::{ChatMessage, ContentPart, Role};
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
/// The last lines are a protocol rather than politeness: the loop closes
/// the session the moment a turn's final text is exactly `DONE`, so a task
/// that finishes early does not spend its remaining turns announcing it;
/// and it suspends the session on exactly `WAIT` while background tasks
/// are outstanding, so waiting does not spend turns either.
pub const CONTINUE_PROMPT: &str = "Continue the task. When it is finished, reply with exactly:\nDONE\nIf you are only waiting on background tasks, reply with exactly:\nWAIT";

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

/// The task's suspended session, if it has one: its newest open session
/// that still has background tasks pending or undelivered.
///
/// An open session with nothing outstanding is not suspended — that is a
/// run cut short (a crash mid-turn), and the next run starts afresh as it
/// always has. Suspension revises design decision 13 (no paused state).
fn suspended_session(
    task: &AutonomousTask,
    rows: &[SessionRow],
    bg: &BackgroundTasks,
) -> Option<String> {
    rows.iter()
        .filter(|r| r.meta.room_id == task.name && !r.is_closed)
        .filter(|r| bg.has_pending_or_inbox(&r.meta.session_id))
        .max_by_key(|r| r.last_at.unwrap_or(r.meta.created_at))
        .map(|r| r.meta.session_id.clone())
}

/// How many turns `session_id` has taken: the user messages this loop
/// sent, told apart by their marker. Counted from the history so a resume
/// (in this process or after a restart) picks up where the run left off.
/// Any text part counts, because delivered completions are placed in
/// front of the marker.
fn turns_spent(store: &SessionStore, session_id: &str, task: &str) -> usize {
    let marker = marker(task);
    // The raw log, not `load_session`: that one is the model's view, which
    // a compaction checkpoint trims, and turns before it still count.
    store
        .load_session_full(session_id)
        .map(|(messages, _)| messages)
        .unwrap_or_default()
        .iter()
        .filter(|m| m.role == Role::User)
        .filter(|m| {
            m.parts
                .iter()
                .any(|p| matches!(p, ContentPart::Text(t) if t.starts_with(&marker)))
        })
        .count()
}

/// See `AutonomousLoop::on_session_created`.
#[cfg(test)]
type SessionCreatedHook = Box<dyn Fn(&str, &str) + Send>;

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
    /// Session id → turns this process has run in it.
    ///
    /// A floor under `turns_spent`, which counts only turns whose message
    /// reached the store: when the store cannot be written, the count on
    /// disk never advances, and a session the model keeps suspending with
    /// `WAIT` would otherwise be resumed (and spend a turn) every cycle
    /// without limit. Dropped when the session closes.
    turns_run: HashMap<String, usize>,
    /// Which host runs autonomous sessions once the workspace is synced:
    /// the primary device only (`placement`).
    placement: Arc<crate::placement::Placement>,
    /// Called with `(task, session_id)` right after a fresh session is
    /// created, so a test can register a background task for a session
    /// whose id it cannot know in advance (the scripted provider calls no
    /// tool that would).
    #[cfg(test)]
    on_session_created: Option<SessionCreatedHook>,
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
            turns_run: HashMap::new(),
            placement: crate::placement::Placement::alone(),
            #[cfg(test)]
            on_session_created: None,
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
        let bg = &self.state.background;

        // A suspended task whose completion has arrived goes first, in
        // priority order. Its cooldown and back-off do not apply: it is
        // finishing a run, not starting one.
        let resume = tasks.iter().filter(|t| t.enabled).find_map(|t| {
            suspended_session(t, &rows, bg)
                .filter(|sid| bg.has_inbox(sid))
                .map(|sid| (t.clone(), sid))
        });
        if let Some((task, sid)) = resume {
            return self.run_task(&task, &store, Some(sid)).await;
        }

        // Otherwise a new start, skipping tasks that are suspended and
        // still waiting.
        let picked = next_task(&tasks, |t| {
            suspended_session(t, &rows, bg).is_none() && is_due(t, &rows, &self.not_before, now)
        })
        .cloned();
        let Some(task) = picked else {
            write_state(&self.workspace_dir, "idle", "nothing due".to_string());
            return Ok(());
        };

        self.run_task(&task, &store, None).await
    }

    /// The session-level unit of work: create a fresh session for the
    /// task (or resume the suspended one in `resume`), then turn until
    /// done, capped, or suspended on `WAIT`.
    async fn run_task(
        &mut self,
        task: &AutonomousTask,
        store: &Arc<SessionStore>,
        resume: Option<String>,
    ) -> anyhow::Result<()> {
        let namespace = DEFAULT_NAMESPACE_NAME;
        // Only a fresh session gets the body; a resumed one already has it.
        let mut send_body = resume.is_none();
        let (session_id, mut turns) = match resume {
            Some(sid) => {
                let run = self.turns_run.get(&sid).copied().unwrap_or(0);
                let spent = turns_spent(store, &sid, &task.name).max(run);
                (sid, spent)
            }
            None => {
                let sid = store.create_autonomous_session(&task.name, namespace)?;
                #[cfg(test)]
                if let Some(hook) = &self.on_session_created {
                    hook(&task.name, &sid);
                }
                (sid, 0)
            }
        };

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
            .map(|p| crate::workspace::slash_path(&p))
            .unwrap_or_else(|| format!("sessions/{namespace}/autonomous/{session_id}"));

        loop {
            let text = if std::mem::take(&mut send_body) {
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
            self.turns_run.insert(session_id.clone(), turns);
            let said_done = outcome.text.as_deref().is_some_and(|t| t.trim() == "DONE");
            let said_wait = outcome.text.as_deref().is_some_and(|t| t.trim() == "WAIT");
            // `text == None` means the provider failed or the tool-round
            // budget ran out. Either way the turn produced no answer, so
            // the session is spent; closing it makes the next cycle start
            // clean instead of appending to a conversation that already
            // went wrong.
            let spent = outcome.text.is_none();
            let capped = turns >= task.max_turns;

            // Done, spent or capped closes the session, and stops whatever
            // it still had running in the background: nothing is left to
            // deliver those results to.
            if said_done || spent || capped {
                self.turns_run.remove(&session_id);
                let _ = self.state.background.cancel_all(&session_id);
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

            // Suspend: leave the session open and set no back-off, so the
            // first cycle after a completion arrives resumes it. A `WAIT`
            // with nothing to wait on is an ordinary continue.
            if said_wait && self.state.background.has_pending_or_inbox(&session_id) {
                info!(
                    "autonomous: {} suspended after {turns} turn(s), waiting on background tasks",
                    task.name
                );
                write_state(
                    &self.workspace_dir,
                    "idle",
                    format!("task: {} waiting on background tasks", task.name),
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
            .map(|p| crate::workspace::slash_path(&p))
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

    // ---- Suspend on `WAIT`, resume on a completion ----

    /// Write `autonomous/<name>.md` with the given front matter lines.
    fn write_task(state: &crate::serve::ServeState, name: &str, front: &str) {
        let dir = state.workspace.dir().join("autonomous");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(format!("{name}.md")),
            format!("---\n{front}\n---\nWork.\n"),
        )
        .unwrap();
    }

    /// `(session_id, task_id)` of the task `loop_registering` registered.
    type Seen = Arc<std::sync::Mutex<Option<(String, String)>>>;

    /// A loop whose new sessions for `task` get one pending background
    /// task registered before their first turn — standing in for a tool
    /// that would register it during the turn. The returned cell holds
    /// `(session_id, task_id)` once it fired.
    fn loop_registering(
        state: &Arc<crate::serve::ServeState>,
        task: &'static str,
    ) -> (AutonomousLoop, Seen) {
        let seen = Arc::new(std::sync::Mutex::new(None));
        let mut lp = AutonomousLoop::new(Arc::clone(state), None);
        let bg = Arc::clone(&state.background);
        let cell = Arc::clone(&seen);
        lp.on_session_created = Some(Box::new(move |name, sid| {
            if name != task {
                return;
            }
            let id = bg
                .register(
                    sid,
                    crate::background::TaskKind::Subagent {
                        agent: "worker".to_string(),
                        handle: "h".to_string(),
                    },
                    "worker".to_string(),
                )
                .unwrap();
            *cell.lock().unwrap() = Some((sid.to_string(), id));
        }));
        (lp, seen)
    }

    fn rows_for(state: &crate::serve::ServeState, name: &str) -> Vec<SessionRow> {
        state
            .autonomous_session_store
            .session_rows()
            .into_iter()
            .filter(|r| r.meta.room_id == name)
            .collect()
    }

    fn texts(msg: &ChatMessage) -> Vec<String> {
        msg.parts
            .iter()
            .filter_map(|p| match p {
                crate::provider::ContentPart::Text(t) => Some(t.clone()),
                _ => None,
            })
            .collect()
    }

    fn finished() -> crate::background::Outcome {
        crate::background::Outcome::Finished {
            text: "all done".to_string(),
            refused: Vec::new(),
        }
    }

    #[tokio::test]
    async fn wait_with_a_pending_task_suspends_without_closing() {
        let state = crate::serve::ServeState::for_test_scripted(false, vec![response("WAIT")]);
        write_task(&state, "a", "max_turns: 3");
        let (mut lp, seen) = loop_registering(&state, "a");
        lp.run_cycle().await.unwrap();

        let rows = rows_for(&state, "a");
        assert_eq!(rows.len(), 1);
        assert!(
            !rows[0].is_closed,
            "WAIT with a pending task must not close"
        );
        let (sid, _) = seen.lock().unwrap().clone().unwrap();
        assert_eq!(rows[0].meta.session_id, sid);
        assert_eq!(
            state
                .autonomous_session_store
                .load_session(&sid)
                .unwrap()
                .len(),
            2,
            "one turn, then suspended"
        );
        assert!(state.background.has_pending_or_inbox(&sid));
        let raw =
            std::fs::read_to_string(state.workspace.dir().join("state/autonomous.json")).unwrap();
        let st: AutonomousState = serde_json::from_str(&raw).unwrap();
        assert_eq!(st.status, "idle");
        assert!(st.reason.contains("waiting"), "reason was {:?}", st.reason);
    }

    #[tokio::test]
    async fn a_suspended_task_is_skipped_while_its_inbox_is_empty_and_another_runs() {
        let state = crate::serve::ServeState::for_test_scripted(
            false,
            vec![response("WAIT"), response("ok")],
        );
        write_task(&state, "a", "priority: 1\nmax_turns: 3");
        write_task(&state, "b", "priority: 2\nmax_turns: 1");
        let (mut lp, _) = loop_registering(&state, "a");
        lp.run_cycle().await.unwrap();
        assert!(rows_for(&state, "b").is_empty(), "a outranks b");

        lp.run_cycle().await.unwrap();
        let a = rows_for(&state, "a");
        assert_eq!(a.len(), 1, "no second session for the suspended task");
        assert!(!a[0].is_closed);
        assert_eq!(
            state
                .autonomous_session_store
                .load_session(&a[0].meta.session_id)
                .unwrap()
                .len(),
            2,
            "a suspended task with an empty inbox takes no turn"
        );
        let b = rows_for(&state, "b");
        assert_eq!(b.len(), 1, "the other task runs while a waits");
        assert!(b[0].is_closed);
    }

    #[tokio::test]
    async fn a_completion_resumes_the_same_session() {
        let state = crate::serve::ServeState::for_test_scripted(
            false,
            vec![response("WAIT"), response("DONE")],
        );
        write_task(&state, "a", "max_turns: 5");
        let (mut lp, seen) = loop_registering(&state, "a");
        lp.run_cycle().await.unwrap();
        let (sid, id) = seen.lock().unwrap().clone().unwrap();
        assert!(state.background.complete(&sid, &id, finished()));

        lp.run_cycle().await.unwrap();
        let rows = rows_for(&state, "a");
        assert_eq!(rows.len(), 1, "resumed, not started afresh");
        assert!(rows[0].is_closed, "the resumed turn said DONE");
        let history = state.autonomous_session_store.load_session(&sid).unwrap();
        assert_eq!(history.len(), 4);
        let resumed = texts(&history[2]);
        assert!(
            resumed
                .iter()
                .any(|t| t.starts_with(&format!("[Background task {id} finished"))),
            "the completion is delivered: {resumed:?}"
        );
        assert!(
            resumed.contains(&format!("[Autonomous: a]\n{CONTINUE_PROMPT}")),
            "a resume continues, it does not resend the body: {resumed:?}"
        );
    }

    #[tokio::test]
    async fn suspension_does_not_spend_max_turns() {
        let (state, log) = crate::serve::ServeState::for_test_scripted_with_log(
            false,
            vec![response("WAIT"), response("working"), response("DONE")],
        );
        write_task(&state, "a", "max_turns: 2");
        let (mut lp, seen) = loop_registering(&state, "a");
        lp.run_cycle().await.unwrap();
        assert_eq!(log.calls().len(), 1, "suspended after the WAIT turn");
        assert!(!rows_for(&state, "a")[0].is_closed);
        let (sid, id) = seen.lock().unwrap().clone().unwrap();
        state.background.complete(&sid, &id, finished());

        lp.run_cycle().await.unwrap();
        let rows = rows_for(&state, "a");
        assert_eq!(rows.len(), 1);
        assert!(rows[0].is_closed, "the WAIT turn counts: turn 2 caps");
        assert_eq!(log.calls().len(), 2, "no third turn");
    }

    /// Turns before a compaction checkpoint still count: the model's view
    /// of the history loses them, the log does not.
    #[tokio::test]
    async fn turns_before_a_compaction_still_count_on_resume() {
        let (state, log) = crate::serve::ServeState::for_test_scripted_with_log(
            false,
            vec![response("WAIT"), response("working"), response("working")],
        );
        write_task(&state, "a", "max_turns: 2");
        let (mut lp, seen) = loop_registering(&state, "a");
        lp.run_cycle().await.unwrap();
        let (sid, id) = seen.lock().unwrap().clone().unwrap();
        state
            .autonomous_session_store
            .append_summary(&sid, "the WAIT turn", 0)
            .unwrap();
        state.background.complete(&sid, &id, finished());

        lp.run_cycle().await.unwrap();
        assert!(rows_for(&state, "a")[0].is_closed, "turn 2 caps");
        assert_eq!(log.calls().len(), 2, "the compacted turn was counted");
    }

    #[tokio::test]
    async fn done_cancels_pending_tasks() {
        let state = crate::serve::ServeState::for_test_scripted(false, vec![response("DONE")]);
        write_task(&state, "a", "max_turns: 3");
        let (mut lp, seen) = loop_registering(&state, "a");
        lp.run_cycle().await.unwrap();
        let (sid, id) = seen.lock().unwrap().clone().unwrap();

        assert!(rows_for(&state, "a")[0].is_closed);
        assert!(
            !state.background.has_pending_or_inbox(&sid),
            "closing the session tears down its background tasks"
        );
        assert!(
            !state.background.complete(&sid, &id, finished()),
            "the task was cancelled, so a late completion is dropped"
        );
    }

    #[tokio::test]
    async fn wait_with_nothing_pending_continues() {
        let state = crate::serve::ServeState::for_test_scripted(
            false,
            vec![response("WAIT"), response("working")],
        );
        write_task(&state, "a", "max_turns: 2");
        let mut lp = AutonomousLoop::new(Arc::clone(&state), None);
        lp.run_cycle().await.unwrap();

        let rows = rows_for(&state, "a");
        assert_eq!(rows.len(), 1);
        assert!(rows[0].is_closed, "max_turns closes it");
        let history = state
            .autonomous_session_store
            .load_session(&rows[0].meta.session_id)
            .unwrap();
        assert_eq!(history.len(), 4, "WAIT with nothing to wait on continues");
        assert_eq!(
            texts(&history[2]),
            vec![format!("[Autonomous: a]\n{CONTINUE_PROMPT}")]
        );
    }

    #[tokio::test]
    async fn a_suspended_task_resumes_after_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let mut first = crate::serve::ServeState::for_test_scripted(false, vec![response("WAIT")]);
        Arc::get_mut(&mut first).unwrap().background =
            crate::background::BackgroundTasks::open(Some(dir.path().to_path_buf()), 4);
        write_task(&first, "a", "max_turns: 5");
        let (mut lp, seen) = loop_registering(&first, "a");
        lp.run_cycle().await.unwrap();
        drop(lp);
        let (sid, id) = seen.lock().unwrap().clone().unwrap();
        assert!(!rows_for(&first, "a")[0].is_closed);

        // The restarted process: same workspace and session store, a
        // registry reopened from the same directory.
        let mut second = crate::serve::ServeState::for_test_scripted(false, vec![response("DONE")]);
        {
            let s = Arc::get_mut(&mut second).unwrap();
            s.background =
                crate::background::BackgroundTasks::open(Some(dir.path().to_path_buf()), 4);
            s.workspace = Arc::clone(&first.workspace);
            s.autonomous_session_store = Arc::clone(&first.autonomous_session_store);
        }
        let mut lp = AutonomousLoop::new(Arc::clone(&second), None);
        lp.run_cycle().await.unwrap();

        let rows = rows_for(&second, "a");
        assert_eq!(rows.len(), 1, "resumed, not started afresh");
        assert!(rows[0].is_closed);
        let history = second.autonomous_session_store.load_session(&sid).unwrap();
        assert_eq!(history.len(), 4);
        let resumed = texts(&history[2]);
        assert!(
            resumed
                .iter()
                .any(|t| t.contains(&format!("{id} was interrupted by a restart"))),
            "the interrupted task is delivered: {resumed:?}"
        );
    }

    /// A session whose store cannot be written does not record its turns,
    /// so `turns_spent` never advances and the undelivered completion
    /// stays in the inbox. A model that keeps replying `WAIT` would then be
    /// resumed, and spend a provider call, on every cycle. The turns this
    /// process ran still count: the session is capped at `max_turns`.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_unwritable_session_is_still_capped_at_max_turns() {
        use std::os::unix::fs::PermissionsExt;
        let (state, log) = crate::serve::ServeState::for_test_scripted_with_log(
            false,
            (0..8).map(|_| response("WAIT")).collect(),
        );
        write_task(&state, "a", "max_turns: 3");
        let (mut lp, seen) = loop_registering(&state, "a");
        lp.run_cycle().await.unwrap();
        assert_eq!(log.calls().len(), 1, "suspended after the first WAIT");
        let (sid, id) = seen.lock().unwrap().clone().unwrap();

        let dir = state
            .autonomous_session_store
            .absolute_path_for(&sid)
            .unwrap();
        assert!(dir.is_dir(), "a session is a directory of entries");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).unwrap();
        // Root writes through the mode bits; the scenario cannot be built.
        if std::fs::write(dir.join("probe"), "").is_ok() {
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
            return;
        }
        assert!(state.background.complete(&sid, &id, finished()));

        for _ in 0..6 {
            lp.run_cycle().await.unwrap();
        }
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();

        assert_eq!(
            log.calls().len(),
            3,
            "max_turns provider calls, not one per cycle"
        );
        assert!(
            !state.background.has_pending_or_inbox(&sid),
            "the capped session's registry is torn down"
        );
    }
}
