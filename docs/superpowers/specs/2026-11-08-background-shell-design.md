# Background shell commands (`shell` with `background`)

- **Scope**: `crates/sapphire-agent-server/` (`background.rs`, `tools/builtin_tools.rs`'s `ShellTool`, a new
  `tools/background_shell.rs`, `tools/background_tools.rs`, `tools/client_tools.rs`'s `shell_start` description,
  `ServeState` in `serve/mod.rs`)
- **References**: `docs/superpowers/specs/2026-11-06-background-tasks-design.md` (the layer; "the layer spec"
  below, which lists this work as out of scope), `src/tools/client_exec.rs` (`run_client_command`: reserve, create,
  track), `src/tools/subagent.rs` (`start_background`: the pattern this follows)
- **Status**: design

## Why

An autonomous refactoring session wants to run `cargo test --workspace` (12 minutes), keep working, and read the
result when it ends. Today it can't: `shell` on the agent's machine blocks the turn and is killed after at most
600 s. In an ACP session `shell_start` keeps a command running on the editor's machine, but nobody is told when it
exits, so the session must poll and can't `WAIT` on it.

## Decisions

1. **`shell` gains `background: true`**, shaped like `subagent`'s `background`: main turn only, offered only while
   `[tools.background] max_per_session > 0`, returns a `bg-` ID at once, and delivers the result to the session's
   inbox when the command exits.
2. **It works on both machines.** Outside ACP the agent runs the process itself; inside an ACP session it runs as
   an editor terminal, and a watcher waits for its exit.
3. **A running command can be peeked and stopped** through `background_task` (`output`, `cancel`).
4. **Only commands that end become tasks.** A background command has a maximum run time (default 30 min, max 24 h)
   and is killed when it runs out, so `WAIT` can't wedge. `shell_start` / `shell_output` / `shell_kill` stay as
   they are, for commands meant to keep running (dev servers).
5. **Permissions are unchanged.** `shell` (Execute) is judged at the main turn's call as today; the background run
   starts only after the call is allowed.

## Structure

### The layer (`background.rs`)

```rust
pub(crate) enum TaskKind {
    // existing Subagent / Question
    Shell {
        command: String,
        on: ShellHost,
    },
}

pub(crate) enum ShellHost {
    /// The agent's own machine.
    Agent,
    /// The connected editor's machine (ACP).
    Editor,
}

pub(crate) enum Outcome {
    // in addition to the existing variants
    Exited {
        /// `None` when the process was killed by a signal or the editor reported none.
        exit_code: Option<i32>,
        output: String,
        /// Set when the run time ran out and the command was killed.
        timed_out: bool,
    },
}
```

- `Shell` counts toward `max_per_session` together with `Subagent`. Questions stay separate.
- `open` (load at start): a pending `Shell` becomes `Interrupted`, like `Subagent`.
- Older state files (without `Shell`) load unchanged.
- `render`, always naming the command's first line:

```
[Command bg-1a2b3c4d exited 0: "cargo test --workspace"]
<output>

[Command bg-1a2b3c4d timed out after 1800s and was killed: "cargo test --workspace"]
<output>

[Command bg-1a2b3c4d was interrupted by a restart: "cargo test --workspace"]
It was stopped; re-run it if you still need it.
```

  A command killed by a signal reads `exited without a code`. `Cancelled` and `Failed` render as for the other
  kinds, with the `Command` prefix.

### The tool (`tools/builtin_tools.rs`, `tools/background_shell.rs`)

- **Spec**: the main turn's `shell` spec carries `background: boolean` ("Run it in the background and return at
  once; its output is delivered to this conversation when it exits. Main conversation only."). Nested turns get
  the spec without it, as `subagent` does.
- **`timeout` with `background`**: the maximum run time, default 1800 s, max 86400 s. Without `background` it keeps
  its current meaning (default 60 s, max 600 s).
- **Refusals** (an error, nothing registered):
  - `background` from a nested turn or a turn without a session:
    `background commands can only be started from the main conversation; call shell without background`.
  - `max_per_session = 0`: `background runs are turned off on this deployment ([tools.background] max_per_session = 0); call shell without background`.
  - The cap: the layer's existing error, which lists the held tasks.
  - Failing to start (the shell isn't found, the workdir doesn't exist, the ACP terminal cap of 8 is full): the
    error. The task is registered before the start so the cap applies, and is removed again on failure.
- **Success** returns:
  `Command {id} started in the background. Its output will be delivered to this conversation when it exits; check it with background_task output, stop it with background_task cancel.`

### Running on the agent's machine

- Spawned like `shell` today (`resolve_shell`, `push_command_line`, the workdir, `kill_on_drop(true)`), with
  stdout and stderr piped.
- On Unix the child gets its own process group (`process_group(0)`); a timeout or a cancel kills the whole group
  (`killpg` with `SIGKILL`), so `cargo test`'s `rustc` and test binaries go too. On Windows only the child is
  killed.
- Two reader tasks copy stdout and stderr, in arrival order, into one shared **tail buffer**: once it passes
  `OUTPUT_CAP_BYTES` (50,000), the head is dropped at a character boundary and the text starts with
  `[… earlier output dropped]`. Bytes are decoded lossily.
- A watcher task waits for the exit or the run time, kills the group on time-out, waits for the readers to drain,
  and calls `complete(Exited { .. })`. Its `AbortHandle` is `attach`ed, so `cancel` aborts it; dropping the
  watcher drops the `Child` and a guard that kills the group.

### Running on the editor's machine (ACP)

- `try_reserve_terminal_slot`, `create_terminal("bash", ["-c", command], workdir, OUTPUT_CAP_BYTES)`,
  `track_terminal`, as `run_client_command` does. It counts toward the 8-terminal cap.
- The ACP client is a task-local, so its `Arc` is taken before the spawn and moved into the watcher.
- The watcher waits for `wait_for_terminal_exit` under the run time. On exit: `terminal_output`,
  `release_terminal`, `untrack_terminal`, `complete(Exited { .. })`. On time-out: `kill_terminal` first, then the
  same.
- `wait_for_terminal_exit` failing (the editor went away) completes the task as
  `Failed("the editor stopped answering while the command ran: {error}")`.

### Peeking and stopping (`background_task`, `ShellRuns`)

- `ServeState` gains `shell_runs: ShellRuns`, an in-memory map from task ID to the live run (not persisted):
  - agent machine: the tail buffer and the process group;
  - editor: the client `Arc` and the terminal handle.

  The watcher removes its entry when it completes.
- `background_task` gains the action `output { id }`: a running command's output so far (the tail; for the editor,
  `terminal_output`), followed by `(still running)`. For a finished command:
  `{id} has already exited; its result is in this conversation.` For a task that isn't a command:
  `{id} is not a command; output works only on background shell commands.`
- `background_task cancel` on a command: the layer's `cancel` (which aborts the watcher and queues `Cancelled`),
  then the run's own stop: kill the process group, or `kill_terminal` + `release_terminal` + `untrack_terminal`
  for the editor. A failed editor kill is a warning in the log only.
- `list` shows commands as `bg-… command "<first line>" (agent | editor)`.
- Session teardown (`cancel_all`): the same per-run stop for every command it returns.
- `shell_start`'s description gains: `To be told when a command finishes, use shell with background instead.`

## Failures and edge cases

- Starting fails → an error; nothing stays registered.
- The session closes (DONE, out of turns, the ACP session ends) → its commands are killed, as above.
- The agent restarts → on a clean shutdown the runtime drops the watchers, whose guards kill the process groups;
  after a crash the processes may outlive the agent (accepted: nothing tracks them any more). Editor terminals
  are released by the editor with the session. The pending tasks become `Interrupted`.
- The output is larger than the cap → the tail is kept (errors are usually at the end).
- A command that waits for stdin: stdin is null, as for `shell` today.
- `WAIT` in an autonomous session with a running command resumes when it exits, through the layer's
  `has_pending_or_inbox`.

## Tests

- Agent machine (`cfg(unix)`, real `sh`):
  - `echo` → `Exited { exit_code: Some(0) }` with the output;
  - a non-zero exit;
  - a run time of 1 s on `sleep 30` → `timed_out` and killed;
  - `cancel` kills the group: a grandchild `sleep` is gone afterwards;
  - `output` while it runs returns the text so far with `(still running)`;
  - the tail buffer drops the head at a character boundary and marks it.
- Editor (the fake `AcpClient`):
  - exit → `Exited` and the terminal is released and untracked;
  - time-out → `kill_terminal`;
  - `cancel` → kill + release;
  - a failed wait → `Failed`;
  - the terminal cap refuses and registers nothing.
- The layer:
  - `open` turns a pending `Shell` into `Interrupted`;
  - commands count toward `max_per_session`;
  - `render` for each outcome;
  - a state file from before this change still loads.
- The tool:
  - only the main turn's `shell` spec has `background`;
  - it's absent when `max_per_session = 0`;
  - a nested turn passing `background` is refused;
  - without `background`, `timeout` keeps its 600 s cap.
- Autonomous: a session that replied `WAIT` with a command running resumes when it exits and receives the output.

## Out of scope

- Killing process trees on Windows (job objects).
- Moving a timed-out one-shot `shell` into the background.
- Exit notifications for `shell_start` terminals.
- Streaming output into the conversation while the command runs.

## Commit scopes

`feat(tools)` / `feat(autonomous)`. Both are agent-internal scopes, so `cliff.toml` doesn't change.
