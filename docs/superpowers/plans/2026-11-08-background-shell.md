# Background Shell Commands Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `shell { background: true }` runs a command on the agent's machine or the ACP editor's machine, returns a `bg-` ID at once, and delivers its output to the session's inbox when it exits; `background_task` can peek at and stop it.

**Architecture:** A new `TaskKind::Shell` / `Outcome::Exited` in the background layer. A new module `tools/background_shell.rs` holds the runners (agent machine: a spawned process group with a tail buffer; editor: an ACP terminal) and the in-memory `ShellRuns` registry for peeking. Each run's watcher task owns a stop guard whose `Drop` kills the process group or the terminal, so the layer's existing `cancel` / `cancel_all` (which abort the watcher) stop the command with no extra calls.

**Tech Stack:** Rust, tokio (`process`, `time`), `libc` (Unix `killpg`), serde, the existing `AcpClient` trait.

**Spec:** `docs/superpowers/specs/2026-11-08-background-shell-design.md`

## Global Constraints

- All paths below are under `crates/sapphire-agent-server/`.
- Run time with `background`: default `1800` s, max `86400` s. Without `background`, `timeout` stays default 60, max 600.
- Output cap: `crate::tools::OUTPUT_CAP_BYTES` (50,000); the tail is kept; the dropped-head marker is exactly `[… earlier output dropped]` followed by a newline.
- Exact texts (copy verbatim):
  - spec property `background`: `Run it in the background and return at once; its output is delivered to this conversation when it exits. Main conversation only.`
  - success: `Command {id} started in the background. Its output will be delivered to this conversation when it exits; check it with background_task output, stop it with background_task cancel.`
  - nested / no session: `background commands can only be started from the main conversation; call shell without background`
  - disabled: `background runs are turned off on this deployment ([tools.background] max_per_session = 0); call shell without background`
  - editor wait failure: `the editor stopped answering while the command ran: {error}`
  - `output` running suffix: `(still running)` on its own line after the text
  - `output` finished: `{id} has already exited; its result is in this conversation.`
  - `output` not a command: `{id} is not a command; output works only on background shell commands.`
  - `shell_start` description addition: `To be told when a command finishes, use shell with background instead.`
  - render headers: `[Command {id} exited {code}: "{first line}"]`, `[Command {id} exited without a code: "{first line}"]`, `[Command {id} timed out after {secs}s and was killed: "{first line}"]`, `[Command {id} was interrupted by a restart: "{first line}"]` + newline + `It was stopped; re-run it if you still need it.`, `[Command {id} was cancelled: "{first line}"]`, `[Command {id} failed: "{first line}"]` + newline + message.
  - `list` row: `- {id} (command "{first line}" on agent|editor): {label} — started {secs}s ago`
- Commit scopes: `feat(tools)` / `feat(autonomous)`; end every commit with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.
- Code that names paths or spawns processes must build on Windows: process-group code is `#[cfg(unix)]`; Windows kills only the child (`start_kill`). Tests that run `sh` are `#[cfg(unix)]`.
- Session teardown is the layer's existing `cancel_all` callers only (autonomous close, `config_tools`). An ACP session ending does not cancel background tasks today and this plan doesn't change that; a lost editor surfaces as the wait-failure `Failed`. (Ruling against the spec's "the ACP session ends" bullet, which assumed a teardown that doesn't exist.)
- The spec's "then the run's own stop" after `cancel` / `cancel_all` is implemented by the watcher's stop guard (`Drop`), not by extra calls in `background_tools.rs` / `autonomous.rs`.

## Review Focus

1. A command that leaves grandchildren (`sh -c 'sleep 300 & wait'`) — cancel and time-out must kill them too, not just `sh`. (Task 2 test `cancel_kills_the_process_group`.)
2. A command that writes much more than 50,000 bytes, or multi-byte UTF-8 split across a cut — the tail must stay valid UTF-8 and marked. (Task 2 tests on `TailBuffer`.)
3. The spawn failing after registration (missing workdir) — nothing may stay pending, or `WAIT` wedges and the cap leaks. (Task 4 test `a_failed_start_leaves_nothing_registered`.)
4. A subagent's nested turn — its `shell` spec must not carry `background`, and passing it anyway is refused. (Task 4 tests.)
5. An editor terminal that never exits until the run time — it must be killed and released, freeing the 8-terminal cap. (Task 3 test `time_out_kills_and_releases_the_terminal`.)

---

### Task 1: Shell tasks in the background layer

**Files:**
- Modify: `src/background.rs` (`TaskKind`, new `ShellHost`, `Outcome`, `render`, new `discard`, tests)
- Modify: `src/tools/background_tools.rs` (the `list` match gains the `Shell` arm — row format from Global Constraints)
- Modify: any other exhaustive `match` on `TaskKind` / `Outcome` that stops compiling (minimal arms only)

**Interfaces:**
- Produces:
  - `TaskKind::Shell { command: String, on: ShellHost, timeout_secs: u64 }` — `timeout_secs` (the run time) is an addition to the spec's variant, needed because the timed-out header names it.
  - `#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)] pub(crate) enum ShellHost { Agent, Editor }`
  - `Outcome::Exited { exit_code: Option<i32>, output: String, timed_out: bool }`
  - `BackgroundTasks::discard(&self, session: &str, id: &str) -> bool` — removes a running task with no completion queued (for a start that failed after `register`); persists; returns whether it existed.
  - `pub(crate) fn first_line(s: &str) -> &str` helper may stay private.

- [ ] **Step 1: Write the failing tests** in `background.rs`'s test module:
  - `shell_counts_toward_max_per_session`: `in_memory(1)`, register a `Shell`, a second `register` (a `Subagent`) errs and the message contains the shell's id.
  - `open_interrupts_a_pending_shell`: persist a pending `Shell` to a temp dir, `open` it, `drain` gives one completion with `Outcome::Interrupted` and `kind` still `Shell`.
  - `discard_removes_without_a_completion`: register, `discard` → `true`, `pending` empty, `drain` empty, `has_pending_or_inbox` false; `discard` again → `false`.
  - `render_shell_outcomes`: for command `"cargo test\n--workspace"` and `timeout_secs: 1800`, assert each header from Global Constraints exactly (`exited 0`, `exited without a code`, `timed out after 1800s and was killed`, interrupted, cancelled, failed), and that the output (or message) follows on the next line.
  - `old_state_without_shell_still_loads`: a JSON file in the pre-change format (a pending `Subagent` and an inbox `Finished`) loads.

- [ ] **Step 2: Run them to verify they fail**
  Run: `cargo test -p sapphire-agent-server background::tests` — Expected: compile errors (no `Shell`, `Exited`, `discard`).

- [ ] **Step 3: Implement.** `render` dispatches `Shell` to a `render_shell` beside `render_question`; `resumable` returns `false` for it. `open`'s existing non-question branch already interrupts it — confirm, don't special-case. `register`'s cap filter (non-questions) already counts it.

- [ ] **Step 4: Run tests** — `cargo test -p sapphire-agent-server background` and `cargo clippy -p sapphire-agent-server --all-targets` — Expected: PASS, no warnings.

- [ ] **Step 5: Commit** — `feat(tools): shell commands as background tasks`

---

### Task 2: Running on the agent's machine

**Files:**
- Create: `src/tools/background_shell.rs` (declare `pub(crate) mod background_shell;` in `src/tools/mod.rs`)
- Modify: `Cargo.toml` — `[target.'cfg(unix)'.dependencies] libc = "0.2"`
- Modify: `src/serve/mod.rs` — `ServeState` gains `pub(crate) shell_runs: Arc<ShellRuns>`, built empty in every constructor (incl. `for_test*`)
- Modify: `src/tools/builtin_tools.rs` — make `resolve_shell`, `push_command_line`, `expand_path` `pub(crate)` if they aren't

**Interfaces:**
- Consumes: Task 1's `TaskKind::Shell`, `Outcome::Exited`, `BackgroundTasks::{attach, complete}`.
- Produces (in `background_shell.rs`):
  - `pub(crate) struct TailBuffer` with `new(cap: usize)`, `push(&mut self, bytes: &[u8])`, `text(&self) -> String` (lossy UTF-8, cut at a char boundary, prefixed by the marker once anything was dropped).
  - `pub(crate) struct ShellRuns` with `new()`, `insert(id: TaskId, run: LiveRun)`, `remove(id: &str)`, `async fn peek(&self, id: &str) -> Option<anyhow::Result<String>>` (`None` = no live run).
  - `pub(crate) enum LiveRun { Agent(Arc<Mutex<TailBuffer>>), Editor { client: Arc<dyn AcpClient>, terminal: TerminalHandle } }` (`Editor` used by Task 3).
  - `pub(crate) struct HostCommand { pub command: String, pub shell: String, pub workdir: PathBuf, pub run_time: Duration }`
  - `pub(crate) fn start_on_agent(cmd: HostCommand, background: Arc<BackgroundTasks>, runs: Arc<ShellRuns>, session: String, id: TaskId) -> anyhow::Result<()>` — spawns synchronously (so a spawn error returns here), then `tokio::spawn`s the watcher and `attach`es its `AbortHandle`.

- [ ] **Step 1: Write the failing tests** (in `background_shell.rs`, `#[cfg(unix)]` where they run `sh`; use `BackgroundTasks::in_memory(4)` and poll `has_inbox` with a 10 s bound):
  - `tail_buffer_keeps_the_tail_and_marks_the_cut`: cap 10, push `"0123456789abcdef"` → text is `"[… earlier output dropped]\n6789abcdef"`.
  - `tail_buffer_cuts_at_a_char_boundary`: cap 5, push `"aaaé€"` (bytes split mid-char) → text after the marker is valid and is a suffix of the input not starting mid-char.
  - `tail_buffer_under_cap_is_unmarked`: cap 100, push `"hi"` → `"hi"`.
  - `an_exit_delivers_the_output`: `echo out; echo err >&2; exit 3` → one completion `Exited { exit_code: Some(3), timed_out: false, .. }` whose output contains `out` and `err`; `runs.peek(id)` is `None` afterwards.
  - `run_time_kills_and_reports`: `sleep 30`, run time 1 s → `Exited { timed_out: true, .. }` within 10 s.
  - `cancel_kills_the_process_group`: `sh -c 'sleep 300 & echo $! > {tmp}/pid; wait'`; wait until the pid file exists; `background.cancel(session, id)`; within 5 s `kill(pid, 0)` fails (the grandchild is gone).
  - `peek_returns_output_so_far`: `echo first; sleep 30` → after `first` appears, `peek` is `Some(Ok(text))` containing `first`; then cancel.
  - `a_spawn_failure_is_returned`: a nonexistent workdir → `start_on_agent` errs.

- [ ] **Step 2: Run them to verify they fail** — `cargo test -p sapphire-agent-server background_shell` — Expected: compile errors.

- [ ] **Step 3: Implement.** Spawn with `process_group(0)` (unix), stdin null, stdout/stderr piped, `kill_on_drop(true)`. Two reader tasks append to the shared `TailBuffer`. The watcher holds a `GroupGuard { pgid: Option<i32> }` whose `Drop` calls `libc::killpg(pgid, SIGKILL)` (unix; ignore `ESRCH`) — on every exit path, including abort — then waits for exit or `run_time`; on time-out it kills the group; it joins the readers (bounded: 2 s, they end when the pipes close), removes the `ShellRuns` entry, and calls `complete(Exited {..})`. `exit_code` is `status.code()`. On Windows the guard does nothing extra (`kill_on_drop` kills the child).

- [ ] **Step 4: Run tests** — `cargo test -p sapphire-agent-server background_shell`, then `cargo clippy --workspace --all-targets` — Expected: PASS.

- [ ] **Step 5: Commit** — `feat(tools): run background commands on the agent's machine`

---

### Task 3: Running on the editor's machine

**Files:**
- Modify: `src/tools/background_shell.rs`
- Modify (if needed): `src/tools/acp_client.rs`'s test fake — only additions for observing `kill_terminal` / `release_terminal` calls

**Interfaces:**
- Consumes: Task 2's `ShellRuns`, `LiveRun::Editor`; `AcpClient::{try_reserve_terminal_slot, create_terminal, track_terminal, wait_for_terminal_exit, terminal_output, kill_terminal, release_terminal, untrack_terminal}`; `client_exec::CLIENT_SHELL`; `client_tools::cap_error`.
- Produces: `pub(crate) async fn start_on_editor(client: Arc<dyn AcpClient>, command: String, workdir: Option<String>, run_time: Duration, background: Arc<BackgroundTasks>, runs: Arc<ShellRuns>, session: String, id: TaskId) -> anyhow::Result<()>` — reserves, creates and tracks the terminal before returning (so the cap error and a create error return here), then spawns the watcher and `attach`es it. `ShellRuns::peek` for `Editor` calls `terminal_output` and returns its text.

- [ ] **Step 1: Write the failing tests** (fake client from `acp_client.rs`'s test module):
  - `an_editor_exit_delivers_and_releases`: `queue_terminal_result("built", Some(0))` → `Exited { exit_code: Some(0), output contains "built" }`; the terminal was released and untracked (`terminal_count() == 0`).
  - `time_out_kills_and_releases_the_terminal`: `make_exit_never_return()`, run time 1 s → `Exited { timed_out: true }`, `kill_terminal` was called, terminal released, count 0.
  - `cancel_kills_and_releases`: never-returning exit; `background.cancel` → within 5 s kill + release observed, count 0.
  - `a_failed_wait_is_failed`: `make_wait_fail_with("gone")` → `Outcome::Failed { message }` equal to `the editor stopped answering while the command ran: gone` (if the fake's error text wraps, assert `starts_with` the prefix and `contains("gone")`).
  - `the_terminal_cap_refuses_before_spawning`: fill the 8 slots, `start_on_editor` errs, and no watcher ran (count still 8).
  - `peek_reads_the_terminal`: `queue_terminal_stdout("partial")` + never-returning exit → `peek` contains `partial`.

- [ ] **Step 2: Run them to verify they fail** — `cargo test -p sapphire-agent-server background_shell` — Expected: compile errors.

- [ ] **Step 3: Implement.** The watcher holds an `EditorGuard { client, terminal, armed: bool }`; while armed, its `Drop` does `tokio::spawn(kill_terminal → release_terminal → untrack_terminal)`, logging failures with `warn!`. Normal exit and time-out paths do the calls inline and disarm it. Capture the client `Arc` before spawning (task-locals don't cross `tokio::spawn`).

- [ ] **Step 4: Run tests** — `cargo test -p sapphire-agent-server background_shell`, `cargo clippy --workspace --all-targets` — Expected: PASS.

- [ ] **Step 5: Commit** — `feat(tools): run background commands on the editor's machine`

---

### Task 4: The `shell` tool and `background_task output`

**Files:**
- Modify: `src/tools/builtin_tools.rs` (`ShellTool`: `pub(crate) fn shell_spec(background: bool) -> ToolSpec`, used by `ShellTool::new`; the `background` branch in `execute`)
- Modify: `src/lib.rs` (when `config.tools.background.max_per_session > 0`: `tool_set.replace_spec("shell", shell_spec(true)).await`, next to the `BackgroundTaskTool` registration)
- Modify: `src/tools/subagent.rs` (`subagent_tool_specs`: the inherited `shell` spec loses `background`, like `without_background` does for `subagent`)
- Modify: `src/tools/background_tools.rs` (`output` action, enum and description; `list` already done in Task 1)
- Modify: `src/tools/client_tools.rs` (`shell_start` description gains the sentence from Global Constraints)
- Modify: `config.example.toml` (the `[tools.background]` comment: `shell` with `background: true` too, run time default 30 min / max 24 h, counted toward `max_per_session`)

**Interfaces:**
- Consumes: Tasks 1–3 (`register`, `discard`, `start_on_agent`, `start_on_editor`, `ShellRuns::peek`, `ServeState.shell_runs`), `current_turn_context()`, `current_acp_client()`.

- [ ] **Step 1: Write the failing tests:**
  - builtin_tools: `shell_spec_offers_background_only_when_asked` — `shell_spec(false)` has no `background` property; `shell_spec(true)` has it with the exact description.
  - builtin_tools (`cfg(unix)`, under a `TurnContext` with a session, depth 0, `ServeState::for_test`): `a_background_shell_returns_at_once_and_delivers` — `{"command":"echo hi","background":true}` returns the success text with a `bg-` id; within 10 s the session's `drain` has `Exited { exit_code: Some(0) }` containing `hi`.
  - builtin_tools: `background_is_refused_from_a_nested_turn` (depth 1) and `…_without_a_session` — exact refusal text; nothing registered.
  - builtin_tools: `background_is_refused_when_disabled` — state with `max_per_session = 0` → exact disabled text.
  - builtin_tools: `a_failed_start_leaves_nothing_registered` — `workdir` that doesn't exist → error, `pending(session)` empty, `has_pending_or_inbox` false.
  - builtin_tools: `background_run_time_defaults_and_caps` — the `HostCommand` (or a small `fn background_run_time(input) -> Duration`) gives 1800 s for none, 86400 s for 999999; and without `background` the existing 600 s cap is unchanged.
  - builtin_tools: inside an ACP-scoped client (`scope_acp_client` with the fake), a background call starts an editor terminal (`last_terminal_command()` is `bash -c <command>`).
  - subagent: `a_nested_shell_spec_has_no_background` — `subagent_tool_specs` over a parent view holding `shell_spec(true)` yields a `shell` without `background`.
  - background_tools: `output_peeks_a_running_command`, `output_on_a_finished_command` (exact text), `output_on_a_subagent_task` (exact text), `output_needs_an_id`.
  - client_tools: the `shell_start` description contains the added sentence.

- [ ] **Step 2: Run them to verify they fail** — `cargo test -p sapphire-agent-server shell background_task` — Expected: FAIL / compile errors.

- [ ] **Step 3: Implement.** In `execute`, when `background` is `true`: check context (session + depth 0), `background.enabled()`, then `register(session, TaskKind::Shell { command, on, timeout_secs }, label = first line of command)`, then start with the matching runner (`current_acp_client()` → editor, else agent; the `shell` parameter is ignored on the editor as today). On a start error, `discard` and return the error. `background_task output`: no live run + the id is still pending → `peek` raced the exit, treat as finished; a non-`Shell` pending task → the not-a-command text; unknown id → the layer's `no running background task` style error.

- [ ] **Step 4: Run tests** — `cargo test --workspace`, `cargo clippy --workspace --all-targets`, `cargo fmt --check` — Expected: PASS.

- [ ] **Step 5: Commit** — `feat(tools): shell runs commands in the background`

---

### Task 5: Autonomous `WAIT` on a command

**Files:**
- Modify: `src/autonomous.rs` (test module only, unless the test exposes a gap)

**Interfaces:**
- Consumes: Task 4's `shell` with `background`; the existing test helpers used by `a_completion_resumes_the_same_session` (`autonomous.rs:945`).

- [ ] **Step 1: Write the test** `a_background_command_resumes_the_session` (`cfg(unix)`), modelled on `a_completion_resumes_the_same_session`: the scripted model's first round calls `shell {"command":"echo done-marker","background":true}` and replies `WAIT`; the session suspends; when the command exits the same session resumes and its next turn's input contains `done-marker` and `[Command `.

- [ ] **Step 2: Run it** — `cargo test -p sapphire-agent-server autonomous::tests::a_background_command_resumes_the_session` — Expected: PASS (the layer already resumes on any completion). If it fails, fix the gap in `autonomous.rs` and say so in the report.

- [ ] **Step 3: Full verification** — `cargo fmt --check && cargo clippy --workspace --all-targets && cargo test --workspace` — Expected: all PASS.

- [ ] **Step 4: Commit** — `feat(autonomous): resume on a background command's exit`
