//! Background shell commands on the agent's own machine.
//!
//! [`start_on_agent`] spawns the command in a process group of its own and
//! hands a watcher task to the background layer. The watcher keeps the
//! output's tail in a [`TailBuffer`], which [`ShellRuns`] exposes for
//! peeking while the command runs, and completes the task with
//! [`Outcome::Exited`] when the command exits or reaches its run time.
//!
//! Whatever ends the watcher — the command exiting, its run time, or the
//! layer aborting it on `cancel` / `cancel_all` — its [`GroupGuard`] kills
//! the whole process group on the way out, so nothing the command started
//! outlives it.
#![allow(dead_code)] // Tasks 3-5 wire this into the shell tool.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Child;
use tokio::task::AbortHandle;

use crate::background::{BackgroundTasks, Outcome, TaskId};
use crate::tools::OUTPUT_CAP_BYTES;
use crate::tools::acp_client::{AcpClient, TerminalHandle};
use crate::tools::builtin_tools::push_command_line;

/// Put before the text once the head of the output had to be dropped.
const DROPPED_MARKER: &str = "[… earlier output dropped]\n";

/// How long the watcher waits for the output readers once the command has
/// exited. They end when the pipes close, which killing the group ensures
/// unless something escaped it.
const READER_GRACE: Duration = Duration::from_secs(2);

/// The last `cap` bytes of a command's output.
pub(crate) struct TailBuffer {
    cap: usize,
    bytes: VecDeque<u8>,
    dropped: bool,
}

impl TailBuffer {
    pub(crate) fn new(cap: usize) -> Self {
        Self {
            cap,
            bytes: VecDeque::new(),
            dropped: false,
        }
    }

    pub(crate) fn push(&mut self, bytes: &[u8]) {
        self.bytes.extend(bytes);
        if self.bytes.len() > self.cap {
            let excess = self.bytes.len() - self.cap;
            self.bytes.drain(..excess);
            self.dropped = true;
        }
    }

    /// The tail as text: lossy UTF-8, with a cut head moved forward past
    /// any continuation bytes so it starts on a character, and the marker
    /// in front once anything was dropped.
    pub(crate) fn text(&self) -> String {
        let (a, b) = self.bytes.as_slices();
        let mut all = Vec::with_capacity(a.len() + b.len());
        all.extend_from_slice(a);
        all.extend_from_slice(b);
        if !self.dropped {
            return String::from_utf8_lossy(&all).into_owned();
        }
        let start = all
            .iter()
            .position(|&byte| byte & 0b1100_0000 != 0b1000_0000)
            .unwrap_or(all.len());
        format!("{DROPPED_MARKER}{}", String::from_utf8_lossy(&all[start..]))
    }
}

/// A background command that is still running, as far as peeking is
/// concerned.
#[derive(Clone)]
pub(crate) enum LiveRun {
    /// On the agent's machine: the watcher fills this buffer.
    Agent(Arc<Mutex<TailBuffer>>),
    /// On the editor's machine: the editor holds the output.
    Editor {
        client: Arc<dyn AcpClient>,
        terminal: TerminalHandle,
    },
}

/// The background commands still running, by task id.
#[derive(Default)]
pub(crate) struct ShellRuns {
    runs: Mutex<HashMap<TaskId, LiveRun>>,
}

impl ShellRuns {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<TaskId, LiveRun>> {
        self.runs.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(crate) fn insert(&self, id: TaskId, run: LiveRun) {
        self.lock().insert(id, run);
    }

    pub(crate) fn remove(&self, id: &str) {
        self.lock().remove(id);
    }

    /// The output so far of the running command `id`; `None` when no such
    /// command is running.
    pub(crate) async fn peek(&self, id: &str) -> Option<anyhow::Result<String>> {
        // Cloned out so the lock is not held across the editor's answer.
        let run = self.lock().get(id).cloned()?;
        Some(match run {
            LiveRun::Agent(buf) => Ok(buf.lock().unwrap_or_else(|e| e.into_inner()).text()),
            LiveRun::Editor { client, terminal } => client
                .terminal_output(&terminal)
                .await
                .map(|out| out.output),
        })
    }
}

/// A command to run on a host, resolved by the caller.
pub(crate) struct HostCommand {
    pub command: String,
    pub shell: String,
    pub workdir: PathBuf,
    pub run_time: Duration,
}

/// Owns the child and stops what the command started when the watcher
/// ends, however it ends.
///
/// `Drop` kills the process group (unix) before the `Child` itself drops,
/// while the unreaped leader still holds the group id, so the signal cannot
/// reach an unrelated group that reused it. The `Child`'s `kill_on_drop`
/// then kills the leader (the only thing it kills on Windows) and tokio
/// reaps it in the background. Dropping also aborts the output readers and
/// takes the run out of [`ShellRuns`].
///
/// After a normal exit the leader is already reaped when the group is
/// killed (to take down what it left behind). If nothing of the group is
/// left by then its id is free, and in that window — the next few
/// instructions — another group could in principle take it; pid
/// allocation makes that a wraparound-only case, and it is accepted.
struct GroupGuard {
    pgid: Option<i32>,
    child: Child,
    readers: Vec<AbortHandle>,
    runs: Arc<ShellRuns>,
    id: TaskId,
}

impl GroupGuard {
    /// Kill the group now, once: later calls (and `Drop`) do nothing.
    fn kill_group(&mut self) {
        #[cfg(unix)]
        if let Some(pgid) = self.pgid.take() {
            // SAFETY: killpg only sends a signal; ESRCH (the group is
            // already gone) is the expected failure and is ignored.
            unsafe {
                libc::killpg(pgid, libc::SIGKILL);
            }
        }
        #[cfg(not(unix))]
        {
            self.pgid = None;
            let _ = self.child.start_kill();
        }
    }
}

impl Drop for GroupGuard {
    fn drop(&mut self) {
        self.kill_group();
        for r in &self.readers {
            r.abort();
        }
        self.runs.remove(&self.id);
    }
}

async fn pump(mut from: impl AsyncRead + Unpin, into: Arc<Mutex<TailBuffer>>) {
    let mut chunk = [0u8; 8192];
    loop {
        match from.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => into
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(&chunk[..n]),
        }
    }
}

/// Start `cmd` on the agent's machine as the registered task `id` of
/// `session`. A spawn failure is returned here (the caller then
/// `discard`s the task); everything after it ends in the task's
/// completion. Must be called inside a tokio runtime.
pub(crate) fn start_on_agent(
    cmd: HostCommand,
    background: Arc<BackgroundTasks>,
    runs: Arc<ShellRuns>,
    session: String,
    id: TaskId,
) -> anyhow::Result<()> {
    let mut command = tokio::process::Command::new(&cmd.shell);
    push_command_line(&mut command, &cmd.shell, &cmd.command);
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .current_dir(&cmd.workdir)
        .kill_on_drop(true);
    // A group of its own, so the guard can stop what the command starts.
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command
        .spawn()
        .with_context(|| format!("Failed to spawn shell '{}'", cmd.shell))?;

    let buf = Arc::new(Mutex::new(TailBuffer::new(OUTPUT_CAP_BYTES)));
    let mut readers = Vec::new();
    if let Some(out) = child.stdout.take() {
        readers.push(tokio::spawn(pump(out, Arc::clone(&buf))));
    }
    if let Some(err) = child.stderr.take() {
        readers.push(tokio::spawn(pump(err, Arc::clone(&buf))));
    }
    runs.insert(id.clone(), LiveRun::Agent(Arc::clone(&buf)));

    let mut guard = GroupGuard {
        pgid: child.id().and_then(|p| i32::try_from(p).ok()),
        child,
        readers: readers.iter().map(|r| r.abort_handle()).collect(),
        runs,
        id: id.clone(),
    };
    let run_time = cmd.run_time;
    let task_background = Arc::clone(&background);
    let task_session = session.clone();
    let task_id = id.clone();
    let watcher = async move {
        let (status, timed_out) = match tokio::time::timeout(run_time, guard.child.wait()).await {
            Ok(status) => (status.ok(), false),
            Err(_) => {
                guard.kill_group();
                // The leader is in the group already; this only makes
                // sure of it.
                let _ = guard.child.start_kill();
                (guard.child.wait().await.ok(), true)
            }
        };
        // Whatever the command left running goes too, which also closes
        // the pipes it held so the readers end.
        guard.kill_group();
        for r in readers {
            let _ = tokio::time::timeout(READER_GRACE, r).await;
        }
        let output = buf.lock().unwrap_or_else(|e| e.into_inner()).text();
        let exit_code = if timed_out {
            None
        } else {
            status.and_then(|s| s.code())
        };
        // Out of `ShellRuns` before the completion lands.
        drop(guard);
        task_background.complete(
            &task_session,
            &task_id,
            Outcome::Exited {
                exit_code,
                output,
                timed_out,
            },
        );
    };
    let task = tokio::spawn(watcher);
    background.attach(&session, &id, task.abort_handle());
    // A cancel that landed between `register` and `attach` found no handle
    // to abort; abort it now. If the task completed instead, this is a
    // no-op on a finished task.
    if !background.pending(&session).iter().any(|t| t.id == id) {
        task.abort();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const MARKER: &str = "[… earlier output dropped]\n";

    #[test]
    fn tail_buffer_keeps_the_tail_and_marks_the_cut() {
        let mut b = TailBuffer::new(10);
        b.push(b"0123456789abcdef");
        assert_eq!(b.text(), "[… earlier output dropped]\n6789abcdef");
    }

    #[test]
    fn tail_buffer_cuts_at_a_char_boundary() {
        // "aaaé€" is a a a C3 A9 E2 82 AC: cap 4 and 6 cut inside a
        // character, cap 5 on a boundary.
        let input = "aaaé€";
        for cap in [5, 4, 6] {
            let mut b = TailBuffer::new(cap);
            b.push(input.as_bytes());
            let text = b.text();
            let rest = text.strip_prefix(MARKER).expect("marked");
            assert!(!rest.contains('\u{FFFD}'), "cap {cap}: {rest:?}");
            assert!(input.ends_with(rest), "cap {cap}: {rest:?}");
        }
    }

    #[test]
    fn tail_buffer_under_cap_is_unmarked() {
        let mut b = TailBuffer::new(100);
        b.push(b"hi");
        assert_eq!(b.text(), "hi");
    }

    #[cfg(unix)]
    mod on_agent {
        use super::*;
        use crate::background::{Completion, ShellHost, TaskKind};
        use std::path::Path;
        use std::time::Instant;

        const SESSION: &str = "s";

        struct Started {
            background: Arc<BackgroundTasks>,
            runs: Arc<ShellRuns>,
            id: TaskId,
        }

        fn start(command: &str, workdir: &Path, run_time: Duration) -> anyhow::Result<Started> {
            let background = BackgroundTasks::in_memory(4);
            let runs = Arc::new(ShellRuns::new());
            let id = background.register(
                SESSION,
                TaskKind::Shell {
                    command: command.into(),
                    on: ShellHost::Agent,
                    timeout_secs: run_time.as_secs(),
                },
                command.into(),
            )?;
            start_on_agent(
                HostCommand {
                    command: command.into(),
                    shell: "/bin/sh".into(),
                    workdir: workdir.into(),
                    run_time,
                },
                Arc::clone(&background),
                Arc::clone(&runs),
                SESSION.into(),
                id.clone(),
            )?;
            Ok(Started {
                background,
                runs,
                id,
            })
        }

        /// Poll `cond` every 20 ms for up to `bound`.
        async fn wait_until(bound: Duration, mut cond: impl FnMut() -> bool) -> bool {
            let deadline = Instant::now() + bound;
            while Instant::now() < deadline {
                if cond() {
                    return true;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            cond()
        }

        async fn completion(s: &Started) -> Completion {
            assert!(
                wait_until(Duration::from_secs(10), || s.background.has_inbox(SESSION)).await,
                "no completion within 10 s"
            );
            let mut drained = s.background.drain(SESSION);
            assert_eq!(drained.len(), 1, "{drained:?}");
            drained.remove(0)
        }

        #[tokio::test]
        async fn an_exit_delivers_the_output() {
            let dir = tempfile::tempdir().unwrap();
            let s = start(
                "echo out; echo err >&2; exit 3",
                dir.path(),
                Duration::from_secs(60),
            )
            .unwrap();
            let c = completion(&s).await;
            assert_eq!(c.task_id, s.id);
            match c.outcome {
                Outcome::Exited {
                    exit_code: Some(3),
                    timed_out: false,
                    output,
                } => {
                    assert!(output.contains("out"), "{output:?}");
                    assert!(output.contains("err"), "{output:?}");
                }
                other => panic!("{other:?}"),
            }
            assert!(s.runs.peek(&s.id).await.is_none());
        }

        #[tokio::test]
        async fn run_time_kills_and_reports() {
            let dir = tempfile::tempdir().unwrap();
            let s = start("sleep 30", dir.path(), Duration::from_secs(1)).unwrap();
            let c = completion(&s).await;
            assert!(
                matches!(
                    c.outcome,
                    Outcome::Exited {
                        timed_out: true,
                        exit_code: None,
                        ..
                    }
                ),
                "{:?}",
                c.outcome
            );
        }

        fn alive(pid: i32) -> bool {
            // SAFETY: signal 0 only checks whether the process exists.
            unsafe { libc::kill(pid, 0) == 0 }
        }

        #[tokio::test]
        async fn cancel_kills_the_process_group() {
            let dir = tempfile::tempdir().unwrap();
            let pid_file = dir.path().join("pid");
            let command = format!("sleep 300 & echo $! > {}; wait", pid_file.display());
            let s = start(&command, dir.path(), Duration::from_secs(600)).unwrap();
            let read_pid = || {
                std::fs::read_to_string(&pid_file)
                    .ok()
                    .and_then(|t| t.trim().parse::<i32>().ok())
            };
            assert!(
                wait_until(Duration::from_secs(10), || read_pid().is_some()).await,
                "the pid file never appeared"
            );
            let pid = read_pid().unwrap();
            assert!(alive(pid));
            s.background.cancel(SESSION, &s.id).unwrap();
            // The grandchild stays a zombie until its parent, killed with
            // it, is reaped and init adopts and reaps it; `kill(pid, 0)`
            // succeeds on a zombie, hence the wait.
            assert!(
                wait_until(Duration::from_secs(5), || !alive(pid)).await,
                "the grandchild {pid} survived the cancel"
            );
            assert!(s.runs.peek(&s.id).await.is_none());
        }

        #[tokio::test]
        async fn peek_returns_output_so_far() {
            let dir = tempfile::tempdir().unwrap();
            let s = start("echo first; sleep 30", dir.path(), Duration::from_secs(60)).unwrap();
            let mut text = String::new();
            let deadline = Instant::now() + Duration::from_secs(10);
            while Instant::now() < deadline {
                match s.runs.peek(&s.id).await {
                    Some(Ok(t)) if t.contains("first") => {
                        text = t;
                        break;
                    }
                    Some(Ok(_)) => {}
                    other => panic!("{other:?}"),
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            assert!(text.contains("first"), "{text:?}");
            assert!(!s.background.has_inbox(SESSION), "it should still run");
            s.background.cancel(SESSION, &s.id).unwrap();
        }

        #[tokio::test]
        async fn a_spawn_failure_is_returned() {
            let dir = tempfile::tempdir().unwrap();
            let missing = dir.path().join("nope");
            assert!(start("true", &missing, Duration::from_secs(60)).is_err());
        }
    }
}
