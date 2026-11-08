//! Registry of background tasks: work started in one turn whose result is
//! delivered into a later turn of the same session.
//!
//! A task is `register`ed (which enforces the per-session cap), has its
//! tokio `AbortHandle` `attach`ed, and ends either through `complete` or
//! `cancel`. Either way the result lands in the session's inbox as a
//! [`Completion`]. Delivery is two-phase so a crash between "handed to the
//! model" and "turn saved" loses nothing: `drain` hands the inbox out but
//! keeps the entries *in flight*, and only `commit_delivery` forgets them.
//!
//! Each session's `{pending, inbox}` is written to `<dir>/<session>.json`
//! whenever it changes (in-flight entries count as inbox, so a crash before
//! `commit_delivery` redelivers). On `open`, tasks that were still pending
//! are turned into `Interrupted` completions: their futures died with the
//! process. Delivery is wired in by later tasks, hence the `dead_code`
//! allowance until then.
#![allow(dead_code)] // Tasks 3-7 wire this into the agent.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::task::AbortHandle;
use tracing::warn;
use uuid::Uuid;

use crate::subagent_cache::check_file_stem;

pub(crate) type TaskId = String;

/// What a background task runs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) enum TaskKind {
    /// A delegated subagent; `handle` lets the parent resume its conversation.
    Subagent { agent: String, handle: String },
    /// A question posted to the consult room, waiting for a human's reply.
    /// It has no future to abort: an answer, the deadline or a withdrawal
    /// ends it. `asked_by` is the handle of the subagent that asked, when a
    /// child did rather than the session itself.
    Question {
        room: String,
        message_ids: Vec<String>,
        question: String,
        asked_by: Option<String>,
        deadline: DateTime<Utc>,
    },
    /// A shell command run in the background. `timeout_secs` is its run time
    /// limit; `on` says whose machine it runs on.
    Shell {
        command: String,
        on: ShellHost,
        timeout_secs: u64,
    },
}

/// Where a background shell command runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum ShellHost {
    /// The agent's own host.
    Agent,
    /// The connected editor's machine (ACP).
    Editor,
}

/// The first line of `s`, or `""` when it is empty.
pub(crate) fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or_default()
}

/// Most questions one session may have waiting at once. They do not count
/// toward `max_per_session`.
pub(crate) const MAX_OPEN_QUESTIONS: usize = 3;

/// Where a reply to a consult-room message belongs.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Route {
    /// A reply to a question still waiting for its answer.
    Pending { session: String, id: TaskId },
    /// A reply to a question already answered, or past its deadline.
    Answered { session: String, id: TaskId },
    /// A withdrawn question, a question of a finished session, or an unknown message.
    Unknown,
}

/// A task that has been started and has not yet ended.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct PendingTask {
    pub id: TaskId,
    pub kind: TaskKind,
    pub label: String,
    pub started_at: DateTime<Utc>,
}

/// A tool call the subagent wanted to make but could not, because nobody was
/// there to approve it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct RefusedCall {
    pub name: String,
    pub input: serde_json::Value,
}

/// How a task ended.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) enum Outcome {
    Finished {
        text: String,
        refused: Vec<RefusedCall>,
    },
    Failed {
        message: String,
    },
    /// The server restarted while the task ran.
    Interrupted,
    Cancelled,
    /// A human answered a question.
    Answered {
        text: String,
        from: String,
    },
    /// A question reached its deadline unanswered.
    NoAnswer,
    /// A reply to a question that had already ended.
    FollowUp {
        text: String,
        from: String,
    },
    /// A background shell command ended on its own or at its time limit.
    /// `exit_code` is `None` when it was killed by a signal or timed out.
    Exited {
        exit_code: Option<i32>,
        output: String,
        timed_out: bool,
    },
}

/// A task's result, waiting to be shown to the model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Completion {
    pub task_id: TaskId,
    pub kind: TaskKind,
    pub outcome: Outcome,
    pub finished_at: DateTime<Utc>,
}

/// What is written for one session. `inbox` includes in-flight entries.
#[derive(Serialize, Deserialize)]
struct Persisted {
    pending: Vec<PendingTask>,
    inbox: Vec<Completion>,
    #[serde(default)]
    closed_questions: Vec<PendingTask>,
}

struct Running {
    task: PendingTask,
    abort: Option<AbortHandle>,
}

#[derive(Default)]
struct SessionTasks {
    running: Vec<Running>,
    /// Completed, not yet handed out by `drain`.
    inbox: Vec<Completion>,
    /// Handed out by `drain`, not yet confirmed by `commit_delivery`.
    in_flight: Vec<Completion>,
    /// Questions that ended but can still be replied to (follow-ups).
    closed_questions: Vec<PendingTask>,
}

/// `bg-` and 8 hex digits from the random tail of a v7 UUID. Its head is a
/// millisecond timestamp whose first 8 digits change only every ~65s, so
/// they would repeat for tasks registered close together.
fn new_id() -> TaskId {
    format!("bg-{}", &Uuid::now_v7().simple().to_string()[24..])
}

fn is_question(t: &PendingTask) -> bool {
    matches!(t.kind, TaskKind::Question { .. })
}

pub(crate) struct BackgroundTasks {
    /// Where state is persisted; `None` for the in-memory registry, which
    /// never touches disk.
    dir: Option<PathBuf>,
    max_per_session: usize,
    sessions: Mutex<HashMap<String, SessionTasks>>,
}

impl BackgroundTasks {
    pub(crate) fn in_memory(max_per_session: usize) -> Arc<Self> {
        Arc::new(Self {
            dir: None,
            max_per_session,
            sessions: Mutex::new(HashMap::new()),
        })
    }

    /// `~/.cache/sapphire-agent/background`, beside the subagent cache.
    pub(crate) fn default_dir() -> Option<PathBuf> {
        dirs::cache_dir().map(|d| d.join("sapphire-agent").join("background"))
    }

    /// Open the registry backed by `dir` (or in memory for `None`), loading
    /// what a previous run left. Unreadable files count as absent. Pending
    /// questions stay pending (the caller re-arms their deadlines, with
    /// `Consult::rearm` or `consult::arm_deadlines_without_room`); every other
    /// pending task becomes an `Interrupted` completion. The file is rewritten.
    pub(crate) fn open(dir: Option<PathBuf>, max_per_session: usize) -> Arc<Self> {
        let mut sessions = HashMap::new();
        if let Some(d) = &dir {
            if let Err(e) = std::fs::create_dir_all(d) {
                warn!(
                    "background tasks: cannot create {d:?} ({e}); state will not survive a restart"
                );
            }
            for entry in std::fs::read_dir(d).into_iter().flatten().flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("json") {
                    continue;
                }
                let Some(session) = path.file_stem().and_then(|s| s.to_str()) else {
                    continue;
                };
                if check_file_stem(session).is_err() {
                    continue;
                }
                let persisted = match std::fs::read_to_string(&path)
                    .map_err(anyhow::Error::from)
                    .and_then(|t| serde_json::from_str::<Persisted>(&t).map_err(Into::into))
                {
                    Ok(p) => p,
                    Err(e) => {
                        warn!("background tasks: {path:?} is unreadable ({e}); treating as empty");
                        continue;
                    }
                };
                let mut s = SessionTasks {
                    inbox: persisted.inbox,
                    closed_questions: persisted.closed_questions,
                    ..Default::default()
                };
                let now = Utc::now();
                for p in persisted.pending {
                    if is_question(&p) {
                        // Nothing ran in the process: the question is still
                        // waiting for its reply.
                        s.running.push(Running {
                            task: p,
                            abort: None,
                        });
                        continue;
                    }
                    s.inbox.push(Completion {
                        task_id: p.id,
                        kind: p.kind,
                        outcome: Outcome::Interrupted,
                        finished_at: now,
                    });
                }
                sessions.insert(session.to_string(), s);
            }
        }
        let this = Arc::new(Self {
            dir,
            max_per_session,
            sessions: Mutex::new(sessions),
        });
        {
            let map = this.lock();
            for (session, s) in map.iter() {
                this.persist(session, s);
            }
        }
        this
    }

    /// Write the session's state, or remove its file when there is none.
    /// Failures only warn: the in-memory state stays authoritative.
    fn persist(&self, session: &str, s: &SessionTasks) {
        let Some(dir) = &self.dir else { return };
        if check_file_stem(session).is_err() {
            // A2A session ids carry client-supplied context ids, so this
            // can happen; say so rather than lose restart safety silently.
            warn!(
                "background tasks: session id {session:?} is not a safe file name; \
                 its state will not survive a restart"
            );
            return;
        }
        let path = dir.join(format!("{session}.json"));
        if s.running.is_empty()
            && s.inbox.is_empty()
            && s.in_flight.is_empty()
            && s.closed_questions.is_empty()
        {
            let _ = std::fs::remove_file(&path);
            return;
        }
        let body = Persisted {
            pending: s.running.iter().map(|r| r.task.clone()).collect(),
            inbox: s.in_flight.iter().chain(s.inbox.iter()).cloned().collect(),
            closed_questions: s.closed_questions.clone(),
        };
        let result = serde_json::to_string(&body)
            .map_err(anyhow::Error::from)
            .and_then(|json| {
                let tmp = dir.join(format!("{session}.json.{}.tmp", Uuid::now_v7()));
                std::fs::write(&tmp, json)?;
                std::fs::rename(&tmp, &path)?;
                Ok(())
            });
        if let Err(e) = result {
            warn!("background tasks: cannot write {path:?} ({e})");
        }
    }

    /// Whether background execution is on (`max_per_session > 0`).
    pub(crate) fn enabled(&self) -> bool {
        self.max_per_session > 0
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, SessionTasks>> {
        self.sessions.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Record a new task, refusing past the cap. The check and the insert
    /// share one lock so concurrent callers cannot both squeeze under it.
    pub(crate) fn register(&self, session: &str, kind: TaskKind, label: String) -> Result<TaskId> {
        debug_assert!(
            !matches!(kind, TaskKind::Question { .. }),
            "questions go through register_question"
        );
        if !self.enabled() {
            bail!("background tasks are disabled (tools.background.max_per_session = 0)");
        }
        let mut map = self.lock();
        let s = map.entry(session.to_string()).or_default();
        let held: Vec<&str> = s
            .running
            .iter()
            .filter(|r| !is_question(&r.task))
            .map(|r| r.task.id.as_str())
            .collect();
        if held.len() >= self.max_per_session {
            bail!(
                "this session already has {} background tasks running ({}); \
                 wait for one to finish or cancel one with background_task",
                held.len(),
                held.join(", ")
            );
        }
        let id = new_id();
        s.running.push(Running {
            task: PendingTask {
                id: id.clone(),
                kind,
                label,
                started_at: Utc::now(),
            },
            abort: None,
        });
        self.persist(session, s);
        Ok(id)
    }

    /// Record a question as pending, refusing past [`MAX_OPEN_QUESTIONS`].
    /// Questions are not subject to `max_per_session`, and the registry being
    /// disabled does not stop them: they run nothing.
    pub(crate) fn register_question(
        &self,
        session: &str,
        kind: TaskKind,
        label: String,
    ) -> Result<TaskId> {
        debug_assert!(matches!(kind, TaskKind::Question { .. }));
        let mut map = self.lock();
        let s = map.entry(session.to_string()).or_default();
        if s.running.iter().filter(|r| is_question(&r.task)).count() >= MAX_OPEN_QUESTIONS {
            bail!(
                "You already have 3 questions waiting for an answer. \
                 Wait for one, or withdraw one with background_task."
            );
        }
        let id = new_id();
        s.running.push(Running {
            task: PendingTask {
                id: id.clone(),
                kind,
                label,
                started_at: Utc::now(),
            },
            abort: None,
        });
        self.persist(session, s);
        Ok(id)
    }

    /// Forget a running task without queuing a completion, for a start that
    /// failed after `register` (the caller reports the failure itself).
    /// `false` when `id` is not running in `session`.
    pub(crate) fn discard(&self, session: &str, id: &str) -> bool {
        let mut map = self.lock();
        let Some(s) = map.get_mut(session) else {
            return false;
        };
        let Some(pos) = s.running.iter().position(|r| r.task.id == id) else {
            return false;
        };
        s.running.remove(pos);
        self.persist(session, s);
        true
    }

    /// Which question a reply to `reply_to` in `room` belongs to.
    pub(crate) fn route_reply(&self, room: &str, reply_to: &str) -> Route {
        let matches = |t: &PendingTask| {
            matches!(&t.kind, TaskKind::Question { room: r, message_ids, .. }
                if r == room && message_ids.iter().any(|m| m == reply_to))
        };
        let map = self.lock();
        for (session, s) in map.iter() {
            if let Some(r) = s.running.iter().find(|r| matches(&r.task)) {
                return Route::Pending {
                    session: session.clone(),
                    id: r.task.id.clone(),
                };
            }
        }
        for (session, s) in map.iter() {
            if let Some(t) = s.closed_questions.iter().find(|t| matches(t)) {
                return Route::Answered {
                    session: session.clone(),
                    id: t.id.clone(),
                };
            }
        }
        Route::Unknown
    }

    /// End a pending question with `outcome`: it moves to the closed list
    /// and a completion is queued. `false` when `id` is not a pending
    /// question of `session`.
    fn close_question(&self, session: &str, id: &str, outcome: Outcome) -> bool {
        let mut map = self.lock();
        let Some(s) = map.get_mut(session) else {
            return false;
        };
        let Some(pos) = s
            .running
            .iter()
            .position(|r| r.task.id == id && is_question(&r.task))
        else {
            return false;
        };
        let r = s.running.remove(pos);
        s.closed_questions.push(r.task.clone());
        s.inbox.push(Completion {
            task_id: r.task.id,
            kind: r.task.kind,
            outcome,
            finished_at: Utc::now(),
        });
        self.persist(session, s);
        true
    }

    /// A human answered the pending question `id`.
    pub(crate) fn answer(&self, session: &str, id: &str, text: String, from: String) -> bool {
        self.close_question(session, id, Outcome::Answered { text, from })
    }

    /// The question's deadline passed. `false` if it already ended.
    pub(crate) fn expire(&self, session: &str, id: &str) -> bool {
        self.close_question(session, id, Outcome::NoAnswer)
    }

    /// Queue a reply to a question that has already ended. `false` when `id`
    /// is not a closed question of `session`.
    pub(crate) fn follow_up(&self, session: &str, id: &str, text: String, from: String) -> bool {
        let mut map = self.lock();
        let Some(s) = map.get_mut(session) else {
            return false;
        };
        let Some(t) = s.closed_questions.iter().find(|t| t.id == id) else {
            return false;
        };
        let (task_id, kind) = (t.id.clone(), t.kind.clone());
        s.inbox.push(Completion {
            task_id,
            kind,
            outcome: Outcome::FollowUp { text, from },
            finished_at: Utc::now(),
        });
        self.persist(session, s);
        true
    }

    /// Questions of `session` still waiting for an answer.
    pub(crate) fn open_questions(&self, session: &str) -> usize {
        self.lock().get(session).map_or(0, |s| {
            s.running.iter().filter(|r| is_question(&r.task)).count()
        })
    }

    /// Every pending question, with its session, for re-arming deadlines
    /// after a restart.
    pub(crate) fn pending_questions(&self) -> Vec<(String, PendingTask)> {
        self.lock()
            .iter()
            .flat_map(|(session, s)| {
                s.running
                    .iter()
                    .filter(|r| is_question(&r.task))
                    .map(|r| (session.clone(), r.task.clone()))
            })
            .collect()
    }

    /// Store the handle used to abort the task. The registry holds only the
    /// `AbortHandle`, not the `JoinHandle`, so it never owns the output.
    pub(crate) fn attach(&self, session: &str, id: &str, abort: AbortHandle) {
        if let Some(r) = self
            .lock()
            .get_mut(session)
            .and_then(|s| s.running.iter_mut().find(|r| r.task.id == id))
        {
            r.abort = Some(abort);
        }
    }

    /// Finish a task. `false` when `id` is not pending (e.g. already cancelled),
    /// in which case the outcome is discarded.
    pub(crate) fn complete(&self, session: &str, id: &str, outcome: Outcome) -> bool {
        let mut map = self.lock();
        let Some(s) = map.get_mut(session) else {
            return false;
        };
        let Some(pos) = s.running.iter().position(|r| r.task.id == id) else {
            return false;
        };
        let r = s.running.remove(pos);
        s.inbox.push(Completion {
            task_id: r.task.id,
            kind: r.task.kind,
            outcome,
            finished_at: Utc::now(),
        });
        self.persist(session, s);
        true
    }

    /// Hand out the inbox, oldest first. The entries stay in flight until
    /// [`commit_delivery`](Self::commit_delivery).
    pub(crate) fn drain(&self, session: &str) -> Vec<Completion> {
        let mut map = self.lock();
        let Some(s) = map.get_mut(session) else {
            return Vec::new();
        };
        let mut out = std::mem::take(&mut s.inbox);
        out.sort_by_key(|c| c.finished_at);
        s.in_flight.extend(out.iter().cloned());
        out
    }

    /// The drained completions reached the model; forget them.
    pub(crate) fn commit_delivery(&self, session: &str) {
        if let Some(s) = self.lock().get_mut(session) {
            s.in_flight.clear();
            self.persist(session, s);
        }
    }

    /// The drained completions did not reach the model (the message that
    /// carried them could not be saved): put them back in the inbox, in
    /// finish order, so the next turn delivers them. Without this they
    /// would sit in flight until some later `commit_delivery` wiped them.
    pub(crate) fn rollback_delivery(&self, session: &str) {
        if let Some(s) = self.lock().get_mut(session) {
            s.inbox.append(&mut s.in_flight);
            s.inbox.sort_by_key(|c| c.finished_at);
            self.persist(session, s);
        }
    }

    pub(crate) fn pending(&self, session: &str) -> Vec<PendingTask> {
        self.lock()
            .get(session)
            .map(|s| s.running.iter().map(|r| r.task.clone()).collect())
            .unwrap_or_default()
    }

    /// Undelivered completions that have not been drained.
    pub(crate) fn has_inbox(&self, session: &str) -> bool {
        self.lock()
            .get(session)
            .is_some_and(|s| !s.inbox.is_empty())
    }

    pub(crate) fn has_pending_or_inbox(&self, session: &str) -> bool {
        self.lock().get(session).is_some_and(|s| {
            !s.running.is_empty() || !s.inbox.is_empty() || !s.in_flight.is_empty()
        })
    }

    /// Abort one task and queue a `Cancelled` completion for it.
    pub(crate) fn cancel(&self, session: &str, id: &str) -> Result<PendingTask> {
        let mut map = self.lock();
        let s = map.entry(session.to_string()).or_default();
        let Some(pos) = s.running.iter().position(|r| r.task.id == id) else {
            let held: Vec<&str> = s.running.iter().map(|r| r.task.id.as_str()).collect();
            bail!(
                "no running background task {id}; running: {}",
                if held.is_empty() {
                    "none".to_string()
                } else {
                    held.join(", ")
                }
            );
        };
        let r = s.running.remove(pos);
        let task = r.task.clone();
        Self::cancel_running(s, r);
        self.persist(session, s);
        Ok(task)
    }

    /// Tear the session down: abort every running task and forget the
    /// session entirely — its entry, its undelivered completions and its
    /// file. Nothing is queued, because a closed session has no next turn
    /// to deliver to. Callers are session-close paths; to stop one task
    /// and tell the model about it, use [`cancel`](Self::cancel).
    ///
    /// A child that outlives this (aborted mid-poll, or finishing between
    /// `register` and `attach`) gets `false` from `complete`, which never
    /// creates an entry, so neither the entry nor the file comes back.
    ///
    /// Returns the pending questions it dropped (subagents are aborted and
    /// not returned), so the caller can withdraw them in the room.
    pub(crate) fn cancel_all(&self, session: &str) -> Vec<PendingTask> {
        let removed = self.lock().remove(session);
        let mut questions = Vec::new();
        if let Some(s) = removed {
            for r in s.running {
                if let Some(a) = r.abort {
                    a.abort();
                }
                if is_question(&r.task) {
                    questions.push(r.task);
                }
            }
        }
        if let Some(dir) = &self.dir
            && check_file_stem(session).is_ok()
        {
            let _ = std::fs::remove_file(dir.join(format!("{session}.json")));
        }
        questions
    }

    fn cancel_running(s: &mut SessionTasks, r: Running) {
        if let Some(a) = r.abort {
            a.abort();
        }
        s.inbox.push(Completion {
            task_id: r.task.id,
            kind: r.task.kind,
            outcome: Outcome::Cancelled,
            finished_at: Utc::now(),
        });
    }
}

/// Whether `c`'s handle can be resumed: its history is stored in `cache`.
/// Checked when the completion is shown, not when it was queued, since
/// what makes it resumable is the cache entry at that moment.
pub(crate) fn resumable(
    cache: Option<&crate::subagent_cache::SubagentCache>,
    c: &Completion,
) -> bool {
    let TaskKind::Subagent { handle, .. } = &c.kind else {
        return false; // a question or a command has no conversation to resume
    };
    cache.is_some_and(|cache| cache.get(handle).is_some())
}

/// Text shown to the model for one completion. An unfinished one offers
/// its handle for `resume` only when `resumable` (see [`resumable`]):
/// pointing the model at a handle with no stored history invites a resume
/// that can only fail.
pub(crate) fn render(c: &Completion, resumable: bool) -> String {
    let (agent, handle) = match &c.kind {
        TaskKind::Subagent { agent, handle } => (agent, handle),
        TaskKind::Question { .. } => return render_question(c),
        TaskKind::Shell { .. } => return render_shell(c),
    };
    let verb = match &c.outcome {
        Outcome::Finished { .. } => "finished",
        Outcome::Failed { .. } => "failed",
        Outcome::Interrupted => "was interrupted by a restart",
        Outcome::Cancelled => "was cancelled",
        // Only questions end this way; see `render_question`.
        Outcome::Answered { .. }
        | Outcome::NoAnswer
        | Outcome::FollowUp { .. }
        | Outcome::Exited { .. } => "ended",
    };
    let mut out = format!(
        "[Background task {} {verb}: subagent \"{agent}\" (handle {handle})]",
        c.task_id
    );
    match &c.outcome {
        Outcome::Finished { text, refused } => {
            out.push('\n');
            out.push_str(text);
            for r in refused {
                out.push_str(&format!(
                    "\nRefused (needs approval): {} {}",
                    r.name, r.input
                ));
            }
        }
        Outcome::Failed { message } => {
            out.push('\n');
            out.push_str(message);
        }
        Outcome::Interrupted
        | Outcome::Cancelled
        | Outcome::Answered { .. }
        | Outcome::NoAnswer
        | Outcome::FollowUp { .. }
        | Outcome::Exited { .. } => {}
    }
    if resumable && !matches!(c.outcome, Outcome::Finished { .. }) {
        out.push_str(&format!(
            "\nTo continue its work, pass handle {handle} as `resume` to the subagent tool."
        ));
    }
    out
}

/// A question's completion, shown to the model.
fn render_question(c: &Completion) -> String {
    let TaskKind::Question {
        question, asked_by, ..
    } = &c.kind
    else {
        unreachable!("render_question is called for questions only");
    };
    let id = &c.task_id;
    let first = question.lines().next().unwrap_or_default();
    let mut out = match &c.outcome {
        Outcome::Answered { text, from } => {
            format!("[Question {id} answered by {from}: \"{first}\"]\n{text}")
        }
        Outcome::NoAnswer => format!(
            "[Question {id}: no answer by the deadline: \"{first}\"]\n\
             Decide for yourself whether to proceed on a guess or stop; \
             if you proceed, say so in your report."
        ),
        Outcome::FollowUp { text, from } => {
            format!("[Question {id} follow-up by {from}: \"{first}\"]\n{text}")
        }
        Outcome::Cancelled => format!("[Question {id} was withdrawn: \"{first}\"]"),
        Outcome::Interrupted => {
            format!("[Question {id} was interrupted by a restart: \"{first}\"]")
        }
        // A question never finishes or fails like a subagent; keep it total.
        Outcome::Finished { text, .. } => format!("[Question {id} ended: \"{first}\"]\n{text}"),
        Outcome::Failed { message } => format!("[Question {id} ended: \"{first}\"]\n{message}"),
        Outcome::Exited { .. } => format!("[Question {id} ended: \"{first}\"]"),
    };
    if let Some(h) = asked_by {
        out.push_str(&format!(
            "\nTo pass this to the subagent, resume handle {h}."
        ));
    }
    out
}

/// A background shell command's completion, shown to the model.
fn render_shell(c: &Completion) -> String {
    let TaskKind::Shell {
        command,
        timeout_secs,
        ..
    } = &c.kind
    else {
        unreachable!("render_shell is called for shell commands only");
    };
    let id = &c.task_id;
    let first = first_line(command);
    match &c.outcome {
        Outcome::Exited {
            exit_code,
            output,
            timed_out,
        } => {
            let how = if *timed_out {
                format!("timed out after {timeout_secs}s and was killed")
            } else if let Some(code) = exit_code {
                format!("exited {code}")
            } else {
                "exited without a code".to_string()
            };
            let mut out = format!("[Command {id} {how}: \"{first}\"]");
            if !output.is_empty() {
                out.push('\n');
                out.push_str(output);
            }
            out
        }
        Outcome::Interrupted => format!(
            "[Command {id} was interrupted by a restart: \"{first}\"]\n\
             It was stopped; re-run it if you still need it."
        ),
        Outcome::Cancelled => format!("[Command {id} was cancelled: \"{first}\"]"),
        Outcome::Failed { message } => format!("[Command {id} failed: \"{first}\"]\n{message}"),
        // A command yields no subagent or question outcome; keep it total.
        Outcome::Finished { text, .. } => format!("[Command {id} ended: \"{first}\"]\n{text}"),
        Outcome::Answered { .. } | Outcome::NoAnswer | Outcome::FollowUp { .. } => {
            format!("[Command {id} ended: \"{first}\"]")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kind(h: &str) -> TaskKind {
        TaskKind::Subagent {
            agent: "reviewer".into(),
            handle: h.into(),
        }
    }

    #[test]
    fn a_rolled_back_delivery_is_drained_again() {
        let t = BackgroundTasks::in_memory(4);
        for h in ["a", "b"] {
            let id = t.register("s", kind(h), h.into()).unwrap();
            t.complete("s", &id, fin());
        }
        let first = t.drain("s");
        assert_eq!(first.len(), 2);
        assert!(!t.has_inbox("s"));
        t.rollback_delivery("s");
        assert!(t.has_inbox("s"));
        let again = t.drain("s");
        let ids = |v: &[Completion]| v.iter().map(|c| c.task_id.clone()).collect::<Vec<_>>();
        assert_eq!(ids(&first), ids(&again));
    }

    fn fin() -> Outcome {
        Outcome::Finished {
            text: "done".into(),
            refused: vec![],
        }
    }

    #[test]
    fn register_refuses_past_the_cap_naming_every_held_id() {
        let t = BackgroundTasks::in_memory(2);
        let a = t.register("s", kind("h1"), "a".into()).unwrap();
        let b = t.register("s", kind("h2"), "b".into()).unwrap();
        let e = t
            .register("s", kind("h3"), "c".into())
            .unwrap_err()
            .to_string();
        assert!(
            e.contains(&a) && e.contains(&b) && e.contains("background_task"),
            "{e}"
        );
    }

    #[test]
    fn register_with_zero_cap_is_refused() {
        let t = BackgroundTasks::in_memory(0);
        assert!(!t.enabled());
        assert!(t.register("s", kind("h"), "a".into()).is_err());
    }

    #[test]
    fn complete_moves_pending_to_inbox() {
        let t = BackgroundTasks::in_memory(2);
        let id = t.register("s", kind("h"), "a".into()).unwrap();
        assert!(id.starts_with("bg-") && id.len() == 11);
        assert!(t.complete("s", &id, fin()));
        assert!(t.pending("s").is_empty());
        assert!(t.has_inbox("s"));
        assert_eq!(t.drain("s").len(), 1);
    }

    #[test]
    fn drain_then_commit_empties_the_inbox() {
        let t = BackgroundTasks::in_memory(2);
        let id = t.register("s", kind("h"), "a".into()).unwrap();
        t.complete("s", &id, fin());
        t.drain("s");
        assert!(!t.has_inbox("s"));
        assert!(t.has_pending_or_inbox("s"));
        t.commit_delivery("s");
        assert!(!t.has_pending_or_inbox("s"));
    }

    #[test]
    fn complete_after_cancel_is_a_no_op() {
        let t = BackgroundTasks::in_memory(2);
        let id = t.register("s", kind("h"), "a".into()).unwrap();
        t.cancel("s", &id).unwrap();
        assert!(!t.complete("s", &id, fin()));
        let d = t.drain("s");
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].outcome, Outcome::Cancelled);
    }

    #[test]
    fn cancel_unknown_id_lists_running_ids() {
        let t = BackgroundTasks::in_memory(2);
        let id = t.register("s", kind("h"), "a".into()).unwrap();
        let e = t.cancel("s", "bg-nope").unwrap_err().to_string();
        assert!(e.contains(&id), "{e}");
    }

    #[tokio::test]
    async fn cancel_all_aborts_attached_tasks() {
        let t = BackgroundTasks::in_memory(2);
        let id = t.register("s", kind("h"), "a".into()).unwrap();
        let jh = tokio::spawn(std::future::pending::<()>());
        t.attach("s", &id, jh.abort_handle());
        t.cancel_all("s");
        let err = jh.await.unwrap_err();
        assert!(err.is_cancelled());
    }

    #[test]
    fn render_formats_each_outcome() {
        let mk = |outcome| Completion {
            task_id: "bg-1".into(),
            kind: kind("h1"),
            outcome,
            finished_at: Utc::now(),
        };
        let s = render(
            &mk(Outcome::Finished {
                text: "all good".into(),
                refused: vec![RefusedCall {
                    name: "shell".into(),
                    input: serde_json::json!({"command": "cargo test"}),
                }],
            }),
            true,
        );
        assert!(
            s.starts_with("[Background task bg-1 finished: subagent \"reviewer\" (handle h1)]")
        );
        assert!(s.contains("all good"));
        assert!(s.contains("Refused (needs approval): shell {\"command\":\"cargo test\"}"));
        let s = render(&mk(Outcome::Interrupted), true);
        assert!(s.contains("was interrupted by a restart"));
        assert!(s.contains("pass handle h1 as `resume`"));
        // A handle with no stored history is not offered for resume.
        let s = render(&mk(Outcome::Cancelled), false);
        assert!(s.contains("was cancelled"));
        assert!(!s.contains("`resume`"), "{s}");
    }

    fn on_disk(dir: &std::path::Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir)
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        v.sort();
        v
    }

    /// A session file exactly as main (e7deabd, before questions existed)
    /// writes it: no `closed_questions` field.
    const PRE_QUESTIONS_FILE: &str = r#"{"pending":[{"id":"bg-1","kind":{"Subagent":{"agent":"reviewer","handle":"h1"}},"label":"reviewer: go","started_at":"2026-11-01T10:00:00Z"}],"inbox":[{"task_id":"bg-0","kind":{"Subagent":{"agent":"reviewer","handle":"h0"}},"outcome":{"Finished":{"text":"done","refused":[]}},"finished_at":"2026-11-01T09:00:00Z"}]}"#;

    #[test]
    fn a_file_from_before_questions_still_loads() {
        let td = tempfile::tempdir().unwrap();
        std::fs::write(td.path().join("s.json"), PRE_QUESTIONS_FILE).unwrap();
        let t = BackgroundTasks::open(Some(td.path().to_path_buf()), 4);
        assert!(t.pending("s").is_empty());
        let inbox = t.drain("s");
        assert_eq!(inbox.len(), 2, "{inbox:?}");
        assert_eq!(inbox[0].task_id, "bg-0");
        assert_eq!(
            inbox[0].outcome,
            Outcome::Finished {
                text: "done".into(),
                refused: Vec::new()
            }
        );
        assert_eq!(inbox[1].task_id, "bg-1");
        assert_eq!(inbox[1].outcome, Outcome::Interrupted);
        assert_eq!(
            inbox[1].kind,
            TaskKind::Subagent {
                agent: "reviewer".into(),
                handle: "h1".into()
            }
        );
    }

    fn shell(cmd: &str) -> TaskKind {
        TaskKind::Shell {
            command: cmd.into(),
            on: ShellHost::Agent,
            timeout_secs: 1800,
        }
    }

    #[test]
    fn shell_counts_toward_max_per_session() {
        let t = BackgroundTasks::in_memory(1);
        let id = t.register("s", shell("sleep 5"), "sleep".into()).unwrap();
        let e = t
            .register("s", kind("h"), "a".into())
            .unwrap_err()
            .to_string();
        assert!(e.contains(&id), "{e}");
    }

    #[test]
    fn open_interrupts_a_pending_shell() {
        let td = tempfile::tempdir().unwrap();
        let t = BackgroundTasks::open(Some(td.path().to_path_buf()), 4);
        t.register("s", shell("sleep 5"), "sleep".into()).unwrap();
        let t2 = BackgroundTasks::open(Some(td.path().to_path_buf()), 4);
        assert!(t2.pending("s").is_empty());
        let d = t2.drain("s");
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].outcome, Outcome::Interrupted);
        assert_eq!(d[0].kind, shell("sleep 5"));
    }

    #[test]
    fn discard_removes_without_a_completion() {
        let t = BackgroundTasks::in_memory(2);
        let id = t.register("s", shell("ls"), "ls".into()).unwrap();
        assert!(t.discard("s", &id));
        assert!(t.pending("s").is_empty());
        assert!(t.drain("s").is_empty());
        assert!(!t.has_pending_or_inbox("s"));
        assert!(!t.discard("s", &id));
    }

    #[test]
    fn render_shell_outcomes() {
        let mk = |outcome| Completion {
            task_id: "bg-1".into(),
            kind: shell("cargo test\n--workspace"),
            outcome,
            finished_at: Utc::now(),
        };
        let exited = |exit_code, timed_out| Outcome::Exited {
            exit_code,
            output: "line one\nline two".into(),
            timed_out,
        };
        assert_eq!(
            render(&mk(exited(Some(0), false)), false),
            "[Command bg-1 exited 0: \"cargo test\"]\nline one\nline two"
        );
        assert_eq!(
            render(&mk(exited(None, false)), false),
            "[Command bg-1 exited without a code: \"cargo test\"]\nline one\nline two"
        );
        assert_eq!(
            render(&mk(exited(None, true)), false),
            "[Command bg-1 timed out after 1800s and was killed: \"cargo test\"]\nline one\nline two"
        );
        assert_eq!(
            render(&mk(Outcome::Interrupted), false),
            "[Command bg-1 was interrupted by a restart: \"cargo test\"]\n\
             It was stopped; re-run it if you still need it."
        );
        assert_eq!(
            render(&mk(Outcome::Cancelled), false),
            "[Command bg-1 was cancelled: \"cargo test\"]"
        );
        assert_eq!(
            render(
                &mk(Outcome::Failed {
                    message: "boom".into()
                }),
                false
            ),
            "[Command bg-1 failed: \"cargo test\"]\nboom"
        );
    }

    #[test]
    fn old_state_without_shell_still_loads() {
        let td = tempfile::tempdir().unwrap();
        std::fs::write(td.path().join("s.json"), PRE_QUESTIONS_FILE).unwrap();
        let t = BackgroundTasks::open(Some(td.path().to_path_buf()), 4);
        assert_eq!(t.drain("s").len(), 2);
    }

    #[test]
    fn state_round_trips_through_the_file() {
        let td = tempfile::tempdir().unwrap();
        let t = BackgroundTasks::open(Some(td.path().to_path_buf()), 4);
        let id = t.register("s", kind("h"), "a".into()).unwrap();
        t.complete("s", &id, fin());
        let t2 = BackgroundTasks::open(Some(td.path().to_path_buf()), 4);
        assert!(t2.has_inbox("s"));
        assert_eq!(t2.drain("s")[0].outcome, fin());
    }

    #[test]
    fn pending_left_on_disk_becomes_interrupted_on_open() {
        let td = tempfile::tempdir().unwrap();
        let t = BackgroundTasks::open(Some(td.path().to_path_buf()), 4);
        t.register("s", kind("h9"), "a".into()).unwrap();
        let t2 = BackgroundTasks::open(Some(td.path().to_path_buf()), 4);
        assert!(t2.pending("s").is_empty());
        let d = t2.drain("s");
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].outcome, Outcome::Interrupted);
        assert_eq!(d[0].kind, kind("h9"));
        // And the rewrite means a third open sees it as an ordinary completion.
        let t3 = BackgroundTasks::open(Some(td.path().to_path_buf()), 4);
        assert!(t3.pending("s").is_empty());
    }

    #[test]
    fn in_flight_entries_survive_a_crash_before_commit() {
        let td = tempfile::tempdir().unwrap();
        let t = BackgroundTasks::open(Some(td.path().to_path_buf()), 4);
        let id = t.register("s", kind("h"), "a".into()).unwrap();
        t.complete("s", &id, fin());
        assert_eq!(t.drain("s").len(), 1);
        let t2 = BackgroundTasks::open(Some(td.path().to_path_buf()), 4);
        let d = t2.drain("s");
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].task_id, id);
        t2.commit_delivery("s");
        let t3 = BackgroundTasks::open(Some(td.path().to_path_buf()), 4);
        assert!(!t3.has_pending_or_inbox("s"));
    }

    #[test]
    fn an_unreadable_file_is_empty_not_fatal() {
        let td = tempfile::tempdir().unwrap();
        std::fs::write(td.path().join("s.json"), "{ not json").unwrap();
        let t = BackgroundTasks::open(Some(td.path().to_path_buf()), 4);
        assert!(!t.has_pending_or_inbox("s"));
        assert!(t.register("s", kind("h"), "a".into()).is_ok());
    }

    #[test]
    fn an_unsafe_session_id_is_not_written() {
        let td = tempfile::tempdir().unwrap();
        let dir = td.path().join("bg");
        let t = BackgroundTasks::open(Some(dir.clone()), 4);
        let id = t.register("../x", kind("h"), "a".into()).unwrap();
        assert!(t.complete("../x", &id, fin()));
        assert!(t.has_inbox("../x"));
        assert!(on_disk(&dir).is_empty());
        assert_eq!(on_disk(td.path()), vec!["bg".to_string()]);
    }

    #[test]
    fn cancel_all_removes_the_file() {
        let td = tempfile::tempdir().unwrap();
        let t = BackgroundTasks::open(Some(td.path().to_path_buf()), 4);
        t.register("s", kind("h"), "a".into()).unwrap();
        assert_eq!(on_disk(td.path()), vec!["s.json".to_string()]);
        t.cancel_all("s");
        assert!(on_disk(td.path()).is_empty());
        assert!(!t.has_pending_or_inbox("s"));
    }

    /// A child that finishes after teardown is dropped and resurrects
    /// neither the session's entry nor its file.
    #[test]
    fn complete_after_cancel_all_resurrects_nothing() {
        let td = tempfile::tempdir().unwrap();
        let t = BackgroundTasks::open(Some(td.path().to_path_buf()), 4);
        let id = t.register("s", kind("h"), "a".into()).unwrap();
        t.cancel_all("s");
        assert!(!t.complete("s", &id, fin()));
        assert!(!t.has_pending_or_inbox("s"));
        assert!(on_disk(td.path()).is_empty());
    }

    fn question(room: &str, mid: &str, text: &str, asked_by: Option<&str>) -> TaskKind {
        TaskKind::Question {
            room: room.into(),
            message_ids: vec![mid.into()],
            question: text.into(),
            asked_by: asked_by.map(Into::into),
            deadline: Utc::now() + chrono::Duration::hours(24),
        }
    }

    #[test]
    fn question_answer_moves_it_to_closed() {
        let t = BackgroundTasks::in_memory(4);
        let id = t
            .register_question("s", question("!r", "m1", "ok?", None), "q".into())
            .unwrap();
        assert_eq!(
            t.route_reply("!r", "m1"),
            Route::Pending {
                session: "s".into(),
                id: id.clone()
            }
        );
        assert_eq!(t.open_questions("s"), 1);
        assert!(t.answer("s", &id, "yes".into(), "alice".into()));
        let d = t.drain("s");
        assert_eq!(d.len(), 1);
        assert_eq!(
            d[0].outcome,
            Outcome::Answered {
                text: "yes".into(),
                from: "alice".into()
            }
        );
        assert_eq!(
            t.route_reply("!r", "m1"),
            Route::Answered {
                session: "s".into(),
                id
            }
        );
        assert_eq!(t.route_reply("!r", "other"), Route::Unknown);
        assert_eq!(t.route_reply("!other", "m1"), Route::Unknown);
        assert_eq!(t.open_questions("s"), 0);
    }

    #[test]
    fn follow_up_queues_without_pending() {
        let t = BackgroundTasks::in_memory(4);
        let id = t
            .register_question("s", question("!r", "m1", "ok?", None), "q".into())
            .unwrap();
        assert!(!t.follow_up("s", &id, "x".into(), "bob".into()));
        assert!(t.answer("s", &id, "yes".into(), "alice".into()));
        t.drain("s");
        t.commit_delivery("s");
        assert!(t.follow_up("s", &id, "also".into(), "bob".into()));
        assert!(t.pending("s").is_empty());
        let d = t.drain("s");
        assert_eq!(d.len(), 1);
        assert_eq!(
            d[0].outcome,
            Outcome::FollowUp {
                text: "also".into(),
                from: "bob".into()
            }
        );
    }

    #[test]
    fn expire_only_once() {
        let t = BackgroundTasks::in_memory(4);
        let id = t
            .register_question("s", question("!r", "m1", "ok?", None), "q".into())
            .unwrap();
        assert!(t.expire("s", &id));
        assert!(!t.expire("s", &id));
        assert!(!t.answer("s", &id, "late".into(), "a".into()));
        assert!(matches!(t.route_reply("!r", "m1"), Route::Answered { .. }));
        assert_eq!(t.drain("s")[0].outcome, Outcome::NoAnswer);
    }

    #[test]
    fn fourth_question_is_refused() {
        let t = BackgroundTasks::in_memory(4);
        for i in 0..3 {
            t.register_question("s", question("!r", &format!("m{i}"), "q", None), "q".into())
                .unwrap();
        }
        let e = t
            .register_question("s", question("!r", "m9", "q", None), "q".into())
            .unwrap_err()
            .to_string();
        assert_eq!(
            e,
            "You already have 3 questions waiting for an answer. Wait for one, or withdraw one with background_task."
        );
        for h in ["a", "b", "c", "d"] {
            t.register("s", kind(h), h.into()).unwrap();
        }
        assert!(t.register("s", kind("e"), "e".into()).is_err());
    }

    #[test]
    fn open_keeps_questions_pending() {
        let td = tempfile::tempdir().unwrap();
        let t = BackgroundTasks::open(Some(td.path().to_path_buf()), 4);
        let q = t
            .register_question("s", question("!r", "m1", "ok?", None), "q".into())
            .unwrap();
        let closed = t
            .register_question("s", question("!r", "m2", "ok2?", None), "q".into())
            .unwrap();
        t.expire("s", &closed);
        t.register("s", kind("h"), "a".into()).unwrap();
        let t2 = BackgroundTasks::open(Some(td.path().to_path_buf()), 4);
        let pending = t2.pending("s");
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, q);
        assert_eq!(t2.pending_questions().len(), 1);
        let d = t2.drain("s");
        assert!(d.iter().any(|c| c.outcome == Outcome::Interrupted));
        assert!(matches!(t2.route_reply("!r", "m1"), Route::Pending { .. }));
        assert!(matches!(t2.route_reply("!r", "m2"), Route::Answered { .. }));
    }

    #[test]
    fn a_withdrawn_question_is_unknown() {
        let t = BackgroundTasks::in_memory(4);
        let id = t
            .register_question("s", question("!r", "m1", "ok?", None), "q".into())
            .unwrap();
        t.cancel("s", &id).unwrap();
        assert_eq!(t.route_reply("!r", "m1"), Route::Unknown);
        assert!(!t.follow_up("s", &id, "x".into(), "bob".into()));
        assert_eq!(t.drain("s")[0].outcome, Outcome::Cancelled);
    }

    #[test]
    fn cancel_all_returns_questions_and_forgets() {
        let t = BackgroundTasks::in_memory(4);
        let a = t
            .register_question("s", question("!r", "m1", "ok?", None), "q".into())
            .unwrap();
        let b = t
            .register_question("s", question("!r", "m2", "ok2?", None), "q".into())
            .unwrap();
        t.expire("s", &b);
        t.register("s", kind("h"), "a".into()).unwrap();
        let dropped = t.cancel_all("s");
        assert_eq!(dropped.len(), 1);
        assert_eq!(dropped[0].id, a);
        assert_eq!(t.route_reply("!r", "m1"), Route::Unknown);
        assert_eq!(t.route_reply("!r", "m2"), Route::Unknown);
    }

    #[test]
    fn render_question_outcomes() {
        let mk = |outcome, asked_by| Completion {
            task_id: "bg-1".into(),
            kind: question("!r", "m1", "Ship it?\nsecond line", asked_by),
            outcome,
            finished_at: Utc::now(),
        };
        let s = render(
            &mk(
                Outcome::Answered {
                    text: "yes".into(),
                    from: "alice".into(),
                },
                None,
            ),
            false,
        );
        assert_eq!(s, "[Question bg-1 answered by alice: \"Ship it?\"]\nyes");
        let s = render(&mk(Outcome::NoAnswer, None), false);
        assert_eq!(
            s,
            "[Question bg-1: no answer by the deadline: \"Ship it?\"]\nDecide for yourself whether to proceed on a guess or stop; if you proceed, say so in your report."
        );
        let s = render(
            &mk(
                Outcome::FollowUp {
                    text: "wait".into(),
                    from: "bob".into(),
                },
                Some("h7"),
            ),
            false,
        );
        assert_eq!(
            s,
            "[Question bg-1 follow-up by bob: \"Ship it?\"]\nwait\nTo pass this to the subagent, resume handle h7."
        );
        let s = render(&mk(Outcome::Cancelled, None), false);
        assert_eq!(s, "[Question bg-1 was withdrawn: \"Ship it?\"]");
        let s = render(&mk(Outcome::Interrupted, None), false);
        assert_eq!(
            s,
            "[Question bg-1 was interrupted by a restart: \"Ship it?\"]"
        );
    }
}
