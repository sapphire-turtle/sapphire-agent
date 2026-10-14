//! The one-shot "run a command on the editor's machine and wait for it"
//! mechanism shared by `shell` and the tools that reuse the same
//! run-and-collect dance.
//!
//! Formatting the result for the model — `format_finished`, the timeout
//! message — stays with the caller in `client_tools.rs`; this module
//! moves mechanism, not presentation.

use crate::tools::OUTPUT_CAP_BYTES;
use crate::tools::acp_client::{ExitStatus, TerminalHandle, TerminalOutput};
use crate::tools::client_tools::cap_error;

/// The result of running one command to completion, or to its timeout.
pub(crate) struct ClientRun {
    pub output: TerminalOutput,
    /// `None` when the command outlived the timeout and was left running.
    pub status: Option<ExitStatus>,
    /// Set when the command outlived the timeout and was left running.
    pub timed_out_handle: Option<TerminalHandle>,
    /// Set when the command finished and its output was collected, but
    /// releasing the terminal afterwards failed. Rendered by the caller
    /// *after* `format_finished`, so a finished command's output and exit
    /// status still read first.
    pub release_warning: Option<String>,
}

/// Run a command on the machine the editor is running on, and wait for
/// it to finish — up to a timeout.
pub(crate) async fn run_client_command(
    client: &std::sync::Arc<dyn crate::tools::acp_client::AcpClient>,
    command: &str,
    args: &[String],
    cwd: Option<&str>,
    timeout: std::time::Duration,
) -> anyhow::Result<ClientRun> {
    // Same cap, same check, as `ClientShellStart`: a timed-out call
    // below leaves a handle tracked, so without this a model
    // looping `shell` with a short `timeout_secs` could
    // accumulate unbounded live processes — see this tool's doc.
    //
    // Reserve-then-create, not read-then-write: `run_llm_turn` runs
    // a turn's permitted calls concurrently, so one assistant
    // message with several `shell`/`shell_start`
    // calls must not let them all read the count before any of
    // them wrote it back. `try_reserve_terminal_slot` does the
    // check and the reservation in one lock span so that cannot
    // happen. See its doc.
    let reservation = client
        .try_reserve_terminal_slot()
        .await
        .map_err(|held| cap_error(&held))?;

    // If `create_terminal` errors, or this whole call is cancelled
    // before it returns — the turn's future dropped mid-RPC, which
    // is exactly the cancellation ACP treats as routine (Escape in
    // an editor, a dropped socket) — `reservation` is dropped here
    // without ever reaching `track_terminal` below. Its `Drop`
    // frees the slot itself; nothing further to do on this path.
    // See `TerminalReservation`'s doc.
    let handle = client
        .create_terminal(command, args, cwd, Some(OUTPUT_CAP_BYTES as u64))
        .await?;
    // Tracked immediately, before anything that can fail or be
    // dropped: a `wait_for_terminal_exit` error below, or this
    // whole future being dropped mid-wait (turn cancellation), must
    // not lose a command that is genuinely still running on the
    // user's machine. This also consumes `reservation`, resolving
    // it into the real handle so its `Drop` won't also try to free
    // the slot out from under the now-tracked handle.
    client.track_terminal(reservation, handle.clone()).await;

    match tokio::time::timeout(timeout, client.wait_for_terminal_exit(&handle)).await {
        Ok(status) => {
            // On error, the handle stays tracked (already tracked
            // above) rather than being lost — the command may still
            // be running and a transient RPC error here is not
            // proof otherwise. See `ClientShellOutput`'s doc for the
            // same reasoning applied to polling.
            let status = status?;
            let output = client.terminal_output(&handle).await?;
            match client.release_terminal(&handle).await {
                Ok(()) => {
                    client.untrack_terminal(&handle).await;
                    Ok(ClientRun {
                        output,
                        status: Some(status),
                        timed_out_handle: None,
                        release_warning: None,
                    })
                }
                Err(e) => {
                    // The command finished and its output was
                    // already collected successfully — that must
                    // not be thrown away just because the release
                    // that follows failed. The handle is left
                    // tracked (over-counting is recoverable; losing
                    // a finished build's output is not), so the
                    // model can retry `shell_kill` to free it.
                    Ok(ClientRun {
                        output,
                        status: Some(status),
                        timed_out_handle: None,
                        release_warning: Some(format!(
                            "[warning: the command finished, but releasing terminal \
                             {handle} failed: {e}. It may still be tracked; use \
                             shell_kill to free it.]"
                        )),
                    })
                }
            }
        }
        Err(_elapsed) => {
            // Already tracked above — the handle escapes this call
            // still running, so it has to stay in the same
            // session-keyed tracking `shell_start` uses,
            // otherwise it would count against nothing, the cap
            // would never see it, and the model would have no way
            // to list it in order to clean it up.
            Ok(ClientRun {
                output: TerminalOutput::default(),
                status: None,
                timed_out_handle: Some(handle),
                release_warning: None,
            })
        }
    }
}
