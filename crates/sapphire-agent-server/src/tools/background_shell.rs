//! Background shell commands, on the agent's own machine or the editor's.
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
//!
//! [`start_on_editor`] runs the command in an ACP terminal instead, which
//! counts toward the session's terminal cap. Its watcher waits for the
//! editor to report the exit, and its [`EditorGuard`] kills and releases
//! the terminal when the watcher ends any other way.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Child;
use tokio::task::AbortHandle;
use tracing::warn;

use crate::background::{BackgroundTasks, Outcome, TaskId};
use crate::tools::OUTPUT_CAP_BYTES;
use crate::tools::acp_client::{AcpClient, TerminalHandle, TerminalOutput};
use crate::tools::builtin_tools::push_command_line;
use crate::tools::client_exec::CLIENT_SHELL;
use crate::tools::client_tools::cap_error;

/// Put before the text once the head of the output had to be dropped.
const DROPPED_MARKER: &str = "[… earlier output dropped]\n";

/// How long the watcher waits for the output readers once the command has
/// exited. They end when the pipes close, which killing the group ensures
/// unless something escaped it.
const READER_GRACE: Duration = Duration::from_secs(2);

/// How long one editor call after the wait — kill, output, release, or a
/// peek — may take. An editor that is connected but no longer answering
/// must not leave the task pending forever. Short in tests, so the
/// hung-editor tests stay fast.
#[cfg(not(test))]
const EDITOR_RPC_TIMEOUT: Duration = Duration::from_secs(10);
#[cfg(test)]
const EDITOR_RPC_TIMEOUT: Duration = Duration::from_millis(200);

/// `call`, or an error once [`EDITOR_RPC_TIMEOUT`] passes without an answer.
async fn bounded<T>(
    call: impl std::future::Future<Output = anyhow::Result<T>>,
) -> anyhow::Result<T> {
    tokio::time::timeout(EDITOR_RPC_TIMEOUT, call)
        .await
        .unwrap_or_else(|_| {
            Err(anyhow::anyhow!(
                "the editor did not answer within {EDITOR_RPC_TIMEOUT:?}"
            ))
        })
}

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
    /// On the agent's machine: the watcher fills `output`. `pgid` is the
    /// command's process group (unix; `None` elsewhere), for
    /// [`ShellRuns::stop_all`].
    Agent {
        output: Arc<Mutex<TailBuffer>>,
        pgid: Option<i32>,
    },
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

    /// Kill the process group of every command still running on the
    /// agent's machine, for a shutdown or restart that exits the process
    /// while the runtime is alive: no watcher is dropped then, so no
    /// [`GroupGuard`] kills its group, and each command, in a group of its
    /// own, would outlive the agent. Editor terminals are left to the
    /// editor, which releases them with the session.
    ///
    /// A run leaves the map only when its guard drops, so a group whose
    /// leader was just reaped can still be listed for a moment; the same
    /// wraparound-only pgid reuse window [`GroupGuard`] accepts.
    pub(crate) fn stop_all(&self) {
        for run in self.lock().values() {
            if let LiveRun::Agent {
                pgid: Some(pgid), ..
            } = run
            {
                #[cfg(unix)]
                // SAFETY: killpg only sends a signal; ESRCH (the group is
                // already gone) is the expected failure and is ignored.
                unsafe {
                    libc::killpg(*pgid, libc::SIGKILL);
                }
                #[cfg(not(unix))]
                let _ = pgid;
            }
        }
    }

    /// The output so far of the running command `id`; `None` when no such
    /// command is running.
    pub(crate) async fn peek(&self, id: &str) -> Option<anyhow::Result<String>> {
        // Cloned out so the lock is not held across the editor's answer.
        let run = self.lock().get(id).cloned()?;
        Some(match run {
            LiveRun::Agent { output, .. } => {
                Ok(output.lock().unwrap_or_else(|e| e.into_inner()).text())
            }
            LiveRun::Editor { client, terminal } => bounded(client.terminal_output(&terminal))
                .await
                .map(editor_text),
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
    // With `process_group(0)` the group id is the leader's pid.
    let pgid = if cfg!(unix) {
        child.id().and_then(|p| i32::try_from(p).ok())
    } else {
        None
    };
    runs.insert(
        id.clone(),
        LiveRun::Agent {
            output: Arc::clone(&buf),
            pgid,
        },
    );

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
        // One deadline for all of them, not a grace period each.
        let deadline = tokio::time::Instant::now() + READER_GRACE;
        for r in readers {
            let _ = tokio::time::timeout_at(deadline, r).await;
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
    spawn_and_attach(watcher, &background, &session, &id);
    Ok(())
}

/// Spawn `watcher` and hand it to the layer as task `id`'s.
fn spawn_and_attach(
    watcher: impl std::future::Future<Output = ()> + Send + 'static,
    background: &BackgroundTasks,
    session: &str,
    id: &TaskId,
) {
    let task = tokio::spawn(watcher);
    background.attach(session, id, task.abort_handle());
    // A cancel that landed between `register` and `attach` found no handle
    // to abort; abort it now. If the task completed instead, this is a
    // no-op on a finished task.
    if !background.pending(session).iter().any(|t| &t.id == id) {
        task.abort();
    }
}

/// The editor's output as text, marked like [`TailBuffer::text`] when the
/// editor cut its head to stay under the byte limit.
fn editor_text(out: TerminalOutput) -> String {
    if out.truncated {
        format!("{DROPPED_MARKER}{}", out.output)
    } else {
        out.output
    }
}

/// Release `terminal` and stop counting it against the session's cap.
/// The untrack happens even when the release fails or times out: a
/// terminal the editor no longer answers for must not hold a slot
/// forever, and the editor releases its terminals with the session anyway.
async fn release_and_untrack(client: &dyn AcpClient, terminal: &TerminalHandle) {
    if let Err(e) = bounded(client.release_terminal(terminal)).await {
        warn!("background command: releasing terminal {terminal} failed: {e:#}");
    }
    client.untrack_terminal(terminal).await;
}

/// Kill the command in `terminal`; a failure or time-out is only logged.
async fn kill(client: &dyn AcpClient, terminal: &TerminalHandle) {
    if let Err(e) = bounded(client.kill_terminal(terminal)).await {
        warn!("background command: killing terminal {terminal} failed: {e:#}");
    }
}

/// Stops the editor's terminal when the watcher ends without doing so
/// itself — the layer aborting it on `cancel` / `cancel_all`, the runtime
/// dropping it, or the editor failing to answer the wait.
///
/// `Drop` cannot await, so while armed it spawns the kill, release and
/// untrack. The watcher's own exit and time-out paths make those calls
/// inline and disarm it. Either way it takes the run out of [`ShellRuns`].
struct EditorGuard {
    client: Arc<dyn AcpClient>,
    terminal: TerminalHandle,
    armed: bool,
    runs: Arc<ShellRuns>,
    id: TaskId,
}

impl EditorGuard {
    /// The watcher stops the terminal itself from here on; the run is no
    /// longer peekable, since the handle is about to be released.
    fn disarm(&mut self) {
        self.armed = false;
        self.runs.remove(&self.id);
    }
}

impl Drop for EditorGuard {
    fn drop(&mut self) {
        self.runs.remove(&self.id);
        if !self.armed {
            return;
        }
        let client = Arc::clone(&self.client);
        let terminal = self.terminal.clone();
        match tokio::runtime::Handle::try_current() {
            Ok(rt) => {
                rt.spawn(async move {
                    kill(client.as_ref(), &terminal).await;
                    release_and_untrack(client.as_ref(), &terminal).await;
                });
            }
            Err(_) => warn!(
                "background command: no runtime to stop terminal {} with",
                self.terminal
            ),
        }
    }
}

/// Start `command` on the editor's machine through `client` as the
/// registered task `id` of `session`. The terminal is reserved, created
/// and tracked before this returns, so the cap refusal and a create error
/// are returned here (the caller then `discard`s the task); everything
/// after it ends in the task's completion.
///
/// `client` is taken by the caller from the task-local scope, which does
/// not reach the spawned watcher.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn start_on_editor(
    client: Arc<dyn AcpClient>,
    command: String,
    workdir: Option<String>,
    run_time: Duration,
    background: Arc<BackgroundTasks>,
    runs: Arc<ShellRuns>,
    session: String,
    id: TaskId,
) -> anyhow::Result<()> {
    // As in `run_client_command`: reserve atomically, and a dropped
    // reservation (create failing or this call cancelled) frees itself.
    let reservation = client
        .try_reserve_terminal_slot()
        .await
        .map_err(|held| cap_error(&held))?;
    let args = ["-c".to_string(), command];
    let terminal = client
        .create_terminal(
            CLIENT_SHELL,
            &args,
            workdir.as_deref(),
            Some(OUTPUT_CAP_BYTES as u64),
        )
        .await?;
    client.track_terminal(reservation, terminal.clone()).await;
    runs.insert(
        id.clone(),
        LiveRun::Editor {
            client: Arc::clone(&client),
            terminal: terminal.clone(),
        },
    );

    let mut guard = EditorGuard {
        client,
        terminal,
        armed: true,
        runs,
        id: id.clone(),
    };
    let task_background = Arc::clone(&background);
    let task_session = session.clone();
    let task_id = id.clone();
    let watcher = async move {
        let client = Arc::clone(&guard.client);
        let terminal = guard.terminal.clone();
        let waited = tokio::time::timeout(run_time, client.wait_for_terminal_exit(&terminal)).await;
        let (status, timed_out) = match waited {
            Ok(Ok(status)) => (Some(status), false),
            Ok(Err(e)) => {
                // Still armed: the guard tries to stop the terminal in
                // the background, so an editor that no longer answers
                // does not hold up the completion.
                drop(guard);
                task_background.complete(
                    &task_session,
                    &task_id,
                    Outcome::Failed {
                        message: format!(
                            "the editor stopped answering while the command ran: {e:#}"
                        ),
                    },
                );
                return;
            }
            Err(_elapsed) => {
                kill(client.as_ref(), &terminal).await;
                (None, true)
            }
        };
        let output = match bounded(client.terminal_output(&terminal)).await {
            Ok(out) => editor_text(out),
            Err(e) => format!("[the editor did not return the output: {e:#}]"),
        };
        guard.disarm();
        drop(guard);
        // On a task of its own so a `cancel` landing now, which aborts
        // this watcher, cannot cut it off between release and untrack.
        let _ = tokio::spawn(async move {
            release_and_untrack(client.as_ref(), &terminal).await;
        })
        .await;
        let exit_code = status
            .and_then(|s| s.exit_code)
            .and_then(|c| i32::try_from(c).ok());
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
    spawn_and_attach(watcher, &background, &session, &id);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const MARKER: &str = "[… earlier output dropped]\n";

    /// Poll `cond` every 20 ms for up to `bound`.
    async fn wait_until(bound: Duration, mut cond: impl FnMut() -> bool) -> bool {
        let deadline = std::time::Instant::now() + bound;
        while std::time::Instant::now() < deadline {
            if cond() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        cond()
    }

    #[test]
    fn tail_buffer_keeps_the_tail_and_marks_the_cut() {
        let mut b = TailBuffer::new(10);
        b.push(b"0123456789abcdef");
        assert_eq!(b.text(), "[… earlier output dropped]\n6789abcdef");
    }

    #[test]
    fn tail_buffer_cuts_at_a_char_boundary() {
        // "aaaé€" is a a a C3 A9 E2 82 AC: cap 4 (A9 …) and cap 2
        // (82 AC) cut inside a character, cap 3 on a boundary.
        let input = "aaaé€";
        for cap in [3, 4, 2] {
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
        async fn stop_all_kills_every_process_group() {
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
            // As on shutdown: nothing aborts the watcher, only the kill.
            s.runs.stop_all();
            assert!(
                wait_until(Duration::from_secs(5), || !alive(pid)).await,
                "the grandchild {pid} survived stop_all"
            );
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

    mod on_editor {
        use super::*;
        use crate::background::{Completion, ShellHost, TaskKind};
        use crate::tools::acp_client::tests::FakeClient;
        use crate::tools::client_tools::MAX_TERMINALS_PER_SESSION;

        const SESSION: &str = "s";

        struct Started {
            background: Arc<BackgroundTasks>,
            runs: Arc<ShellRuns>,
            fake: Arc<FakeClient>,
            id: TaskId,
        }

        async fn start(fake: Arc<FakeClient>, run_time: Duration) -> anyhow::Result<Started> {
            let background = BackgroundTasks::in_memory(4);
            let runs = Arc::new(ShellRuns::new());
            let command = "make";
            let id = background.register(
                SESSION,
                TaskKind::Shell {
                    command: command.into(),
                    on: ShellHost::Editor,
                    timeout_secs: run_time.as_secs(),
                },
                command.into(),
            )?;
            let client: Arc<dyn AcpClient> = Arc::clone(&fake) as Arc<dyn AcpClient>;
            start_on_editor(
                client,
                command.into(),
                Some("/work".into()),
                run_time,
                Arc::clone(&background),
                Arc::clone(&runs),
                SESSION.into(),
                id.clone(),
            )
            .await?;
            Ok(Started {
                background,
                runs,
                fake,
                id,
            })
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

        fn released(fake: &FakeClient) -> usize {
            fake.released.lock().unwrap().len()
        }

        fn killed(fake: &FakeClient) -> usize {
            fake.killed.lock().unwrap().len()
        }

        #[tokio::test]
        async fn an_editor_exit_delivers_and_releases() {
            let fake = Arc::new(FakeClient::default());
            fake.queue_terminal_result("built", Some(0));
            let s = start(Arc::clone(&fake), Duration::from_secs(60))
                .await
                .unwrap();
            let c = completion(&s).await;
            assert_eq!(c.task_id, s.id);
            match c.outcome {
                Outcome::Exited {
                    exit_code: Some(0),
                    timed_out: false,
                    output,
                } => assert!(output.contains("built"), "{output:?}"),
                other => panic!("{other:?}"),
            }
            assert_eq!(
                fake.last_terminal_command().unwrap(),
                vec!["bash".to_string(), "-c".into(), "make".into()]
            );
            assert_eq!(released(&s.fake), 1);
            assert_eq!(s.fake.tracked_count(), 0);
            assert!(s.runs.peek(&s.id).await.is_none());
        }

        #[tokio::test]
        async fn time_out_kills_and_releases_the_terminal() {
            let fake = Arc::new(FakeClient::default());
            fake.make_exit_never_return();
            let s = start(Arc::clone(&fake), Duration::from_secs(1))
                .await
                .unwrap();
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
            assert_eq!(killed(&fake), 1);
            assert_eq!(released(&fake), 1);
            assert_eq!(fake.tracked_count(), 0);
        }

        #[tokio::test]
        async fn cancel_kills_and_releases() {
            let fake = Arc::new(FakeClient::default());
            fake.make_exit_never_return();
            let s = start(Arc::clone(&fake), Duration::from_secs(600))
                .await
                .unwrap();
            assert_eq!(fake.tracked_count(), 1);
            s.background.cancel(SESSION, &s.id).unwrap();
            assert!(
                wait_until(Duration::from_secs(5), || {
                    killed(&fake) == 1 && released(&fake) == 1 && fake.tracked_count() == 0
                })
                .await,
                "killed {}, released {}, tracked {}",
                killed(&fake),
                released(&fake),
                fake.tracked_count()
            );
            assert!(s.runs.peek(&s.id).await.is_none());
        }

        #[tokio::test]
        async fn a_failed_wait_is_failed() {
            let fake = Arc::new(FakeClient::default());
            fake.make_wait_fail_with("gone");
            let s = start(Arc::clone(&fake), Duration::from_secs(60))
                .await
                .unwrap();
            match completion(&s).await.outcome {
                Outcome::Failed { message } => assert_eq!(
                    message,
                    "the editor stopped answering while the command ran: gone"
                ),
                other => panic!("{other:?}"),
            }
        }

        #[tokio::test]
        async fn the_terminal_cap_refuses_before_spawning() {
            let fake = Arc::new(FakeClient::default());
            fake.hand_out_distinct_handles();
            for _ in 0..MAX_TERMINALS_PER_SESSION {
                let reservation = fake.try_reserve_terminal_slot().await.ok().unwrap();
                let handle = fake.create_terminal("x", &[], None, None).await.unwrap();
                fake.track_terminal(reservation, handle).await;
            }
            assert_eq!(fake.tracked_count(), MAX_TERMINALS_PER_SESSION);
            let err = start(Arc::clone(&fake), Duration::from_secs(60))
                .await
                .err()
                .expect("the cap refuses");
            assert!(err.to_string().contains("maximum"), "{err}");
            assert_eq!(fake.tracked_count(), MAX_TERMINALS_PER_SESSION);
            assert_eq!(fake.terminal_count(), MAX_TERMINALS_PER_SESSION);
        }

        #[tokio::test]
        async fn a_hung_editor_still_completes_and_frees_the_slot() {
            let fake = Arc::new(FakeClient::default());
            fake.make_exit_never_return();
            fake.make_editor_stop_answering();
            let s = start(Arc::clone(&fake), Duration::from_secs(1))
                .await
                .unwrap();
            match completion(&s).await.outcome {
                Outcome::Exited {
                    timed_out: true,
                    exit_code: None,
                    output,
                } => assert!(
                    output.starts_with("[the editor did not return the output: "),
                    "{output:?}"
                ),
                other => panic!("{other:?}"),
            }
            assert_eq!(killed(&fake), 1);
            assert_eq!(released(&fake), 1);
            assert_eq!(fake.tracked_count(), 0);
        }

        #[tokio::test]
        async fn a_cancel_on_a_hung_editor_frees_the_slot() {
            let fake = Arc::new(FakeClient::default());
            fake.make_exit_never_return();
            fake.make_editor_stop_answering();
            let s = start(Arc::clone(&fake), Duration::from_secs(600))
                .await
                .unwrap();
            s.background.cancel(SESSION, &s.id).unwrap();
            assert!(
                wait_until(Duration::from_secs(5), || fake.tracked_count() == 0).await,
                "killed {}, released {}, tracked {}",
                killed(&fake),
                released(&fake),
                fake.tracked_count()
            );
        }

        #[tokio::test]
        async fn peek_on_a_hung_editor_is_an_error() {
            let fake = Arc::new(FakeClient::default());
            fake.make_exit_never_return();
            fake.make_editor_stop_answering();
            let s = start(Arc::clone(&fake), Duration::from_secs(600))
                .await
                .unwrap();
            let peeked = tokio::time::timeout(Duration::from_secs(5), s.runs.peek(&s.id))
                .await
                .expect("peek must not hang");
            assert!(matches!(peeked, Some(Err(_))), "{peeked:?}");
            s.background.cancel(SESSION, &s.id).unwrap();
        }

        #[tokio::test]
        async fn peek_reads_the_terminal() {
            let fake = Arc::new(FakeClient::default());
            fake.queue_terminal_stdout("partial");
            fake.make_exit_never_return();
            let s = start(Arc::clone(&fake), Duration::from_secs(600))
                .await
                .unwrap();
            match s.runs.peek(&s.id).await {
                Some(Ok(text)) => assert!(text.contains("partial"), "{text:?}"),
                other => panic!("{other:?}"),
            }
            s.background.cancel(SESSION, &s.id).unwrap();
        }
    }
}
