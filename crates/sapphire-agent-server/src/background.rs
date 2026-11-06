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
    /// what a previous run left. Unreadable files count as absent. Tasks that
    /// were pending become `Interrupted` completions and the file is rewritten.
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
                    ..Default::default()
                };
                let now = Utc::now();
                for p in persisted.pending {
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
            return;
        }
        let path = dir.join(format!("{session}.json"));
        if s.running.is_empty() && s.inbox.is_empty() && s.in_flight.is_empty() {
            let _ = std::fs::remove_file(&path);
            return;
        }
        let body = Persisted {
            pending: s.running.iter().map(|r| r.task.clone()).collect(),
            inbox: s.in_flight.iter().chain(s.inbox.iter()).cloned().collect(),
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
        if !self.enabled() {
            bail!("background tasks are disabled (tools.background.max_per_session = 0)");
        }
        let mut map = self.lock();
        let s = map.entry(session.to_string()).or_default();
        if s.running.len() >= self.max_per_session {
            let held: Vec<&str> = s.running.iter().map(|r| r.task.id.as_str()).collect();
            bail!(
                "this session already has {} background tasks running ({}); \
                 wait for one to finish or cancel one with background_task",
                s.running.len(),
                held.join(", ")
            );
        }
        let id = format!("bg-{}", &Uuid::now_v7().simple().to_string()[..8]);
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
    pub(crate) fn cancel(&self, session: &str, id: &str) -> Result<()> {
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
        Self::cancel_running(s, r);
        self.persist(session, s);
        Ok(())
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
    pub(crate) fn cancel_all(&self, session: &str) {
        let removed = self.lock().remove(session);
        if let Some(s) = removed {
            for r in s.running {
                if let Some(a) = r.abort {
                    a.abort();
                }
            }
        }
        if let Some(dir) = &self.dir
            && check_file_stem(session).is_ok()
        {
            let _ = std::fs::remove_file(dir.join(format!("{session}.json")));
        }
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
    let TaskKind::Subagent { handle, .. } = &c.kind;
    cache.is_some_and(|cache| cache.get(handle).is_some())
}

/// Text shown to the model for one completion. An unfinished one offers
/// its handle for `resume` only when `resumable` (see [`resumable`]):
/// pointing the model at a handle with no stored history invites a resume
/// that can only fail.
pub(crate) fn render(c: &Completion, resumable: bool) -> String {
    let TaskKind::Subagent { agent, handle } = &c.kind;
    let verb = match &c.outcome {
        Outcome::Finished { .. } => "finished",
        Outcome::Failed { .. } => "failed",
        Outcome::Interrupted => "was interrupted by a restart",
        Outcome::Cancelled => "was cancelled",
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
        Outcome::Interrupted | Outcome::Cancelled => {}
    }
    if resumable && !matches!(c.outcome, Outcome::Finished { .. }) {
        out.push_str(&format!(
            "\nTo continue its work, pass handle {handle} as `resume` to the subagent tool."
        ));
    }
    out
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
}
