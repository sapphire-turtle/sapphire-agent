//! HTTP plumbing shared by the streaming providers: the client they build
//! and the deadline every read from the upstream runs under.
//!
//! Both providers used to build `reqwest::Client::new()`, which has no
//! timeout of any kind, and read their SSE stream with a bare
//! `stream.next().await`. An upstream that sent its headers and then went
//! quiet — OpenRouter has done exactly this mid-stream — left that `.await`
//! parked forever: no error, no log line, and the turn above it (a
//! subagent's, or the parent's own) simply never ended (#258).
//!
//! The deadline is an **idle** timeout, not a total one. A long answer from
//! a slow model is legitimate and can take many minutes end to end; what is
//! never legitimate is minutes of silence. So the clock restarts on every
//! chunk, and it only fires when nothing at all has arrived for the whole
//! window.

use anyhow::{Result, anyhow};
use reqwest::Client;
use std::future::Future;
use std::time::Duration;

/// Default for `connect_timeout_secs`: how long establishing the TCP (and
/// TLS) connection may take. Generous for a LAN endpoint, and still short
/// enough that an unreachable host fails a turn promptly rather than after
/// the OS's own multi-minute SYN retry schedule.
pub fn default_connect_timeout_secs() -> u64 {
    15
}

/// Default for `stream_idle_timeout_secs`: how long the upstream may go
/// without sending a single byte — before the response headers or between
/// two chunks of the stream.
///
/// Five minutes rather than something tighter because the wait before the
/// *first* byte includes prompt processing. A remote API streams its headers
/// almost at once, but a local server working through a very large prompt
/// can stay silent for minutes before its first token, and that is not a
/// hang. Raise it for such a server; `0` turns the deadline off.
pub fn default_stream_idle_timeout_secs() -> u64 {
    300
}

/// `0` is the spelling for "no deadline", the same convention
/// `[tools.tool_rounds]` uses: a config file has to say "off" somehow, and a
/// zero-second deadline is never a meaningful one.
pub fn secs(value: u64) -> Option<Duration> {
    (value != 0).then(|| Duration::from_secs(value))
}

/// The client a streaming provider sends through.
///
/// Only the connect phase is bounded here. reqwest's own `timeout` is a
/// deadline on the *whole* request, body included, which is exactly the
/// total timeout this module avoids; the idle deadline is applied per read
/// by [`idle`] instead, where the error can also say which provider stalled.
pub fn client(connect_timeout: Option<Duration>) -> Client {
    let mut builder = Client::builder();
    if let Some(t) = connect_timeout {
        builder = builder.connect_timeout(t);
    }
    // `Client::new()` makes the same call and panics the same way; the only
    // failure it can hit is the TLS backend refusing to initialise, which no
    // provider could recover from anyway.
    builder
        .build()
        .expect("failed to initialise the HTTP client")
}

/// Await `fut` — one read from the upstream — under the idle deadline.
///
/// `what` names the provider and the phase for the error message, since the
/// error is what ends up in the log and, for a subagent, is the only trace
/// of why its turn stopped.
pub async fn idle<F: Future>(
    deadline: Option<Duration>,
    what: impl FnOnce() -> String,
    fut: F,
) -> Result<F::Output> {
    let Some(deadline) = deadline else {
        return Ok(fut.await);
    };
    tokio::time::timeout(deadline, fut).await.map_err(|_| {
        anyhow!(
            "{} sent nothing for {}s; giving up on the stalled stream",
            what(),
            deadline.as_secs_f64()
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_seconds_means_no_deadline() {
        assert_eq!(secs(0), None);
        assert_eq!(secs(300), Some(Duration::from_secs(300)));
    }

    #[tokio::test]
    async fn a_read_that_never_resolves_fails_at_the_deadline() {
        let err = idle(
            Some(Duration::from_millis(50)),
            || "the test upstream".to_string(),
            std::future::pending::<()>(),
        )
        .await
        .expect_err("a pending read must not wait forever");
        assert!(
            err.to_string().contains("the test upstream sent nothing"),
            "the error must name who stalled: {err}"
        );
    }

    #[tokio::test]
    async fn no_deadline_waits_for_the_read() {
        let got = idle(None, || unreachable!(), async {
            tokio::time::sleep(Duration::from_millis(20)).await;
            7
        })
        .await
        .unwrap();
        assert_eq!(got, 7);
    }
}
