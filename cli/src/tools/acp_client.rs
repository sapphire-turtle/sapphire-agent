//! The agent's way of reaching the editor's machine.
//!
//! A tool's `execute` takes only its JSON input — it has no idea which
//! session called it, and the ACP connection lives per-connection in
//! `serve::acp`. Threading a session through `Tool` would touch every
//! tool for the benefit of six, so the handle is carried in a
//! `tokio::task_local` scoped around tool execution instead.
//!
//! `src/timer.rs` solves the same problem the same way: the turn loops
//! wrap `tools.execute(...)` in `scope_timer_origin` so the timer tool
//! can read where its call came from.
//!
//! This is a trait rather than the SDK's connection type so the tools
//! can be driven by a fake in tests, and so everything that knows about
//! ACP's wire types stays inside `serve::acp`.

use std::collections::HashMap;
use std::sync::Arc;

/// An opaque handle to a command running on the client's machine.
///
/// Opaque on purpose: the value is the client's, and nothing here may
/// parse or construct one except from what the client returned. The
/// inner `String` is public only so tests (the fake client) can build
/// one directly — a real handle is only ever the client's own value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalHandle(pub String);

impl std::fmt::Display for TerminalHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExitStatus {
    pub exit_code: Option<u32>,
    pub signal: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TerminalOutput {
    pub output: String,
    /// The client hit the byte limit we asked for and cut the output.
    pub truncated: bool,
    /// `None` while the command is still running.
    pub exit_status: Option<ExitStatus>,
}

/// The session-keyed store of terminals a session has started and not
/// yet released.
///
/// `Arc`-wrapped so it can be shared between `ServeState.acp_terminals`
/// (`src/serve/mod.rs`), the authoritative copy that outlives any one
/// connection, and whichever [`AcpClient`] is tracking against it —
/// `AcpClientHandle` (`src/serve/acp.rs`) for a real connection,
/// `FakeClient` below for tests. `client_tools::ClientShellStart`'s cap
/// check has to see every handle a session holds, including ones left
/// behind by a connection other than the one asking, without this
/// module depending on `ServeState` itself — hence the type living
/// here rather than in `serve`.
///
/// `std::sync::Mutex`, not `tokio::sync::Mutex`, and deliberately so:
/// [`TerminalReservation`]'s `Drop` frees a leaked reservation
/// synchronously, and `Drop` cannot `.await` a `tokio::sync::Mutex`.
/// The trade a sync mutex usually asks for — never held across an
/// `.await` — holds here: every critical section on this registry
/// (`try_reserve_terminal_slot`, `track_terminal`, `untrack_terminal`,
/// and `TerminalReservation`'s own `resolve`/`Drop`) is a short,
/// synchronous `HashMap`/`Vec` operation with no `.await` inside the
/// lock span.
pub(crate) type TerminalRegistry = Arc<std::sync::Mutex<HashMap<String, Vec<TerminalHandle>>>>;

/// What the editor can be asked to do on its own machine.
///
/// Every method maps 1:1 onto one ACP `agent → client` request. The
/// full set is exactly this — ACP has no directory listing, delete,
/// stat or rename, which is why there are no client-side tools for
/// those.
#[async_trait::async_trait]
pub trait AcpClient: Send + Sync {
    async fn read_text_file(
        &self,
        path: &str,
        line: Option<u32>,
        limit: Option<u32>,
    ) -> anyhow::Result<String>;

    async fn write_text_file(&self, path: &str, content: &str) -> anyhow::Result<()>;

    async fn create_terminal(
        &self,
        command: &str,
        args: &[String],
        cwd: Option<&str>,
        output_byte_limit: Option<u64>,
    ) -> anyhow::Result<TerminalHandle>;

    async fn terminal_output(&self, terminal: &TerminalHandle) -> anyhow::Result<TerminalOutput>;

    async fn wait_for_terminal_exit(&self, terminal: &TerminalHandle)
    -> anyhow::Result<ExitStatus>;

    /// Ends the command but keeps the handle usable, so the output can
    /// still be collected afterwards.
    async fn kill_terminal(&self, terminal: &TerminalHandle) -> anyhow::Result<()>;

    /// Frees the handle — **and kills the command if it is still
    /// running.** The ACP schema says so explicitly: `terminal/release`
    /// kills the command just like `terminal/kill` does, it just also
    /// invalidates the `TerminalId` afterwards. Nothing may call this
    /// just because a connection went away — releasing on a dropped
    /// socket would kill a user's build over a network blip.
    async fn release_terminal(&self, terminal: &TerminalHandle) -> anyhow::Result<()>;

    /// Atomically check this session's terminal cap and, if under it,
    /// reserve the slot — in one lock span, so the check and the
    /// reservation cannot be separated by another call.
    ///
    /// Replaces the old pattern of reading the session's currently held
    /// handles and deciding, separately, whether to call
    /// [`create_terminal`](AcpClient::create_terminal) based on what it
    /// saw: `run_llm_turn` executes a turn's permitted tool calls
    /// concurrently (`futures_util::future::join_all`), so one
    /// assistant message with N `shell_start` (or `shell`)
    /// calls had all N read the count before any of them wrote it back
    /// — the cap was bypassable within a single turn. Reserving
    /// atomically here closes that gap.
    ///
    /// On success, returns a [`TerminalReservation`] that holds the
    /// slot until [`track_terminal`] consumes it, resolving it into a
    /// real handle. If the caller instead drops the reservation without
    /// ever calling `track_terminal` — `create_terminal` fails, an
    /// early return, or the whole call is cancelled (the turn's future
    /// dropped mid-RPC, which ACP treats as routine: Escape in an
    /// editor, a dropped socket) — the reservation's own `Drop` frees
    /// the slot. Nothing else can leak it, which is the point: a
    /// cancelled `create_terminal` used to leave an un-freeable
    /// placeholder in the registry forever.
    ///
    /// On refusal, returns a [`CapHeld`] naming the handles currently
    /// held and counting any reservations still in flight (neither of
    /// which the caller can just drop into a `Vec<TerminalHandle>` —
    /// an in-flight reservation has no handle a model could act on)
    /// so the refusal message can still account for the full cap
    /// rather than silently omitting what it can't name.
    async fn try_reserve_terminal_slot(&self) -> Result<TerminalReservation, CapHeld>;

    /// Consume `reservation`, resolving it into `handle` so it counts
    /// against the cap as a real, trackable terminal until
    /// [`untrack_terminal`](AcpClient::untrack_terminal) drops it.
    ///
    /// Takes the reservation by value specifically so a resolved
    /// reservation cannot also be freed by its own `Drop` — see
    /// [`TerminalReservation::resolve`].
    async fn track_terminal(&self, reservation: TerminalReservation, handle: TerminalHandle);

    /// Drop `handle` from this client's session tracking.
    ///
    /// Not a release: called after a real `release_terminal`/
    /// `kill_terminal`+`release_terminal` succeeds, and also when the
    /// client reports the handle unknown — in that second case there
    /// is nothing left to release, only bookkeeping to correct so a
    /// handle the client has already forgotten stops counting against
    /// the cap.
    async fn untrack_terminal(&self, handle: &TerminalHandle);
}

/// What [`AcpClient::try_reserve_terminal_slot`] returns on refusal.
///
/// Split into named handles and a bare reservation count because an
/// in-flight reservation has no id yet for the model to act on (it
/// hasn't been handed a handle by `create_terminal`) — so it cannot
/// simply be added to `handles`. Dropping it from the refusal
/// silently instead would tell a model holding 8 terminals that it
/// holds only, say, 3: a shorter list than the cap it's told it's
/// hit, with no way to reconcile the two. [`cap_error`](
/// crate::tools::client_tools::cap_error) reports both.
pub(crate) struct CapHeld {
    pub(crate) handles: Vec<TerminalHandle>,
    pub(crate) reservations: usize,
}

/// Holds one session's terminal-cap slot from a successful
/// [`AcpClient::try_reserve_terminal_slot`] until
/// [`AcpClient::track_terminal`] resolves it into a real handle.
///
/// The fix this type exists for: the placeholder
/// `try_reserve_terminal_slot` used to push into the registry was
/// freed only by `track_terminal` or an explicit `untrack_terminal`
/// call on `create_terminal` failure — so if the call was cancelled
/// in between (the turn's future dropped mid-RPC, which is exactly
/// what a dropped socket or an Escape-to-cancel produces), neither
/// ever ran and the placeholder stayed in the registry forever,
/// permanently costing the session one of its 8 terminal slots.
///
/// Making the reservation a guard closes that: `Drop` frees the slot
/// unless [`resolve`](Self::resolve) has already consumed it, so
/// *however* the reservation stops being held — resolved, explicitly
/// dropped, or the enclosing future simply going away — the slot is
/// accounted for exactly once.
pub(crate) struct TerminalReservation {
    registry: TerminalRegistry,
    session_key: String,
    /// Set by [`resolve`](Self::resolve) so `Drop` can tell "already
    /// turned into a real handle" apart from "still just a
    /// placeholder, free it." Without this, `Drop` would run *after*
    /// `resolve` too (dropping `self` is unconditional, not something
    /// `resolve` moving fields out of `self` skips) and remove the
    /// real handle `resolve` just installed.
    resolved: bool,
}

impl TerminalReservation {
    pub(crate) fn new(registry: TerminalRegistry, session_key: String) -> Self {
        Self {
            registry,
            session_key,
            resolved: false,
        }
    }

    /// Swap this reservation's placeholder for `handle` in one lock
    /// span, and mark it resolved so `Drop` leaves the result alone.
    ///
    /// Consumes `self` (rather than taking `&mut self`) so a caller
    /// cannot accidentally resolve the same reservation twice or use
    /// it again after resolving — the type system, not just the
    /// `resolved` flag, rules that out.
    pub(crate) fn resolve(mut self, handle: TerminalHandle) {
        let mut held = self.registry.lock().unwrap();
        let entry = held.entry(self.session_key.clone()).or_default();
        if let Some(pos) = entry.iter().position(|h| h.0 == RESERVED_TERMINAL_MARKER) {
            entry.remove(pos);
        }
        entry.push(handle);
        self.resolved = true;
    }
}

impl Drop for TerminalReservation {
    fn drop(&mut self) {
        if self.resolved {
            return;
        }
        // Not `self.registry.lock().await` — see `TerminalRegistry`'s
        // doc for why this registry is a `std::sync::Mutex`: `Drop`
        // cannot `.await`, and this is the whole reason the reservation
        // can free itself from here regardless of *how* it stopped
        // being held.
        let mut held = self.registry.lock().unwrap();
        if let Some(entry) = held.get_mut(&self.session_key) {
            if let Some(pos) = entry.iter().position(|h| h.0 == RESERVED_TERMINAL_MARKER) {
                entry.remove(pos);
            }
            if entry.is_empty() {
                held.remove(&self.session_key);
            }
        }
    }
}

/// The value [`AcpClient::try_reserve_terminal_slot`] pushes in place of
/// a real handle to hold a session's slot for the duration of the
/// `create_terminal` round trip. Never a real client-issued id — reused
/// here so [`AcpClient::untrack_terminal`] can free a reservation that
/// never turned into a terminal, using the same removal path as a real
/// handle.
pub(crate) fn reserved_terminal_placeholder() -> TerminalHandle {
    TerminalHandle(RESERVED_TERMINAL_MARKER.to_string())
}

pub(crate) const RESERVED_TERMINAL_MARKER: &str = "\0reserved-terminal-slot";

tokio::task_local! {
    static ACP_CLIENT_TL: Arc<dyn AcpClient>;
}

/// Run `fut` with a client reachable from `current_acp_client`.
pub fn scope_acp_client<F: std::future::Future>(
    client: Arc<dyn AcpClient>,
    fut: F,
) -> impl std::future::Future<Output = F::Output> {
    ACP_CLIENT_TL.scope(client, fut)
}

/// The client for the turn currently executing, if it has one.
///
/// `None` on every non-ACP transport — `/rpc`, Matrix, Discord, voice —
/// which is what makes the client tools refuse there rather than
/// reaching for a connection that does not exist.
pub fn current_acp_client() -> Option<Arc<dyn AcpClient>> {
    ACP_CLIENT_TL.try_with(Arc::clone).ok()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// One recorded call to `read_text_file`: `(path, line, limit)`.
    pub(crate) type ReadCall = (String, Option<u32>, Option<u32>);

    /// One recorded call to `create_terminal`: `(command, args, cwd,
    /// output_byte_limit)`.
    pub(crate) type CreateCall = (String, Vec<String>, Option<String>, Option<u64>);

    /// A stand-in for the editor. Records what it was asked and answers
    /// from a script, so the tools can be driven without a socket.
    #[derive(Default)]
    pub(crate) struct FakeClient {
        pub reads: Mutex<Vec<ReadCall>>,
        pub writes: Mutex<Vec<(String, String)>>,
        pub read_answer: Mutex<Option<Result<String, String>>>,
        pub creates: Mutex<Vec<CreateCall>>,
        pub released: Mutex<Vec<TerminalHandle>>,
        pub killed: Mutex<Vec<TerminalHandle>>,
        /// Set by [`FakeClient::make_exit_never_return`]. When true,
        /// `wait_for_terminal_exit` blocks forever instead of resolving
        /// — the only way to make the one-shot `shell` tool's
        /// `tokio::time::timeout` race actually win on the timeout arm
        /// without a test sleeping out a real wall-clock timeout.
        exit_never_returns: Mutex<bool>,
        /// Set by [`FakeClient::make_create_terminal_hang`] /
        /// [`FakeClient::let_create_terminal_finish`]. When true,
        /// `create_terminal` records the call and then blocks forever
        /// (after its usual `yield_now`) instead of returning a handle
        /// — used to park a caller between `try_reserve_terminal_slot`
        /// and `track_terminal`, so a test can drop or abort it there
        /// the same way a cancelled turn would, and check that the
        /// reservation frees itself rather than leaking.
        create_never_returns: Mutex<bool>,
        /// Set by [`FakeClient::hand_out_distinct_handles`]. When true,
        /// `create_terminal` hands back `t1`, `t2`, … in order instead
        /// of always `t1` — needed by the cap test, which has to tell
        /// the eight handles it holds apart.
        distinct_handles: Mutex<bool>,
        next_handle: Mutex<u32>,
        /// Set by [`FakeClient::make_output_fail_with`]:
        /// `terminal_output` returns this as an error instead of a
        /// result, simulating a client that no longer recognises the
        /// handle (e.g. after its own restart).
        output_error: Mutex<Option<String>>,
        /// Set by [`FakeClient::make_kill_fail_with`]: both
        /// `kill_terminal` and `release_terminal` return this as an
        /// error instead of succeeding, simulating a client that
        /// answers "no such terminal" to a kill/release the same way
        /// `output_error` simulates it for a read.
        kill_error: Mutex<Option<String>>,
        /// Set by [`FakeClient::make_wait_fail_with`]:
        /// `wait_for_terminal_exit` returns this as an error instead of
        /// resolving, simulating a client mid-reconnect answering the
        /// RPC with an error while the command it asked about is still
        /// running. Checked before `exit_never_returns` so a test can
        /// pick exactly one of the two `wait_for_terminal_exit`
        /// behaviours.
        wait_error: Mutex<Option<String>>,
        /// The session id this fake's terminal tracking is keyed
        /// under, and the registry it tracks in. Plain fields rather
        /// than `Mutex`-wrapped: both are set once, by direct field
        /// assignment on an owned, not-yet-`Arc`-wrapped value (struct-
        /// update syntax needs every field visible at the call site,
        /// including the private ones above, so it doesn't work from
        /// outside this module). `client_tools`'s `shell_test_state`
        /// points `terminals` at the same `Arc` a `ServeState` under
        /// test uses, so tracking driven purely by calling this fake is
        /// visible on `ServeState.acp_terminals` too. Every other test
        /// builds a `FakeClient` with `..Default::default()` or
        /// `FakeClient::default()` untouched, which leaves an empty,
        /// private registry — correct, self-contained cap bookkeeping
        /// for tests that never look at a `ServeState`.
        pub(crate) terminal_session: String,
        pub(crate) terminals: TerminalRegistry,
        /// (stdout, exit_code) pairs queued for successive
        /// `create_terminal` calls, consumed in call order — the n-th
        /// queued entry becomes what `terminal_output` and
        /// `wait_for_terminal_exit` report for the n-th terminal
        /// created. Used by `skill_tools`'s tests, where the
        /// skills-index resolver and its `cat` fallback are two
        /// distinct `create_terminal` round trips that must answer
        /// with two different bodies (and, for the exit-status tests,
        /// two different outcomes). Everything that doesn't queue
        /// anything keeps the pre-existing defaults: empty output,
        /// `exit_code: None`.
        terminal_queue: Mutex<std::collections::VecDeque<(String, Option<u32>)>>,
        /// A queued (stdout, exit_code) pair committed to a specific
        /// handle id by `create_terminal` once it consumes from
        /// `terminal_queue`, so a later `terminal_output`/
        /// `wait_for_terminal_exit` call for that handle can look it
        /// back up.
        handle_result: Mutex<HashMap<String, (String, Option<u32>)>>,
    }

    impl FakeClient {
        /// Make every future `wait_for_terminal_exit` call hang forever,
        /// so a caller racing it against a timeout always sees the
        /// timeout branch.
        pub(crate) fn make_exit_never_return(&self) {
            *self.exit_never_returns.lock().unwrap() = true;
        }

        /// Make every future `create_terminal` call record itself (so
        /// tests can still see the reservation land) and then hang
        /// forever instead of returning — parking the caller between
        /// `try_reserve_terminal_slot` and `track_terminal`, exactly
        /// where a cancelled `create_terminal` RPC leaves a real turn.
        pub(crate) fn make_create_terminal_hang(&self) {
            *self.create_never_returns.lock().unwrap() = true;
        }

        /// Undo [`make_create_terminal_hang`](Self::make_create_terminal_hang),
        /// so a subsequent `create_terminal` call resolves normally
        /// again.
        pub(crate) fn let_create_terminal_finish(&self) {
            *self.create_never_returns.lock().unwrap() = false;
        }

        /// Make `create_terminal` hand back sequential ids (`t1`, `t2`,
        /// …) instead of always `t1`.
        pub(crate) fn hand_out_distinct_handles(&self) {
            *self.distinct_handles.lock().unwrap() = true;
        }

        /// Make `terminal_output` fail with `message`, as if the client
        /// no longer recognised the handle.
        pub(crate) fn make_output_fail_with(&self, message: &str) {
            *self.output_error.lock().unwrap() = Some(message.to_string());
        }

        /// Make both `kill_terminal` and `release_terminal` fail with
        /// `message`, as if the client no longer recognised the handle
        /// by the time a kill reached it.
        pub(crate) fn make_kill_fail_with(&self, message: &str) {
            *self.kill_error.lock().unwrap() = Some(message.to_string());
        }

        /// Make `wait_for_terminal_exit` fail with `message` instead of
        /// resolving, as if the client's connection dropped mid-RPC
        /// while the command it was asked about kept running.
        pub(crate) fn make_wait_fail_with(&self, message: &str) {
            *self.wait_error.lock().unwrap() = Some(message.to_string());
        }

        /// Queue `text` as the stdout the next `create_terminal` call's
        /// terminal will report via `terminal_output`, with a
        /// successful exit code (`Some(0)`) — the common case, where a
        /// test only cares about the command's output. Consumed in
        /// call order — see `terminal_queue`'s doc.
        pub(crate) fn queue_terminal_stdout(&self, text: &str) {
            self.queue_terminal_result(text, Some(0));
        }

        /// Like [`queue_terminal_stdout`](Self::queue_terminal_stdout),
        /// but with an explicit exit code instead of the default
        /// success — for a test simulating a command that ran but
        /// failed (e.g. `cat` on a path that turns out not to exist).
        pub(crate) fn queue_terminal_result(&self, text: &str, exit_code: Option<u32>) {
            self.terminal_queue
                .lock()
                .unwrap()
                .push_back((text.to_string(), exit_code));
        }

        /// Queue `text` as the answer to the next `read_text_file`,
        /// which reports it with a successful status. Without this the
        /// fake answers with an empty string, which is indistinguishable
        /// from a real client's empty file — so a test asserting on what
        /// the *client's* copy holds needs this to say so.
        pub(crate) fn queue_read_result(&self, text: &str) {
            *self.read_answer.lock().unwrap() = Some(Ok(text.to_string()));
        }

        /// The paths this client has been asked to read, in call order —
        /// how a routing test proves that a read went to the editor's
        /// machine rather than the agent's own.
        pub(crate) fn read_paths(&self) -> Vec<String> {
            self.reads
                .lock()
                .unwrap()
                .iter()
                .map(|(path, _, _)| path.clone())
                .collect()
        }

        /// How many terminals this client has been asked to create —
        /// what `skill_tools`'s cache test uses to prove the skills
        /// index is resolved once and reused, not re-resolved on every
        /// call.
        pub(crate) fn terminal_count(&self) -> usize {
            self.creates.lock().unwrap().len()
        }

        /// The command and argv of the most recent `terminal/create`, or
        /// `None` if there has not been one. Used by `client_tools`'s
        /// terminal-routed tests, which assert on the *shape* of the
        /// command (script, positional arguments, argv0 placeholder)
        /// rather than only on what came back from it.
        pub(crate) fn last_terminal_command(&self) -> Option<Vec<String>> {
            let creates = self.creates.lock().unwrap();
            let (command, args, _, _) = creates.last()?;
            let mut argv = Vec::with_capacity(args.len() + 1);
            argv.push(command.clone());
            argv.extend(args.iter().cloned());
            Some(argv)
        }
    }

    #[async_trait::async_trait]
    impl AcpClient for FakeClient {
        async fn read_text_file(
            &self,
            path: &str,
            line: Option<u32>,
            limit: Option<u32>,
        ) -> anyhow::Result<String> {
            self.reads
                .lock()
                .unwrap()
                .push((path.to_string(), line, limit));
            match self.read_answer.lock().unwrap().take() {
                Some(Ok(s)) => Ok(s),
                Some(Err(e)) => Err(anyhow::anyhow!(e)),
                None => Ok(String::new()),
            }
        }
        async fn write_text_file(&self, path: &str, content: &str) -> anyhow::Result<()> {
            self.writes
                .lock()
                .unwrap()
                .push((path.to_string(), content.to_string()));
            Ok(())
        }
        async fn create_terminal(
            &self,
            command: &str,
            args: &[String],
            cwd: Option<&str>,
            output_byte_limit: Option<u64>,
        ) -> anyhow::Result<TerminalHandle> {
            self.creates.lock().unwrap().push((
                command.to_string(),
                args.to_vec(),
                cwd.map(|s| s.to_string()),
                output_byte_limit,
            ));
            // A real `create_terminal` is a JSON-RPC round trip to the
            // editor, which genuinely suspends the caller. Yielding here
            // reproduces that suspension so a concurrency test can
            // actually interleave sibling calls between this client's
            // `try_reserve_terminal_slot` and its `track_terminal` — a
            // fake that resolved synchronously would never give a
            // sibling call a chance to run in between, and a cap-bypass
            // test built on it would pass even against the old, buggy
            // read-then-write code.
            tokio::task::yield_now().await;
            if *self.create_never_returns.lock().unwrap() {
                // Never resolves — see `make_create_terminal_hang`. The
                // call is recorded and the reservation above is already
                // in the registry; only the RPC round trip itself hangs,
                // matching a real cancelled `create_terminal`.
                std::future::pending::<()>().await;
            }
            let id = if *self.distinct_handles.lock().unwrap() {
                let mut next = self.next_handle.lock().unwrap();
                *next += 1;
                format!("t{next}")
            } else {
                "t1".to_string()
            };
            if let Some(result) = self.terminal_queue.lock().unwrap().pop_front() {
                self.handle_result
                    .lock()
                    .unwrap()
                    .insert(id.clone(), result);
            }
            Ok(TerminalHandle(id))
        }
        async fn terminal_output(&self, t: &TerminalHandle) -> anyhow::Result<TerminalOutput> {
            if let Some(message) = self.output_error.lock().unwrap().clone() {
                return Err(anyhow::anyhow!(message));
            }
            let mut out = TerminalOutput::default();
            if let Some((stdout, _)) = self.handle_result.lock().unwrap().get(&t.0).cloned() {
                out.output = stdout;
            }
            Ok(out)
        }
        async fn wait_for_terminal_exit(&self, t: &TerminalHandle) -> anyhow::Result<ExitStatus> {
            if let Some(message) = self.wait_error.lock().unwrap().clone() {
                return Err(anyhow::anyhow!(message));
            }
            if *self.exit_never_returns.lock().unwrap() {
                // Never resolves — see `make_exit_never_return`.
                std::future::pending::<()>().await;
            }
            let exit_code = self
                .handle_result
                .lock()
                .unwrap()
                .get(&t.0)
                .and_then(|(_, code)| *code);
            Ok(ExitStatus {
                exit_code,
                signal: None,
            })
        }
        async fn kill_terminal(&self, t: &TerminalHandle) -> anyhow::Result<()> {
            self.killed.lock().unwrap().push(t.clone());
            if let Some(message) = self.kill_error.lock().unwrap().clone() {
                return Err(anyhow::anyhow!(message));
            }
            Ok(())
        }
        async fn release_terminal(&self, t: &TerminalHandle) -> anyhow::Result<()> {
            self.released.lock().unwrap().push(t.clone());
            if let Some(message) = self.kill_error.lock().unwrap().clone() {
                return Err(anyhow::anyhow!(message));
            }
            Ok(())
        }
        async fn try_reserve_terminal_slot(&self) -> Result<TerminalReservation, CapHeld> {
            let mut registry = self.terminals.lock().unwrap();
            let held = registry.entry(self.terminal_session.clone()).or_default();
            if held.len() >= crate::tools::client_tools::MAX_TERMINALS_PER_SESSION {
                let handles: Vec<TerminalHandle> = held
                    .iter()
                    .filter(|h| h.0 != RESERVED_TERMINAL_MARKER)
                    .cloned()
                    .collect();
                let reservations = held.len() - handles.len();
                return Err(CapHeld {
                    handles,
                    reservations,
                });
            }
            held.push(reserved_terminal_placeholder());
            Ok(TerminalReservation::new(
                Arc::clone(&self.terminals),
                self.terminal_session.clone(),
            ))
        }
        async fn track_terminal(&self, reservation: TerminalReservation, handle: TerminalHandle) {
            reservation.resolve(handle);
        }
        async fn untrack_terminal(&self, handle: &TerminalHandle) {
            let mut registry = self.terminals.lock().unwrap();
            if let Some(held) = registry.get_mut(&self.terminal_session) {
                if let Some(pos) = held.iter().position(|h| h == handle) {
                    held.remove(pos);
                }
                if held.is_empty() {
                    registry.remove(&self.terminal_session);
                }
            }
        }
    }

    /// Outside a scope there is no client — a channel or `/rpc` turn
    /// must not find one lying around from an earlier ACP turn.
    #[tokio::test]
    async fn there_is_no_client_outside_a_scope() {
        assert!(current_acp_client().is_none());
    }

    #[tokio::test]
    async fn a_scoped_client_is_visible_inside_and_gone_after() {
        let fake: Arc<dyn AcpClient> = Arc::new(FakeClient::default());
        scope_acp_client(Arc::clone(&fake), async {
            let seen = current_acp_client().expect("inside the scope");
            seen.write_text_file("/p/a.txt", "hi").await.unwrap();
        })
        .await;
        assert!(
            current_acp_client().is_none(),
            "the scope must not leak past its future"
        );
    }

    /// The scope has to survive being handed to a spawned task's await
    /// points, since a turn awaits the model between tool calls.
    #[tokio::test]
    async fn the_scope_survives_an_await() {
        let fake: Arc<dyn AcpClient> = Arc::new(FakeClient::default());
        scope_acp_client(fake, async {
            tokio::task::yield_now().await;
            assert!(current_acp_client().is_some(), "still scoped after a yield");
        })
        .await;
    }
}
