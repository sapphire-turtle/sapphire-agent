//! The ACP side of the shell and file tools: what the same tool names do
//! when the turn's machine is the *editor's*, not this agent's own.
//!
//! `file_read`/`file_write`/`file_append`/`file_delete`/`shell`
//! (`src/tools/builtin_tools.rs`) each read
//! `crate::tools::acp_client::current_acp_client()` at the top of their
//! `execute` and hand over to a `client_*` function here when a client is
//! scoped to the turn. That scoping *is* the routing decision (#270): one
//! tool name reaches one machine or the other, and the tool description
//! says which, because a name that meant a fixed machine would be wrong in
//! one of the two cases every time.
//!
//! Everything that knows ACP's wire surface lives here for that reason.
//! Four of the `client_*` functions are not ACP requests at all: ACP has no
//! append, delete, list or stat, so `client_append` is read → concatenate
//! → write, and `client_delete` / `client_dir_list` / `client_dir_walk`
//! are `rm` and `find` over the terminal.
//!
//! The three tools with no agent-side body (`ClientShellStart` /
//! `ClientShellOutput` / `ClientShellKill`) are ordinary `Tool`s and still
//! live at the bottom of this file.

use crate::provider::ToolSpec;
use crate::tools::acp_client::{
    AcpClient, ExitStatus, TerminalHandle, TerminalOutput, current_acp_client,
};
use crate::tools::builtin_tools::truncation_marker;
// `ShellTool::execute` now dispatches the ACP branch itself (see the `shell`
// tools); this module only constructs one inside its test module.
#[cfg(test)]
use crate::tools::builtin_tools::ShellTool;
use crate::tools::client_exec::run_client_command;
use crate::tools::{OUTPUT_CAP_BYTES, Tool, ToolKind};
use anyhow::{Context, Result};
use async_trait::async_trait;
use serde_json::json;

/// The refusal for the three tools that have no agent-side body.
///
/// `ClientShellStart`/`ClientShellOutput`/`ClientShellKill` exist only
/// against a client, so with none scoped there is nothing to route to and
/// the call is refused.
///
/// The unified `file_read`/`file_write`/`file_append`/`file_delete`/
/// `shell` tools do **not** refuse this way any more: "no editor" now means
/// the agent's own machine, which is a route rather than an error, so those
/// tools have no such message to give (#270).
///
/// One constant rather than a literal per tool, so the wording — and the
/// substring the tests key on — cannot drift between them. `skill_tools`
/// keeps its own copy of the same sentence; see the note there.
const NO_EDITOR: &str = "no editor is connected to this session; this tool only works over ACP";

// ---------------------------------------------------------------------------
// ACP-side implementations for the unified tools
// ---------------------------------------------------------------------------
//
// These are not tools any more: `builtin_tools.rs` picks between them and
// its own bodies by reading `current_acp_client()`, so nothing here
// advertises itself to the model.

/// `file_read` against the editor's machine. The tool's own `offset`/
/// `limit` map onto ACP's `line`/`limit`, which exist for exactly this
/// reason: the full file does not have to cross the wire to read a range.
///
/// Note what does *not* happen here: the agent-side body's line-number
/// prefixing and `/dev/`+`/proc/` guard are about how a file is read off
/// *this* machine, so the client's answer is passed through as the client
/// wrote it rather than being reformatted into the host's shape.
pub(crate) async fn client_read(
    client: &std::sync::Arc<dyn AcpClient>,
    input: &serde_json::Value,
) -> Result<String> {
    let path = input["path"].as_str().context("missing 'path'")?;
    let line = input["offset"].as_u64().map(|v| v as u32);
    let limit = input["limit"].as_u64().map(|v| v as u32);
    client.read_text_file(path, line, limit).await
}

/// `file_write` against the editor's machine.
pub(crate) async fn client_write(
    client: &std::sync::Arc<dyn AcpClient>,
    input: &serde_json::Value,
) -> Result<String> {
    let path = input["path"].as_str().context("missing 'path'")?;
    let content = input["content"].as_str().context("missing 'content'")?;
    client.write_text_file(path, content).await?;
    Ok(format!("Written: {path} ({} bytes)", content.len()))
}

/// `file_append` against the editor's machine.
///
/// ACP has no append, so this is read → concatenate → write. Two
/// consequences the tool description has to carry, because a model that
/// does not know them will use this where it should use a shell: the whole
/// file crosses the wire twice, and the pair is not atomic — a write by
/// another process between the read and the write is lost. A missing parent
/// directory is a `fs/write_text_file` error, not a silent `mkdir`.
pub(crate) async fn client_append(
    client: &std::sync::Arc<dyn AcpClient>,
    input: &serde_json::Value,
) -> Result<String> {
    let path = input["path"].as_str().context("missing 'path'")?;
    let content = input["content"].as_str().context("missing 'content'")?;
    // A missing file is the ordinary "create it" case, not an error —
    // `file_append`'s agent-side contract says it creates the file. Every
    // other read failure is reported as-is by the `?` on `write_text_file`.
    let existing = client
        .read_text_file(path, None, None)
        .await
        .unwrap_or_default();
    let mut merged = existing;
    merged.push_str(content);
    client.write_text_file(path, &merged).await?;
    Ok(format!("Appended: {path} (+{} bytes)", content.len()))
}

// ---------------------------------------------------------------------------
// The ACP surface ACP does not have, spelled as a command
// ---------------------------------------------------------------------------

/// The waiting budget for the short, local commands the unified tools run on
/// the client (`rm` today, `find` for a client-side `dir_list`/`dir_walk`).
/// Same value as `skill_tools`' `LOCAL_TIMEOUT`: these cost about as little
/// as reading a file.
pub(crate) const CLIENT_LOCAL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Run one `bash -c <script> <argv0> <args...>` on the client and require it
/// to have finished with exit code 0, returning its stdout.
///
/// Paths travel as **positional arguments**, never interpolated into the
/// script: `terminal/create` takes a command and an argv separately, so
/// there is no shell quoting to get wrong and a path with spaces or a quote
/// in it is safe by construction.
///
/// A timeout is reported as an error naming the terminal handle rather than
/// swallowing it: a handle left tracked counts against the session's
/// 8-terminal cap (`MAX_TERMINALS_PER_SESSION`), so the model has to be told
/// which one to free.
///
/// `bash` rather than `sh`, and rather than whatever the client happens to
/// ship as `/bin/sh`: the scripts here use nothing beyond POSIX, but naming
/// the interpreter keeps the same script from meaning two things depending
/// on where it lands.
pub(crate) async fn run_client_bash(
    client: &std::sync::Arc<dyn AcpClient>,
    script: &str,
    argv0: &str,
    args: &[String],
) -> Result<String> {
    let mut bash_args = vec!["-c".to_string(), script.to_string()];
    bash_args.push(argv0.to_string());
    bash_args.extend(args.iter().cloned());

    let run = run_client_command(client, "bash", &bash_args, None, CLIENT_LOCAL_TIMEOUT).await?;
    if let Some(handle) = run.timed_out_handle {
        anyhow::bail!(
            "timed out after {}s on the editor's machine; the command is still \
             running as terminal {handle}. Use shell_output to check on \
             it, or shell_kill to stop it.",
            CLIENT_LOCAL_TIMEOUT.as_secs()
        );
    }
    let status = run
        .status
        .expect("run_client_command always sets `status` when it does not time out");
    if status.signal.is_some() || status.exit_code != Some(0) {
        anyhow::bail!(
            "the command failed on the editor's machine: {}",
            format_exit_status(&status).trim()
        );
    }
    Ok(run.output.output)
}

/// Resolve a leading `~` on the **client**, in the client's own shell.
///
/// The agent-side bodies expand `~/...` with `shellexpand::tilde` against
/// *this* machine's `$HOME` before they touch a filesystem; the same path
/// reaching the client has to be expanded against the *client's* `$HOME`,
/// which only the client's shell knows. `find "$1"` cannot do it either: the
/// quoting that keeps a path with spaces safe is exactly what stops the shell
/// from expanding a tilde inside it -- which is how `file_delete`, `dir_list`
/// and `dir_walk` all ended up failing on the `~/...` paths their own
/// descriptions advertise.
///
/// `~user/...` is deliberately left alone: no portable shell expands another
/// user's home from a script, and the agent side does not either
/// (`shellexpand::tilde` handles a bare `~` only). Such a path then fails the
/// directory check below rather than silently reaching somewhere else.
///
/// Every script here moves the path out of `$1` first and uses `"$p"` below:
/// `$1` is a positional argument of `bash -c` rather than a name the script
/// can quote its way around, and `p` is also what the checks name in their
/// messages.
const RESOLVE_TILDE_SH: &str = r#"
p="$1"
case "$p" in
  "~") p="$HOME" ;;
  "~/") p="$HOME/" ;;
  "~/"*) r="$p"; r="${r#?}"; r="${r#?}"; p="$HOME/$r" ;;
esac
"#;

/// `file_delete` against the editor's machine: `rm`, since ACP has no delete.
///
/// The `-d` check is what keeps a recursive flag from being needed at all: the
/// agent-side contract is "files, never directories", so a directory is
/// refused here rather than removed. Both refusals are non-zero exits, which
/// is how [`run_client_bash`] reports them.
///
/// The path arrives as `$1`, not `$0`: `bash -c <script> <argv0> [args...]`
/// puts the first word *after* the script in `$0`, so a placeholder `$0` plus
/// `rm -- "$p"` is what makes the path the first real argument. Passing the
/// path as `argv0` instead would silently delete whatever `$0` happened to
/// name — here, nothing, and every delete would fail with "no such file".
/// `"$p"` is [`RESOLVE_TILDE_SH`]'s expansion of that argument, so `~/x`
/// deletes the client's `$HOME/x` — the path the tool description promises.
pub(crate) fn delete_script() -> String {
    format!("{RESOLVE_TILDE_SH}{DELETE_TAIL_SH}")
}

/// [`delete_script`]'s own body, run once `$1` has become `$p`.
const DELETE_TAIL_SH: &str = r#"
if [ -d "$p" ]; then
  echo "is a directory" >&2
  exit 1
fi
if [ ! -e "$p" ]; then
  echo "no such file" >&2
  exit 1
fi
rm -- "$p"
"#;

/// The `$0` placeholder [`delete_script`] expects. Bash's `-c` has no other way
/// to place a value in `$1`: the name is required syntactically, but nothing
/// reads it.
const DELETE_ARGV0: &str = "client_delete";

/// `file_delete` against the editor's machine.
pub(crate) async fn client_delete(
    client: &std::sync::Arc<dyn AcpClient>,
    input: &serde_json::Value,
) -> Result<String> {
    let path = input["path"].as_str().context("missing 'path'")?;
    run_client_bash(client, &delete_script(), DELETE_ARGV0, &[path.to_string()]).await?;
    Ok(format!("Deleted: {path}"))
}

// ---------------------------------------------------------------------------
// Listing and walking: `find` over the same terminal
// ---------------------------------------------------------------------------
//
// ACP has no list, glob or stat either, so both tools run one `find` on the
// client and the *shape* the model sees is built here. That is the point of
// #262 travelling this road: two machines, one output format, so a `dir_list`
// result reads identically whichever side it came from.
//
// The *order* is the other half of that, and it belongs to the script: the
// `head` that caps a walk's traversal has to cut the entries in the same order
// the agent's own walk would have produced, or the cap would keep a different
// prefix than the agent side keeps. `shape_entries` therefore keeps the
// script's order rather than sorting it again — see [`ORDER_SH`] for the order
// itself.

/// The ordering stage every listing pipeline runs between `find` and whatever
/// follows it (the classifier, or the walk's `head`).
///
/// The agent-side walk is a **pre-order DFS**: each directory's children in
/// path order, with a directory's whole subtree emitted immediately after the
/// directory itself — `sub`, everything under `sub`, then the sibling file
/// `sub.txt`. `find` alone does *not* produce that: it reports a directory's
/// entries in readdir order, so "use find's own order" is not even
/// deterministic across machines and filesystems. A plain `LC_ALL=C sort` of
/// the whole listing is not it either: it compares `/` (0x2F) as an ordinary
/// byte, and 0x2F is greater than `.` (0x2E), so `sub.txt` sorts before `sub`
/// and drags the whole subtree behind it. Rewriting the separator to 0x01 — a
/// byte below every printable one — makes a whole-path byte comparison agree
/// with the agent's component-wise pre-order DFS, including the
/// directory-then-its-contents rule that no other single sort expresses.
///
/// The two `tr`s are the two halves of that: the first produces the sort key,
/// the second restores each line to the path `find` printed. A path containing
/// a literal 0x01 byte is therefore outside the design, in the same class as a
/// filename containing a newline (see [`shape_entries`]).
const ORDER_SH: &str = r#" | tr '/' '\001' | LC_ALL=C sort | tr '\001' '/' |"#;

/// Print one `E<TAB>path` / `D<TAB>path` / `F<TAB>path` line per entry.
///
/// `E` is the prologue's announcement of the path being listed (see
/// [`LISTING_PROLOGUE_SH`]); `D`/`F` are one entry each. This runs as the last
/// stage of the pipeline, so what it prints is the script's stdout.
///
/// The loop variable is `e`, not `p`: `p` is the resolved path
/// [`RESOLVE_TILDE_SH`] produced, and this half never needs it.
const CLASSIFY_LINE: &str = r#"
while IFS= read -r e; do
  if [ -d "$e" ]; then printf 'D\t%s\n' "$e"; else printf 'F\t%s\n' "$e"; fi
done
"#;

/// The prologue both listing scripts run first: the client's `~` expansion
/// ([`RESOLVE_TILDE_SH`]), a refusal of anything that is not a directory, and
/// the `E` announcement [`CLASSIFY_LINE`] expects.
///
/// The refusal is what keeps a missing directory from *looking* like an empty
/// one: `find` prints nothing on stdout when it fails, so a script that only
/// reported what came back would answer `(empty) <path>` for a path that is
/// not there, where the agent side reports "Failed to list". A path that is a
/// file rather than a directory is refused here for the same reason.
const LISTING_PROLOGUE_SH: &str = r#"
set -e
if [ ! -d "$p" ]; then
  echo "not a directory: $p" >&2
  exit 1
fi
printf 'E\t%s\n' "$p"
"#;

/// The `head` that caps a walk's traversal: `$3` is `max_entries + 1`, one more
/// than the caller will show, which is what makes "there was more" decidable
/// without a second round trip. `head` rather than `find -quit` because it
/// stops the *traversal*, where `find` would keep descending a huge tree it is
/// about to discard — and it cuts the same order the agent side cuts, thanks to
/// [`ORDER_SH`].
const WALK_HEAD_SH: &str = r#" head -n "$3" |"#;

/// `find`'s exit status, read after the pipeline rather than trusted from it:
/// the pipeline's own status is the classifier's (0 — it read to EOF), so a
/// `find` that died — an unreadable directory inside the tree, a path that
/// vanished between the prologue's check and the traversal — would otherwise be
/// reported as a short listing rather than an error.
///
/// `141` is success here: the walk's `head` closes the pipe once it has `$3`
/// lines, and a `find` killed by SIGPIPE is a traversal that was stopped on
/// purpose. (`sort` buffers all of `find`'s output before printing any of it,
/// so `find` has normally finished by then; tolerating the signal keeps the
/// check correct for the case where it has not.)
const STATUS_CHECK_SH: &str = r#"
st=("${PIPESTATUS[@]}")
case "${st[0]}" in
  0 | 141) ;;
  *) echo "find failed: $p" >&2; exit 1 ;;
esac
"#;

/// `dir_list`: the direct children of the resolved path, one level only.
/// `-mindepth 1` is what keeps the starting point out of its own listing.
const LIST_SH: &str = r#"find "$p" -mindepth 1 -maxdepth 1"#;

/// `dir_walk`: every entry below the resolved path, deepest `$2`.
///
/// `$2` is already `max_depth + 1` (see [`client_dir_walk`]): `find` counts the
/// starting point as depth 0 while the tool's own `max_depth` counts entries
/// below it, so this is also what makes `max_depth = 0` mean "direct children
/// only" on both machines.
const WALK_SH: &str = r#"find "$p" -mindepth 1 -maxdepth "$2""#;

/// The `$0` placeholder the scripts above expect. Bash's `-c` has no other way
/// to place a value in `$1`: the name is required syntactically, but nothing
/// reads it. Distinct per tool so a failure message naming the command says
/// which one it was.
const LIST_ARGV0: &str = "client_dir_list";
const WALK_ARGV0: &str = "client_dir_walk";

/// Assemble a listing script: prologue, `find`, ordering, an optional `head`,
/// the classifier, and the epilogue that reads `find`'s status.
fn find_script(pipe: &str, head: &str, epilogue: &str) -> String {
    format!(
        "{RESOLVE_TILDE_SH}{LISTING_PROLOGUE_SH}{pipe}{ORDER_SH}{head}\n{{\n{CLASSIFY_LINE}}}\n{epilogue}"
    )
}

/// The script [`client_dir_list`] runs, assembled here so that the tests — the
/// ones that run it through a real shell above all — exercise the same text the
/// tool does.
pub(crate) fn list_script() -> String {
    find_script(LIST_SH, "", STATUS_CHECK_SH)
}

/// The script [`client_dir_walk`] runs.
pub(crate) fn walk_script() -> String {
    find_script(WALK_SH, WALK_HEAD_SH, STATUS_CHECK_SH)
}

/// Turn the script's `E`/`D`/`F` lines into what `dir_list`/`dir_walk` return on
/// the agent's own machine: one entry per line, directories with a trailing
/// slash, and `(empty) <path>` when there is nothing.
///
/// **The order is the script's, and it is not re-sorted here.** The script
/// emits the agent's own pre-order DFS (see [`ORDER_SH`]) because the walk's
/// `head` has to cut that order's prefix; sorting again *here* — by the shown
/// string, or by the path — would undo exactly the ordering the script went to
/// the trouble of producing, and would put `sub.txt` before `sub/inner.txt`,
/// which is the bug that led here. One owner for the order, and it is the side
/// that can also cap the traversal.
///
/// `limit` is `max_entries + 1` (or `usize::MAX` for a listing that cannot
/// truncate): one more than the caller will show, so "there was more" is
/// decidable without a second round trip. The second half of the return is
/// whether the script's own output exceeded it.
///
/// An `E` line (the prologue's announcement) is where `(empty)` gets its path
/// from: both machines then name the path they *resolved*, and for a client
/// that is a path only its own shell could resolve (`~/x` against a `$HOME`
/// this side does not share). A script that announces nothing falls back to the
/// `path` the caller passed.
///
/// A line that is not shaped as one of those three is dropped rather than
/// shown: the script is the only writer on the happy path, but a filename
/// containing a newline arrives as lines of its own and a stray diagnostic on
/// stdout must not become an entry.
pub(crate) fn shape_entries(stdout: &str, path: &str, limit: usize) -> (Vec<String>, bool) {
    let mut resolved: Option<&str> = None;
    let mut entries: Vec<String> = Vec::new();
    for line in stdout.lines() {
        let Some((kind, name)) = line.split_once('\t') else {
            continue;
        };
        match kind {
            "E" => resolved = Some(name),
            "D" => entries.push(format!("{name}/")),
            "F" => entries.push(name.to_string()),
            _ => {}
        }
    }
    let truncated = entries.len() > limit;
    entries.truncate(limit);
    if entries.is_empty() {
        return (vec![format!("(empty) {}", resolved.unwrap_or(path))], false);
    }
    (entries, truncated)
}

/// `dir_list` against the editor's machine: one `find`, then this side's
/// formatting.
///
/// The context on a failure is the agent side's own sentence, so the same
/// situation reads the same way whichever machine it happened on.
pub(crate) async fn client_dir_list(
    client: &std::sync::Arc<dyn AcpClient>,
    input: &serde_json::Value,
) -> Result<String> {
    let path = input["path"].as_str().context("missing 'path'")?;
    let stdout = run_client_bash(client, &list_script(), LIST_ARGV0, &[path.to_string()])
        .await
        .with_context(|| format!("Failed to list '{path}'"))?;
    let (entries, _) = shape_entries(&stdout, path, usize::MAX);
    Ok(entries.join("\n"))
}

/// `dir_walk` against the editor's machine. Same script as `dir_list` with the
/// two bounds as positional arguments and the agent-side truncation marker
/// appended when the cap was hit.
pub(crate) async fn client_dir_walk(
    client: &std::sync::Arc<dyn AcpClient>,
    input: &serde_json::Value,
) -> Result<String> {
    let path = input["path"].as_str().context("missing 'path'")?;
    let max_depth = input["max_depth"].as_u64().unwrap_or(5).min(20);
    let max_entries = input["max_entries"].as_u64().unwrap_or(500).clamp(1, 5000) as usize;
    let stdout = run_client_bash(
        client,
        &walk_script(),
        WALK_ARGV0,
        &[
            // `max_depth = 0` means "direct children only", matching the agent
            // side -- hence `+ 1`, since `find -maxdepth` counts the starting
            // point as depth 0.
            (max_depth + 1).to_string(),
            (max_entries + 1).to_string(),
        ],
    )
    .await
    .with_context(|| format!("Failed to walk '{path}'"))?;
    let (mut entries, truncated) = shape_entries(&stdout, path, max_entries);
    if truncated {
        // The agent side's own sentence, from the one place it is written.
        entries.push(truncation_marker(max_entries));
    }
    Ok(entries.join("\n"))
}

// ---------------------------------------------------------------------------
// The one-shot command: formatting, and the timeout policy
// ---------------------------------------------------------------------------

/// The cap on how long the one-shot `shell` waits on the client. Past this
/// the command keeps running and the caller gets its handle back instead of
/// a result — see [`format_timed_out`] for why releasing (which the ACP
/// schema defines as killing) is wrong here.
const DEFAULT_TIMEOUT_SECS: u64 = 120;
const MAX_TIMEOUT_SECS: u64 = 600;

/// Clamp a requested timeout (seconds) to `(0, MAX_TIMEOUT_SECS]`,
/// defaulting to `DEFAULT_TIMEOUT_SECS` when the caller didn't ask for one.
/// Pulled out of `ShellTool::execute` so the cap can be tested directly
/// instead of a test waiting out a real timeout.
pub(crate) fn clamp_timeout(requested: Option<u64>) -> std::time::Duration {
    std::time::Duration::from_secs(
        requested
            .unwrap_or(DEFAULT_TIMEOUT_SECS)
            .clamp(1, MAX_TIMEOUT_SECS),
    )
}

/// Render an exit status the same way regardless of caller —
/// `format_finished` (the one-shot path) and `ClientShellOutput` (the
/// long-running path) both need this tail, and duplicating the three-way
/// match would let the two drift apart. `pub(crate)` so `skill_tools`'s
/// terminal-fallback and index-resolver failures are worded the same way
/// rather than inventing a second phrasing.
pub(crate) fn format_exit_status(status: &ExitStatus) -> String {
    match (&status.exit_code, &status.signal) {
        (Some(code), _) => format!("\n[exit code: {code}]"),
        (None, Some(signal)) => format!("\n[terminated by signal: {signal}]"),
        (None, None) => "\n[exit status unknown]".to_string(),
    }
}

/// Render a finished command's output for the model: the (possibly
/// truncated) text plus how it ended.
pub(crate) fn format_finished(output: &TerminalOutput, status: &ExitStatus) -> String {
    let mut out = output.output.clone();
    if output.truncated {
        out.push_str("\n[output truncated]");
    }
    out.push_str(&format_exit_status(status));
    out
}

/// Render the message for a one-shot command that outlived its timeout: the
/// terminal was left running rather than killed, and the model must not
/// re-run the command because of this call alone.
///
/// # Why a timed-out client command is not killed
///
/// ACP's `terminal/release` kills the command it releases — the schema says
/// so explicitly, the same way `terminal/kill` does. Releasing on timeout
/// would therefore throw away a build that has already run for however long
/// the timeout allowed, and for a non-idempotent command (`git push`, a
/// migration, a script that writes files) a retry after that would run it a
/// second time.
///
/// So on timeout `ShellTool::execute`'s ACP branch releases nothing. The
/// terminal keeps running and the handle is handed back in the result text
/// — and tracked in `ServeState.acp_terminals`, the same registry
/// `ClientShellStart` uses, so it counts against the session's cap and shows
/// up if the model has to list what it is holding — so the model can poll it
/// with `shell_output` or stop it with `shell_kill`. The
/// decision to kill is
/// left to the model or the human, never made here on their behalf. This is
/// a deliberate departure from what the protocol's own `terminal/kill` doc
/// suggests (kill on timeout and collect the output).
///
/// The one new risk this creates is the model reading a timeout as a failure
/// and re-running the command — which for a non-idempotent command is
/// exactly the outcome not-releasing was meant to avoid. The result text is
/// worded so that misreading is not possible, and ends with an explicit
/// instruction not to re-run.
///
/// `pub(crate)` because the formatting lives here while the branch that
/// calls it lives in `ShellTool::execute`, so that a `shell` and a
/// `shell_start` timeout read the same way.
pub(crate) fn format_timed_out(handle: &TerminalHandle, timeout: std::time::Duration) -> String {
    format!(
        "[timed out after {}s — the command is still running as terminal {handle}. \
         It was not killed. Use shell_output to check on it, or \
         shell_kill to stop it. \
         Do not re-run the command.]",
        timeout.as_secs()
    )
}

// ---------------------------------------------------------------------------
// shell_start / shell_output / shell_kill
// ---------------------------------------------------------------------------

/// How many terminals one session may hold at once.
///
/// A ceiling rather than a cleanup: nothing here is released on
/// disconnect (see `ServeState::acp_terminals`'s doc), so without a
/// cap a model that keeps starting commands and never killing them
/// would accumulate processes on the user's machine indefinitely.
/// Refusing the ninth — and naming the eight it is holding — makes the
/// model clean up rather than the agent guess which one is safe to
/// kill.
pub(crate) const MAX_TERMINALS_PER_SESSION: usize = 8;

/// The refusal `ClientShellStart` returns when the session is already
/// at the cap. Pulled out so the wording — and the handle substrings
/// (`t1`, `t{MAX_TERMINALS_PER_SESSION}`) the cap test keys on — lives
/// in one place.
///
/// `held.handles` alone can be shorter than `MAX_TERMINALS_PER_SESSION`
/// even though the session really is at the cap: a concurrent call
/// elsewhere in this same turn may be mid-`create_terminal`, holding a
/// reservation with no handle yet. Naming only `held.handles` in that
/// case would tell the model it holds fewer terminals than the cap it
/// is being refused against, with no way to reconcile the two — so any
/// in-flight reservations are appended as a count instead of being
/// silently dropped from the listing.
pub(crate) fn cap_error(held: &crate::tools::acp_client::CapHeld) -> anyhow::Error {
    let mut parts: Vec<String> = held.handles.iter().map(TerminalHandle::to_string).collect();
    if held.reservations > 0 {
        parts.push(format!(
            "{} more still starting (no handle yet)",
            held.reservations
        ));
    }
    anyhow::anyhow!(
        "already holding the maximum of {MAX_TERMINALS_PER_SESSION} terminals for this \
         session: {}. Use shell_kill to free one before starting another.",
        parts.join(", ")
    )
}

/// Start a long-running command on the editor's machine and return
/// immediately with a terminal handle, rather than waiting for it to
/// finish the way `shell` does.
///
/// The handle is tracked against the session
/// (`ServeState.acp_terminals`) the moment the client hands it back —
/// before that, `shell_output`/`shell_kill` would have
/// nothing to check the model's handle against, and the cap below
/// would never see it.
///
/// The cap is checked *before* calling `create_terminal`: a session
/// already holding `MAX_TERMINALS_PER_SESSION` is refused without a
/// round trip to the editor, and the refusal names every handle held
/// so the model knows what to free.
pub struct ClientShellStart {
    spec: ToolSpec,
}

impl ClientShellStart {
    pub fn new() -> Self {
        Self {
            spec: ToolSpec {
                name: "shell_start".into(),
                description: format!(
                    "Start a long-running command on the machine the connected \
                    editor is running on — NOT this agent's own machine — and return \
                    immediately with a terminal handle instead of waiting for it to finish. \
                    Prefer this over `shell` for a command expected to keep running \
                    (a dev server, a watch task) or that may outlast a reasonable wait. \
                    Check on it with shell_output and stop it with shell_kill. \
                    A session may hold at most {MAX_TERMINALS_PER_SESSION} terminals at \
                    once; starting one more than that is refused, naming the handles \
                    already held, until one is freed. "
                )
                .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "command": {
                            "type": "string",
                            "description": "The command to run (not a shell string — no pipes or redirection)."
                        },
                        "args": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "Arguments to pass to the command."
                        },
                        "cwd": {
                            "type": "string",
                            "description": "Working directory on the editor's machine. Defaults to the session's cwd."
                        }
                    },
                    "required": ["command"]
                }),
            },
        }
    }
}

impl Default for ClientShellStart {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for ClientShellStart {
    fn kind(&self) -> ToolKind {
        ToolKind::Execute
    }

    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn execute(&self, input: &serde_json::Value) -> Result<String> {
        let command = input["command"].as_str().context("missing 'command'")?;
        let args: Vec<String> = input["args"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        let cwd = input["cwd"].as_str();

        let client = current_acp_client().ok_or_else(|| anyhow::anyhow!(NO_EDITOR))?;

        // Reserve-then-create, not read-then-write: see
        // `AcpClient::try_reserve_terminal_slot`'s doc. `run_llm_turn`
        // runs a turn's permitted calls concurrently, so one assistant
        // message containing several `shell_start` blocks must
        // not let them all read the count before any of them wrote it
        // back — a real, reachable way to bypass the cap within one
        // turn, not just across concurrent prompts.
        let reservation = client
            .try_reserve_terminal_slot()
            .await
            .map_err(|held| cap_error(&held))?;

        // As in the one-shot path: if `create_terminal` errors or this call
        // is cancelled before it returns, `reservation` is dropped
        // without reaching `track_terminal`, and its `Drop` frees the
        // slot on its own.
        let handle = client
            .create_terminal(command, &args, cwd, Some(OUTPUT_CAP_BYTES as u64))
            .await?;
        client.track_terminal(reservation, handle.clone()).await;
        Ok(format!(
            "Started terminal {handle}. Use shell_output to check on it, or \
             shell_kill to stop it."
        ))
    }
}

/// Check on a command started by `shell_start` (or left running
/// by a `shell` timeout): its output so far, whether it has
/// finished, and its exit status if it has.
///
/// # An output error does NOT untrack the handle
///
/// The sanctioned reason to drop tracking without a release is "the
/// client says this handle doesn't exist" — but a `terminal_output`
/// error is not proof of that. A transient failure (the client
/// mid-reconnect, a timed-out request) surfaces through the same `Err`
/// as a genuinely unknown handle, and `AcpClient` gives no way to tell
/// the two apart. Untracking on every error would silently drop a
/// terminal that is still running: it stops counting against the cap
/// and disappears from the listing the model is told to clean up from,
/// with nothing left for the model to do about it.
///
/// So this tool leaves the handle tracked on any error and just
/// reports the client's message. Over-counting is the recoverable
/// direction — `shell_kill` untracks unconditionally (see its
/// doc), so the model can always clear a handle it no longer needs by
/// killing it, even if this tool keeps failing on it.
pub struct ClientShellOutput {
    spec: ToolSpec,
}

impl ClientShellOutput {
    pub fn new() -> Self {
        Self {
            spec: ToolSpec {
                name: "shell_output".into(),
                description: "Check on a command started with shell_start (or left \
                    running by a shell call that timed out): its output so far, \
                    whether it has finished, and its exit status if it has. Does not stop \
                    the command."
                    .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "terminal": {
                            "type": "string",
                            "description": "The terminal handle returned by shell_start \
                                (or by a shell call that timed out)."
                        }
                    },
                    "required": ["terminal"]
                }),
            },
        }
    }
}

impl Default for ClientShellOutput {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for ClientShellOutput {
    fn kind(&self) -> ToolKind {
        ToolKind::Read
    }

    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn execute(&self, input: &serde_json::Value) -> Result<String> {
        let terminal = input["terminal"].as_str().context("missing 'terminal'")?;
        let handle = TerminalHandle(terminal.to_string());

        let client = current_acp_client().ok_or_else(|| anyhow::anyhow!(NO_EDITOR))?;
        // Deliberately does NOT untrack on error — see this tool's doc.
        // An error here could be transient rather than "this handle is
        // truly gone," and untracking a terminal that is still running
        // would be unrecoverable; leaving it tracked is not.
        let output = client.terminal_output(&handle).await?;

        let mut out = output.output.clone();
        if output.truncated {
            out.push_str("\n[output truncated]");
        }
        match &output.exit_status {
            Some(status) => out.push_str(&format_exit_status(status)),
            None => out.push_str("\n[still running]"),
        }
        Ok(out)
    }
}

/// Stop a command started by `shell_start` (or left running by
/// a `shell` timeout) and free its terminal handle.
///
/// Kills, then releases: ACP's `terminal/kill` alone leaves the handle
/// valid, so a caller that stopped there would leak it against the cap
/// forever. `release_terminal` — which the ACP schema defines as also
/// killing the command, redundantly with the kill just sent — is what
/// invalidates the handle on the client's side.
///
/// # Untracks unconditionally, even if `kill`/`release` error
///
/// This is the other half of `ClientShellOutput`'s asymmetry (see its
/// doc): an output error leaves a handle tracked because untracking a
/// terminal that might still be running is unrecoverable. A kill is
/// the opposite case — it is the action the cap's own refusal message
/// tells the model to take to free a slot, and a handle the client has
/// genuinely forgotten would otherwise error here forever and jam that
/// slot for good. So both wire calls are attempted and the handle is
/// dropped from tracking regardless of whether either succeeded; only
/// then does a real error from either call propagate to the model.
/// Over-counting from here is recoverable (the model can call this
/// tool again, or check with `shell_output`); a permanently
/// stuck slot is not.
pub struct ClientShellKill {
    spec: ToolSpec,
}

impl ClientShellKill {
    pub fn new() -> Self {
        Self {
            spec: ToolSpec {
                name: "shell_kill".into(),
                description: format!(
                    "Stop a command started with shell_start (or left \
                    running by a shell call that timed out) and free its terminal \
                    handle. Use this to make room under the {MAX_TERMINALS_PER_SESSION}-\
                    terminal cap, or to give up on a command that is no longer needed. "
                )
                .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "terminal": {
                            "type": "string",
                            "description": "The terminal handle to stop and free."
                        }
                    },
                    "required": ["terminal"]
                }),
            },
        }
    }
}

impl Default for ClientShellKill {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for ClientShellKill {
    fn kind(&self) -> ToolKind {
        ToolKind::Execute
    }

    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn execute(&self, input: &serde_json::Value) -> Result<String> {
        let terminal = input["terminal"].as_str().context("missing 'terminal'")?;
        let handle = TerminalHandle(terminal.to_string());

        let client = current_acp_client().ok_or_else(|| anyhow::anyhow!(NO_EDITOR))?;

        // Both attempted before either `?` — see this tool's doc for
        // why: a kill failure must not skip the release attempt, and
        // the handle must come out of tracking even if both failed.
        let kill = client.kill_terminal(&handle).await;
        let release = client.release_terminal(&handle).await;
        client.untrack_terminal(&handle).await;
        kill?;
        release?;

        Ok(format!("Stopped and released terminal {handle}."))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::serve::ServeState;
    use crate::tools::acp_client::tests::FakeClient;
    use crate::tools::acp_client::{AcpClient, scope_acp_client};
    use serde_json::json;
    use std::sync::Arc;

    /// The session id [`shell_test_state`]'s `FakeClient` tracks
    /// terminals under, and the key the tests below read back off
    /// `ServeState.acp_terminals`.
    const TEST_SESSION_ID: &str = "client-tools-test-session";

    /// A `ServeState` and a `FakeClient` wired to share one terminal
    /// registry under [`TEST_SESSION_ID`] — the same relationship
    /// `AcpProgress`/`AcpClientHandle` have to a real `ServeState` in
    /// production (`src/serve/acp.rs`), so driving a tool purely
    /// through the fake is visible on `state.acp_terminals` exactly the
    /// way it would be for a real session.
    async fn shell_test_state() -> (Arc<ServeState>, Arc<FakeClient>) {
        let state = ServeState::for_test(true);
        // Field assignment rather than `FakeClient { .., ..Default::default() }`:
        // struct-update syntax requires every field to be visible from
        // the call site, including the ones left untouched, and most of
        // `FakeClient`'s fields are private to `acp_client`'s own test
        // module.
        let mut fake = FakeClient::default();
        fake.terminal_session = TEST_SESSION_ID.to_string();
        fake.terminals = Arc::clone(&state.acp_terminals);
        (state, Arc::new(fake))
    }

    /// Drives the real connection-teardown path
    /// (`crate::serve::acp::release_connection_sessions`) for one
    /// session id, rather than reimplementing "a connection closed"
    /// locally. The property under test is *what does not happen* —
    /// `acp_terminals` is untouched — so a test that only exercised a
    /// stand-in would not stop a future release from being added to
    /// the real path.
    async fn simulate_connection_teardown(state: &Arc<ServeState>, session_id: &str) {
        crate::serve::acp::release_connection_sessions(state, vec![session_id.to_string()]).await;
    }

    /// `line` and `limit` exist in ACP because a coding agent reads big
    /// files in pieces. Passing them through is the reason to prefer
    /// this over shelling out to `sed`.
    #[tokio::test]
    async fn the_long_running_tools_refuse_without_a_client() {
        assert!(
            ClientShellStart::new()
                .execute(&json!({"command": "ls", "args": []}))
                .await
                .unwrap_err()
                .to_string()
                .contains("no editor")
        );
        assert!(
            ClientShellOutput::new()
                .execute(&json!({"terminal": "t1"}))
                .await
                .unwrap_err()
                .to_string()
                .contains("no editor")
        );
        assert!(
            ClientShellKill::new()
                .execute(&json!({"terminal": "t1"}))
                .await
                .unwrap_err()
                .to_string()
                .contains("no editor")
        );
    }

    /// The handle has to be recorded against the session, because that
    /// is what a reconnecting client's next turn will look it up by.
    #[tokio::test]
    async fn start_records_the_handle_against_the_session() {
        let (state, fake) = shell_test_state().await;
        let client: Arc<dyn AcpClient> = Arc::clone(&fake) as Arc<dyn AcpClient>;
        scope_acp_client(client, async {
            ClientShellStart::new()
                .execute(&json!({"command": "cargo", "args": ["test"]}))
                .await
                .unwrap();
        })
        .await;

        let held = state.acp_terminals.lock().unwrap();
        assert_eq!(held.get(TEST_SESSION_ID).map(Vec::len), Some(1));
    }

    /// `kill` alone leaves the handle valid — the protocol says so, and
    /// says to release it afterwards. Doing only half would leak a
    /// handle against the cap forever.
    #[tokio::test]
    async fn kill_stops_the_command_and_then_frees_the_handle() {
        let (state, fake) = shell_test_state().await;
        let client: Arc<dyn AcpClient> = Arc::clone(&fake) as Arc<dyn AcpClient>;
        scope_acp_client(client, async {
            ClientShellStart::new()
                .execute(&json!({"command": "sleep", "args": ["999"]}))
                .await
                .unwrap();
            ClientShellKill::new()
                .execute(&json!({"terminal": "t1"}))
                .await
                .unwrap();
        })
        .await;

        assert_eq!(
            fake.killed.lock().unwrap().len(),
            1,
            "the command is stopped"
        );
        assert_eq!(
            fake.released.lock().unwrap().len(),
            1,
            "and the handle freed"
        );
        assert!(
            state
                .acp_terminals
                .lock()
                .unwrap()
                .get(TEST_SESSION_ID)
                .is_none_or(Vec::is_empty),
            "and it is no longer tracked"
        );
    }

    /// The cap has to name what is holding it. A bare "too many
    /// terminals" leaves the model with nothing to act on.
    #[tokio::test]
    async fn the_cap_refuses_and_lists_what_is_held() {
        let (_state, fake) = shell_test_state().await;
        fake.hand_out_distinct_handles();
        let client: Arc<dyn AcpClient> = Arc::clone(&fake) as Arc<dyn AcpClient>;

        let refusal = scope_acp_client(client, async {
            for _ in 0..MAX_TERMINALS_PER_SESSION {
                ClientShellStart::new()
                    .execute(&json!({"command": "sleep", "args": ["999"]}))
                    .await
                    .unwrap();
            }
            ClientShellStart::new()
                .execute(&json!({"command": "one", "args": ["too", "many"]}))
                .await
                .unwrap_err()
                .to_string()
        })
        .await;

        assert!(refusal.contains("t1"), "names a held handle: {refusal}");
        assert!(
            refusal.contains(&format!("t{MAX_TERMINALS_PER_SESSION}")),
            "names the last one too: {refusal}"
        );
        assert_eq!(
            fake.creates.lock().unwrap().len(),
            MAX_TERMINALS_PER_SESSION,
            "the refused call must not have reached the client"
        );
    }

    /// Review round 1, Finding 2: an output error alone is not proof the
    /// client has forgotten the handle — it could be transient (a
    /// reconnect, a timed-out request) — so it must not untrack a
    /// terminal that might still be running. Only `shell_kill`
    /// untracks unconditionally, because a handle the model explicitly
    /// asked to kill is exactly the case the cap's refusal message
    /// points the model at. This is a stronger pair than "an unknown
    /// handle is dropped from tracking" was: it proves the asymmetry in
    /// both directions, not just that *something* untracks eventually.
    #[tokio::test]
    async fn an_unknown_handle_is_dropped_from_tracking() {
        let (state, fake) = shell_test_state().await;
        let client: Arc<dyn AcpClient> = Arc::clone(&fake) as Arc<dyn AcpClient>;

        let output_err = scope_acp_client(Arc::clone(&client), async {
            ClientShellStart::new()
                .execute(&json!({"command": "sleep", "args": ["999"]}))
                .await
                .unwrap();

            fake.make_output_fail_with("no such terminal");
            ClientShellOutput::new()
                .execute(&json!({"terminal": "t1"}))
                .await
                .unwrap_err()
                .to_string()
        })
        .await;

        assert!(
            output_err.contains("no such terminal"),
            "the client's words reach the model: {output_err}"
        );
        assert_eq!(
            state
                .acp_terminals
                .lock()
                .unwrap()
                .get(TEST_SESSION_ID)
                .map(Vec::len),
            Some(1),
            "an output error alone must not drop tracking — the terminal could still be \
             running, and untracking it would lose it for good"
        );
        assert!(
            fake.released.lock().unwrap().is_empty(),
            "an output error must not release either — only a kill does that"
        );

        // Even a client that keeps saying "no such terminal" on kill
        // and release must not leave the slot stuck forever.
        fake.make_kill_fail_with("no such terminal");
        let kill_err = scope_acp_client(client, async {
            ClientShellKill::new()
                .execute(&json!({"terminal": "t1"}))
                .await
                .unwrap_err()
                .to_string()
        })
        .await;

        assert!(
            kill_err.contains("no such terminal"),
            "the failure is still reported: {kill_err}"
        );
        assert!(
            state
                .acp_terminals
                .lock()
                .unwrap()
                .get(TEST_SESSION_ID)
                .is_none_or(Vec::is_empty),
            "but a kill frees the handle from tracking regardless of whether the client \
             errors on it — otherwise the slot the cap points the model at could never \
             be freed"
        );
    }

    /// The property this whole task is shaped around. `terminal/release`
    /// kills the command, so a dropped socket must not trigger one — a
    /// network blip would otherwise kill the user's build.
    ///
    /// What this does and does not fence: `release_connection_sessions`
    /// takes only `&Arc<ServeState>` and a list of session ids — it has
    /// no route to any `AcpClient` — so `fake.released`/`fake.killed`
    /// staying empty is true by construction here and can never fail.
    /// The load-bearing assertion is the last one: the handle is still
    /// tracked after teardown. A release added to `serve_connection`'s
    /// teardown *outside* `release_connection_sessions` (rather than
    /// inside it) would not be caught by this test at all — only by
    /// code review of that call site, or a wire-level test in
    /// `src/serve/acp.rs`.
    #[tokio::test]
    async fn a_connection_ending_releases_nothing() {
        let (state, fake) = shell_test_state().await;
        let client: Arc<dyn AcpClient> = Arc::clone(&fake) as Arc<dyn AcpClient>;
        scope_acp_client(client, async {
            ClientShellStart::new()
                .execute(&json!({"command": "cargo", "args": ["test"]}))
                .await
                .unwrap();
        })
        .await;

        simulate_connection_teardown(&state, TEST_SESSION_ID).await;

        assert!(
            fake.released.lock().unwrap().is_empty(),
            "release kills the command; a lost socket is not a reason to"
        );
        assert!(fake.killed.lock().unwrap().is_empty());
        assert_eq!(
            state
                .acp_terminals
                .lock()
                .unwrap()
                .get(TEST_SESSION_ID)
                .map(Vec::len),
            Some(1),
            "and the handle stays addressable for a client that reconnects"
        );
    }

    /// Task 6's ruling that the plan itself does not state: a one-shot
    /// `shell` call that outruns its timeout must be tracked too
    /// — not just handed back in the result text. Otherwise the handle
    /// escapes both the cap and the "what is holding this session"
    /// listing, and the model is told to clean up while the very thing
    /// it needs to clean up stays invisible.
    /// Review round 1, Finding 1: the one-shot path must respect the
    /// same cap `shell_start` does. `shell`'s timeout
    /// branch tracks a handle (previous test), so without a cap check
    /// on this path too, a model looping `shell` with a short
    /// `timeout_secs` could accumulate live processes past the cap the
    /// same way looping `shell_start` would — exactly what
    /// `MAX_TERMINALS_PER_SESSION` exists to prevent.
    // -----------------------------------------------------------------
    // Final review, Fix 1 & Fix 2
    // -----------------------------------------------------------------

    /// Fix 1, item 1: a `wait_for_terminal_exit` error (the client
    /// mid-reconnect, an RPC timeout) is not proof the command has
    /// stopped — it is still running on the user's machine. The handle
    /// must stay tracked so the model can still poll or kill it later;
    /// losing it here is exactly the "under-counting loses a live
    /// process" outcome the design rules out.
    /// Fix 1, item 3: a `release_terminal` failure must not discard
    /// output that was already collected successfully. A finished
    /// build's output is real work; throwing it away because the
    /// unrelated release call that follows failed would be worse than
    /// reporting both.
    /// Fix 2: `run_llm_turn` executes a turn's permitted tool calls
    /// concurrently (`futures_util::future::join_all`, `src/serve/mod.rs`),
    /// so one assistant message containing several `shell_start`
    /// blocks runs them all at once against the same session. The old
    /// code read `tracked_terminals()` and wrote `track_terminal()` as
    /// two separate steps, so every concurrent call could read the
    /// count before any of them wrote it back — bypassing the cap
    /// within a single turn, not just across the (unsupported)
    /// concurrent-prompts case. `try_reserve_terminal_slot` closes that
    /// by making the check and the reservation one atomic step.
    ///
    /// `FakeClient::create_terminal` yields once (simulating the real
    /// RPC round trip `AcpClientHandle::create_terminal` makes) so the
    /// concurrent calls below actually interleave between their reserve
    /// and their track step — without that, this test would pass even
    /// against the old, buggy read-then-write code.
    #[tokio::test]
    async fn concurrent_starts_cannot_exceed_the_cap() {
        let (state, fake) = shell_test_state().await;
        fake.hand_out_distinct_handles();
        let client: Arc<dyn AcpClient> = Arc::clone(&fake) as Arc<dyn AcpClient>;

        let attempts = MAX_TERMINALS_PER_SESSION + 4;
        let inputs: Vec<serde_json::Value> = (0..attempts)
            .map(|_| json!({"command": "sleep", "args": ["999"]}))
            .collect();
        let tool = ClientShellStart::new();

        let outcomes = scope_acp_client(client, async {
            futures_util::future::join_all(inputs.iter().map(|input| tool.execute(input))).await
        })
        .await;

        let succeeded = outcomes.iter().filter(|r| r.is_ok()).count();
        assert_eq!(
            succeeded, MAX_TERMINALS_PER_SESSION,
            "one turn's concurrent shell_start calls must not exceed the cap: {outcomes:?}"
        );
        assert_eq!(
            state
                .acp_terminals
                .lock()
                .unwrap()
                .get(TEST_SESSION_ID)
                .map(Vec::len),
            Some(MAX_TERMINALS_PER_SESSION),
            "the registry itself must never hold more than the cap, even transiently"
        );
    }

    // -----------------------------------------------------------------
    // Reservation-leak fix: cancellation between reserve and track
    // -----------------------------------------------------------------

    /// The regression this whole task exists to close, reproduced and
    /// then disproved.
    ///
    /// Before `TerminalReservation` existed, the placeholder
    /// `try_reserve_terminal_slot` pushed was freed only by
    /// `track_terminal` or an explicit `untrack_terminal` on
    /// `create_terminal` failure — neither of which runs when the
    /// call's future is dropped in between (a cancelled turn: Escape in
    /// an editor, a dropped socket firing `connection_cancel`, exactly
    /// what `create_terminal`'s RPC being in flight is the routine case
    /// for). The placeholder stayed in the registry forever, and after
    /// `MAX_TERMINALS_PER_SESSION` such cancellations the session could
    /// never start another terminal again, on this connection or any
    /// reconnect.
    ///
    /// Cancelling `MAX_TERMINALS_PER_SESSION + 1` times — one more than
    /// the cap — and then still succeeding is the sharpest version of
    /// this: if even a single cancellation leaked its slot, the cap
    /// would already be hit and the final start below would be refused
    /// instead of succeeding.
    #[tokio::test]
    async fn a_dropped_reservation_does_not_leak_the_slot() {
        let (state, fake) = shell_test_state().await;
        // Parks every `create_terminal` call after it records itself
        // and after the reservation above it has already landed in the
        // registry — the same window a real cancellation drops the
        // turn's future in.
        fake.make_create_terminal_hang();
        let client: Arc<dyn AcpClient> = Arc::clone(&fake) as Arc<dyn AcpClient>;

        for _ in 0..(MAX_TERMINALS_PER_SESSION + 1) {
            let task_client = Arc::clone(&client);
            let join = tokio::spawn(scope_acp_client(task_client, async move {
                ClientShellStart::new()
                    .execute(&json!({"command": "sleep", "args": ["999"]}))
                    .await
            }));

            // Let the spawned task actually reserve its slot and reach
            // the hang inside `create_terminal` before cancelling it —
            // otherwise this loop would abort a task that never got far
            // enough to exercise the drop path at all.
            for _ in 0..200 {
                if state
                    .acp_terminals
                    .lock()
                    .unwrap()
                    .get(TEST_SESSION_ID)
                    .map(Vec::len)
                    == Some(1)
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
            assert_eq!(
                state
                    .acp_terminals
                    .lock()
                    .unwrap()
                    .get(TEST_SESSION_ID)
                    .map(Vec::len),
                Some(1),
                "the reservation must be visible before it is cancelled, or this test does \
                 not exercise the drop path this fix is about"
            );

            // The cancellation this fix is about: the turn's future is
            // dropped mid-RPC. `abort` reproduces exactly that for a
            // spawned task.
            join.abort();
            let _ = join.await;

            assert!(
                state
                    .acp_terminals
                    .lock()
                    .unwrap()
                    .get(TEST_SESSION_ID)
                    .is_none_or(Vec::is_empty),
                "a reservation whose future is dropped before track_terminal must free its \
                 slot immediately, not leak it"
            );
        }

        // The proof that matters: after MAX_TERMINALS_PER_SESSION + 1
        // cancellations, a fresh start still succeeds. Before this fix,
        // each cancellation above would have permanently cost the
        // session a slot, and it would already be stuck at the cap.
        fake.let_create_terminal_finish();
        scope_acp_client(client, async {
            ClientShellStart::new()
                .execute(&json!({"command": "sleep", "args": ["999"]}))
                .await
                .expect(
                    "the session must still be able to start a terminal after repeated \
                     cancellations",
                )
        })
        .await;
    }

    // -----------------------------------------------------------------
    // Reservation-leak fix: cap_error accounts for in-flight reservations
    // -----------------------------------------------------------------

    /// The second half of the trap this task closes: a model refused at
    /// the cap while some of what it holds is still an in-flight
    /// reservation (no handle yet, because `create_terminal` hasn't
    /// returned) must not be shown a listing shorter than the maximum
    /// it is told it hit. `try_reserve_terminal_slot` cannot name a
    /// reservation as a handle — there isn't one yet — so `cap_error`
    /// has to account for it some other way instead of just omitting
    /// it.
    #[test]
    fn cap_error_accounts_for_in_flight_reservations() {
        let held = crate::tools::acp_client::CapHeld {
            handles: vec![
                TerminalHandle("t1".to_string()),
                TerminalHandle("t2".to_string()),
                TerminalHandle("t3".to_string()),
            ],
            reservations: 5,
        };
        let message = cap_error(&held).to_string();

        assert!(
            message.contains(&MAX_TERMINALS_PER_SESSION.to_string()),
            "still names the cap itself: {message}"
        );
        for id in ["t1", "t2", "t3"] {
            assert!(
                message.contains(id),
                "still names every real handle held: {message}"
            );
        }
        // The bug this guards against: a refusal that claims "the
        // maximum of 8" while naming only 3 handles and saying nothing
        // about the other 5 gives the model nothing to reconcile the
        // two numbers with.
        assert!(
            message.contains('5'),
            "the 5 in-flight reservations must be accounted for, not silently dropped from \
             a listing shorter than the 8 the message claims: {message}"
        );
    }

    /// End-to-end version of the same fix: eight concurrent
    /// `shell_start` calls that are all still mid-`create_terminal`
    /// (parked, so none of them has resolved into a real handle yet)
    /// must still be reflected in the refusal a ninth concurrent call
    /// gets — as a count, since none of the eight has a handle for the
    /// message to name.
    #[tokio::test]
    async fn the_cap_refusal_accounts_for_reservations_still_in_flight() {
        let (state, fake) = shell_test_state().await;
        fake.make_create_terminal_hang();
        let client: Arc<dyn AcpClient> = Arc::clone(&fake) as Arc<dyn AcpClient>;

        let mut holders = Vec::new();
        for _ in 0..MAX_TERMINALS_PER_SESSION {
            let task_client = Arc::clone(&client);
            holders.push(tokio::spawn(scope_acp_client(task_client, async move {
                ClientShellStart::new()
                    .execute(&json!({"command": "sleep", "args": ["999"]}))
                    .await
            })));
        }

        for _ in 0..2000 {
            if state
                .acp_terminals
                .lock()
                .unwrap()
                .get(TEST_SESSION_ID)
                .map(Vec::len)
                == Some(MAX_TERMINALS_PER_SESSION)
            {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(
            state
                .acp_terminals
                .lock()
                .unwrap()
                .get(TEST_SESSION_ID)
                .map(Vec::len),
            Some(MAX_TERMINALS_PER_SESSION),
            "all eight must be reserved (none resolved — create_terminal is parked) before \
             the refusal below is meaningful"
        );

        let refusal = scope_acp_client(Arc::clone(&client), async {
            ClientShellStart::new()
                .execute(&json!({"command": "one", "args": ["too", "many"]}))
                .await
                .unwrap_err()
                .to_string()
        })
        .await;

        assert!(
            refusal.contains(&MAX_TERMINALS_PER_SESSION.to_string()),
            "the refusal must account for every in-flight reservation, not present a \
             listing shorter than the {MAX_TERMINALS_PER_SESSION} it claims: {refusal}"
        );

        for h in holders {
            h.abort();
            let _ = h.await;
        }
    }

    // -----------------------------------------------------------------
    // The ACP half of the one-shot `shell` tool
    // -----------------------------------------------------------------
    //
    // `shell` is no longer a pair of tools: `ShellTool::execute` dispatches
    // into `client_exec` when a client is scoped. The timeout policy and the
    // tracking rules that used to belong to the old `ClientShell` tool therefore have to
    // be verified where they now run.

    /// Enough of a `ShellTool` to exercise the ACP branch: the workspace
    /// root is never read on that path (the client call's `cwd` comes from
    /// `workdir`), so any directory will do.
    fn shell_tool() -> ShellTool {
        ShellTool::new(std::env::temp_dir())
    }

    /// The whole point of the timeout: a build that outruns it keeps
    /// running, and the model is handed the handle instead of a corpse.
    /// Killing here would throw away the work and, for a non-idempotent
    /// command, run it twice.
    #[tokio::test]
    async fn a_timed_out_command_is_not_killed_and_hands_back_its_handle() {
        let fake = Arc::new(FakeClient::default());
        fake.make_exit_never_return();
        let client: Arc<dyn AcpClient> = Arc::clone(&fake) as Arc<dyn AcpClient>;

        let out = scope_acp_client(client, async {
            shell_tool()
                .execute(&json!({"command": "cargo test", "timeout": 1}))
                .await
                .unwrap()
        })
        .await;

        assert!(out.contains("still running"), "got: {out}");
        assert!(
            out.contains("t1"),
            "the handle must be in the result: {out}"
        );
        assert!(
            fake.released.lock().unwrap().is_empty(),
            "release kills the command — it must not be called on a timeout"
        );
        assert!(fake.killed.lock().unwrap().is_empty(), "nor kill");
    }

    #[tokio::test]
    async fn a_command_that_finishes_in_time_is_released() {
        let fake = Arc::new(FakeClient::default());
        let client: Arc<dyn AcpClient> = Arc::clone(&fake) as Arc<dyn AcpClient>;
        scope_acp_client(client, async {
            shell_tool()
                .execute(&json!({"command": "ls"}))
                .await
                .unwrap();
        })
        .await;
        assert_eq!(
            fake.released.lock().unwrap().len(),
            1,
            "the handle is freed"
        );
    }

    /// The cap is handed to the client so the output is cut at the source
    /// rather than shipped across the wire and cut here.
    #[tokio::test]
    async fn the_output_cap_is_passed_to_the_client() {
        let fake = Arc::new(FakeClient::default());
        let client: Arc<dyn AcpClient> = Arc::clone(&fake) as Arc<dyn AcpClient>;
        scope_acp_client(client, async {
            shell_tool()
                .execute(&json!({"command": "ls"}))
                .await
                .unwrap();
        })
        .await;
        let (_, _, _, limit) = fake.creates.lock().unwrap()[0].clone();
        assert_eq!(limit, Some(crate::tools::OUTPUT_CAP_BYTES as u64));
    }

    /// A `shell` call that outruns its timeout must be tracked too — the
    /// command is still running, so it has to count against the session's
    /// cap and be listable for cleanup.
    #[tokio::test]
    async fn a_timed_out_one_shot_is_tracked_against_the_session() {
        let (state, fake) = shell_test_state().await;
        fake.make_exit_never_return();
        let client: Arc<dyn AcpClient> = Arc::clone(&fake) as Arc<dyn AcpClient>;

        scope_acp_client(client, async {
            shell_tool()
                .execute(&json!({"command": "cargo test", "timeout": 1}))
                .await
                .unwrap();
        })
        .await;

        assert_eq!(
            state
                .acp_terminals
                .lock()
                .unwrap()
                .get(TEST_SESSION_ID)
                .map(Vec::len),
            Some(1),
            "a timed-out one-shot's handle must be tracked, or it escapes the cap and \
             the model can never see it to clean it up"
        );
    }

    /// The one-shot path must respect the same cap `shell_start` does. Its
    /// timeout branch tracks a handle (previous test), so without a cap
    /// check here a model looping `shell` with a short `timeout` could
    /// accumulate live processes past the cap the same way looping
    /// `shell_start` would — exactly what `MAX_TERMINALS_PER_SESSION`
    /// exists to prevent.
    #[tokio::test]
    async fn the_one_shot_path_is_also_capped() {
        let (_state, fake) = shell_test_state().await;
        fake.hand_out_distinct_handles();
        let client: Arc<dyn AcpClient> = Arc::clone(&fake) as Arc<dyn AcpClient>;

        let refusal = scope_acp_client(client, async {
            for _ in 0..MAX_TERMINALS_PER_SESSION {
                ClientShellStart::new()
                    .execute(&json!({"command": "sleep 999"}))
                    .await
                    .unwrap();
            }
            shell_tool()
                .execute(&json!({"command": "one too many"}))
                .await
                .unwrap_err()
                .to_string()
        })
        .await;

        assert!(refusal.contains("t1"), "names a held handle: {refusal}");
        assert_eq!(
            fake.creates.lock().unwrap().len(),
            MAX_TERMINALS_PER_SESSION,
            "the refused one-shot call must not have reached the client either"
        );
    }

    /// A `wait_for_terminal_exit` error (the client mid-reconnect, an RPC
    /// timeout) is not proof the command has stopped — it is still running
    /// on the user's machine. The handle must stay tracked so the model can
    /// still poll or kill it later; losing it here is exactly the
    /// "under-counting loses a live process" outcome the design rules out.
    #[tokio::test]
    async fn a_wait_for_exit_error_leaves_the_handle_tracked() {
        let (state, fake) = shell_test_state().await;
        fake.make_wait_fail_with("connection reset");
        let client: Arc<dyn AcpClient> = Arc::clone(&fake) as Arc<dyn AcpClient>;

        let err = scope_acp_client(client, async {
            shell_tool()
                .execute(&json!({"command": "cargo build"}))
                .await
                .unwrap_err()
                .to_string()
        })
        .await;

        assert!(err.contains("connection reset"), "got: {err}");
        assert_eq!(
            state
                .acp_terminals
                .lock()
                .unwrap()
                .get(TEST_SESSION_ID)
                .map(Vec::len),
            Some(1),
            "a wait-for-exit error must not drop tracking — the command may still be running"
        );
    }

    /// A `release_terminal` failure must not discard output that was
    /// already collected successfully. A finished build's output is real
    /// work; throwing it away because the unrelated release call that
    /// follows failed would be worse than reporting both.
    #[tokio::test]
    async fn a_release_error_still_returns_the_output_and_leaves_the_handle_tracked() {
        let (state, fake) = shell_test_state().await;
        // Also fails `kill_terminal`, but this path never calls it.
        fake.make_kill_fail_with("no such terminal");
        let client: Arc<dyn AcpClient> = Arc::clone(&fake) as Arc<dyn AcpClient>;

        let out = scope_acp_client(client, async {
            shell_tool()
                .execute(&json!({"command": "cargo build"}))
                .await
                .unwrap()
        })
        .await;

        assert!(
            out.contains("[exit status unknown]"),
            "the finished command's output must still be reported: {out}"
        );
        assert!(
            out.contains("no such terminal"),
            "the release failure must be surfaced too, not swallowed: {out}"
        );
        // Order, not just presence: `.contains(...)` alone would not have
        // caught a regression that put the release warning first. The
        // finished command's own output and exit status must read before
        // the unrelated warning about releasing its terminal.
        assert!(
            out.find("[exit status unknown]").unwrap() < out.find("[warning:").unwrap(),
            "the exit status must be rendered before the release warning: {out}"
        );
        assert_eq!(
            state
                .acp_terminals
                .lock()
                .unwrap()
                .get(TEST_SESSION_ID)
                .map(Vec::len),
            Some(1),
            "the handle stays tracked — a release failure is not proof it actually freed"
        );
    }

    /// `#262`'s feature, over the interface the old design rejected: the
    /// listing is a shell command on the client, and the *shape* the model
    /// sees is this side's -- so a `dir_list` result reads identically
    /// whichever machine it came from.
    ///
    /// The order, though, is the **script's**, not this side's: it arrives in
    /// the agent's own pre-order DFS (see `ORDER_SH`) and is passed through.
    /// Re-sorting here would undo that order for exactly the pair this fixture
    /// ends on -- `sub` (whose contents the script emits right after it)
    /// before the sibling file `sub.txt` -- which is the bug the review of
    /// this task found. A fake cannot show the script's real order;
    /// `builtin_tools`'s `the_client_scripts_keep_the_agents_own_dfs_order`
    /// runs the script itself for that.
    #[tokio::test]
    async fn dir_list_shapes_the_clients_find_output_like_the_agents_own() {
        let client = Arc::new(FakeClient::default());
        client.queue_terminal_stdout(
            "E\t/z\nF\t/z/a.txt\nF\t/z/b.txt\nD\t/z/sub\nF\t/z/sub/inner.txt\nF\t/z/sub.txt\n",
        );
        let as_client = Arc::clone(&client) as Arc<dyn AcpClient>;

        let out = scope_acp_client(
            Arc::clone(&as_client),
            client_dir_list(&as_client, &json!({"path": "/z"})),
        )
        .await
        .unwrap();

        assert_eq!(
            out,
            "/z/a.txt\n/z/b.txt\n/z/sub/\n/z/sub/inner.txt\n/z/sub.txt"
        );

        let argv = client
            .last_terminal_command()
            .expect("a terminal was created");
        assert_eq!(argv[0], "bash");
        assert_eq!(argv[1], "-c");
        assert_eq!(
            argv[2].as_str(),
            list_script().as_str(),
            "the script the tool runs is the one the tests can run too"
        );
        assert!(
            argv[2].contains("-mindepth 1") && argv[2].contains("-maxdepth 1"),
            "one level only, and not the directory itself: {}",
            argv[2]
        );
        assert_eq!(
            argv[3], "client_dir_list",
            "argv0 is a placeholder; the path must not sit in `$0`, which no script reads"
        );
        assert_eq!(argv[4], "/z", "the path is `$1`, as a positional argument");
        assert_eq!(argv.len(), 5, "no extra arguments: {argv:?}");
    }

    /// The walk honours `max_depth`/`max_entries` and reports truncation with
    /// the same marker the agent-side walk uses -- the model must not have to
    /// learn a second vocabulary for "there was more".
    ///
    /// Both bounds travel as positional arguments, so they are asserted in the
    /// argv rather than inside the script: `max_depth + 1` (find counts the
    /// starting point as depth 0) and `max_entries + 1` (one more than will be
    /// shown, which is what makes truncation decidable here).
    #[tokio::test]
    async fn dir_walk_truncates_with_the_same_marker_as_the_agent_side() {
        let client = Arc::new(FakeClient::default());
        client.queue_terminal_stdout("E\t/z\nF\t/z/f0\nF\t/z/f1\nF\t/z/f2\n");
        let as_client = Arc::clone(&client) as Arc<dyn AcpClient>;

        let text = scope_acp_client(
            Arc::clone(&as_client),
            client_dir_walk(
                &as_client,
                &json!({"path": "/z", "max_depth": 2, "max_entries": 2}),
            ),
        )
        .await
        .unwrap();

        assert_eq!(
            text,
            "/z/f0\n/z/f1\n[truncated \u{2014} more than 2 entries; \
             raise max_entries or narrow path]"
        );

        let argv = client.last_terminal_command().unwrap();
        assert_eq!(argv[2].as_str(), walk_script().as_str());
        assert!(
            argv[2].contains("-maxdepth \"$2\""),
            "the depth is an argument, got: {}",
            argv[2]
        );
        assert!(
            argv[2].contains("head -n \"$3\""),
            "the entry cap is an argument, got: {}",
            argv[2]
        );
        assert_eq!(argv[3], "client_dir_walk");
        assert_eq!(argv[4], "3", "max_depth + 1, got: {argv:?}");
        assert_eq!(argv[5], "3", "max_entries + 1, got: {argv:?}");
        assert_eq!(argv.len(), 6, "no extra arguments: {argv:?}");
    }

    /// An empty directory is `(empty) <path>` here too: the script prints no
    /// entries for one, and the marker is added on this side.
    ///
    /// The path in that marker is the one the **client** resolved (the script's
    /// `E` line), not the string the caller passed: `~/x` is a directory on the
    /// client's machine whose absolute path this side cannot know, so naming
    /// the unexpanded form would answer a question the model did not ask. This
    /// is the one place the two machines' `(empty)` output can differ in shape
    /// from the input, and it differs exactly the way the agent side's
    /// `path.display()` does -- both name what was actually resolved.
    #[tokio::test]
    async fn an_empty_client_directory_gets_the_empty_marker() {
        let client = Arc::new(FakeClient::default());
        client.queue_terminal_stdout("E\t/z\n");
        let as_client = Arc::clone(&client) as Arc<dyn AcpClient>;

        let listed = scope_acp_client(
            Arc::clone(&as_client),
            client_dir_list(&as_client, &json!({"path": "/z"})),
        )
        .await
        .unwrap();
        assert_eq!(listed, "(empty) /z");

        client.queue_terminal_stdout("E\t/z\n");
        let walked = scope_acp_client(
            Arc::clone(&as_client),
            client_dir_walk(&as_client, &json!({"path": "/z"})),
        )
        .await
        .unwrap();
        assert_eq!(walked, "(empty) /z");

        client.queue_terminal_stdout("E\t/home/me/x\n");
        let tilde = scope_acp_client(
            Arc::clone(&as_client),
            client_dir_list(&as_client, &json!({"path": "~/x"})),
        )
        .await
        .unwrap();
        assert_eq!(
            tilde, "(empty) /home/me/x",
            "the marker names the path the client resolved, not `~/x`"
        );
    }

    /// A line that is not one of the three classified kinds is dropped, not
    /// shown: the script is the only writer on the happy path, but a filename
    /// containing a newline arrives as lines of its own, and a stray diagnostic
    /// on stdout must not become an entry.
    #[test]
    fn only_classified_lines_become_entries() {
        let (entries, truncated) = shape_entries(
            "noise\nE\t/z\nF\t/z/a.txt\nX\t/z/b.txt\nD\t/z/sub\nF\t/z/sub.txt\n",
            "/z",
            usize::MAX,
        );
        assert_eq!(
            entries,
            vec![
                "/z/a.txt".to_string(),
                "/z/sub/".to_string(),
                "/z/sub.txt".to_string()
            ],
            "the script's order is kept, and nothing outside it is re-sorted"
        );
        assert!(!truncated);
    }

    /// A `find` that failed is an error, not an empty listing.
    ///
    /// This is the shape the review caught: `find` prints nothing on stdout
    /// when it cannot read the directory, so a client that only reported what
    /// came back answered `(empty) <path>` -- indistinguishable, to the model,
    /// from a directory that really is empty. Both tools fail here, and both
    /// name the tool and the path the way the agent-side bodies do.
    #[tokio::test]
    async fn a_failed_find_is_an_error_not_an_empty_listing() {
        let client = Arc::new(FakeClient::default());
        client.queue_terminal_result("", Some(1));
        let as_client = Arc::clone(&client) as Arc<dyn AcpClient>;

        let listed = scope_acp_client(
            Arc::clone(&as_client),
            client_dir_list(&as_client, &json!({"path": "/z/nope"})),
        )
        .await
        .unwrap_err();
        assert_eq!(
            listed.to_string(),
            "Failed to list '/z/nope'",
            "the same sentence the agent side uses for the same situation"
        );

        client.queue_terminal_result("", Some(1));
        let walked = scope_acp_client(
            Arc::clone(&as_client),
            client_dir_walk(&as_client, &json!({"path": "/z/nope"})),
        )
        .await
        .unwrap_err();
        assert_eq!(walked.to_string(), "Failed to walk '/z/nope'");
    }
}
