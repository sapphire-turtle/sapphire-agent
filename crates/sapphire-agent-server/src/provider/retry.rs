//! Retry wrapper for responses that came back unusable.
//!
//! Three shapes of reply used to pass through as a successful, *empty* turn:
//!
//! - **Cut off at `max_tokens`.** A model stuck in a reasoning loop spends
//!   the whole budget thinking and never reaches an answer. The provider
//!   reports `finish_reason: "length"` (OpenAI-compatible) or
//!   `stop_reason: "max_tokens"` (Anthropic), the visible text is empty or
//!   half a sentence, and any tool call's arguments are truncated JSON.
//! - **A stream that ended without saying how.** The upstream closed the
//!   connection before its final chunk — a local server killed or starved
//!   by a heavy build on the same machine does exactly this. The provider
//!   reports it as [`IncompleteStream`].
//! - **Nothing at all.** No text and no tool call, with a normal stop.
//!
//! None of these are an answer, and the turn loop read the first and last
//! as "the model replied with nothing" and handed the turn back to the user
//! with no error anywhere. The same request usually succeeds on a second
//! try — sampling is not deterministic — so each is retried a few times,
//! logged on every attempt, and turned into a real error once the retries
//! are spent so the client is told the turn failed.
//!
//! Every other error passes straight through: an API error or an idle
//! timeout is not made better by asking again, and retrying a 300-second
//! stall would only multiply the wait.

use crate::provider::{ChatMessage, ChatResponse, Provider, ToolSpec};
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use std::fmt;
use std::sync::Arc;
use tracing::{error, warn};

/// Default for `incomplete_retries`: how many times an unusable response is
/// retried before the call fails. Two retries means three attempts in all.
pub fn default_incomplete_retries() -> u32 {
    2
}

/// The upstream's stream ended before it said it was finished — no finish
/// reason, no `[DONE]`, no `message_stop`, or a read error mid-body.
///
/// A distinct type so [`RetryProvider`] can tell it apart from errors that
/// are not worth retrying.
#[derive(Debug)]
pub struct IncompleteStream {
    pub provider: String,
    pub detail: String,
}

impl fmt::Display for IncompleteStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}: the response stream ended before it was complete ({})",
            self.provider, self.detail
        )
    }
}

impl std::error::Error for IncompleteStream {}

/// Why a response cannot be used as an answer, if it cannot.
fn unusable(resp: &ChatResponse) -> Option<String> {
    if matches!(resp.stop_reason.as_deref(), Some("length" | "max_tokens")) {
        return Some(format!(
            "the response hit max_tokens (stop reason '{}') before it finished",
            resp.stop_reason.as_deref().unwrap_or_default()
        ));
    }
    let no_text = resp.text.as_deref().is_none_or(|t| t.trim().is_empty());
    if no_text && !resp.has_tool_calls() {
        return Some(format!(
            "the response was empty — no text and no tool call (stop reason {:?})",
            resp.stop_reason
        ));
    }
    None
}

pub struct RetryProvider {
    inner: Arc<dyn Provider>,
    retries: u32,
}

impl RetryProvider {
    pub fn new(inner: Arc<dyn Provider>, retries: u32) -> Self {
        Self { inner, retries }
    }
}

#[async_trait]
impl Provider for RetryProvider {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn calibration_keys(&self) -> Vec<&str> {
        self.inner.calibration_keys()
    }

    async fn chat(
        &self,
        system: Option<&str>,
        messages: &[ChatMessage],
        tools: Option<&[ToolSpec]>,
    ) -> Result<ChatResponse> {
        let attempts = self.retries + 1;
        let mut attempt = 1;
        loop {
            let reason = match self.inner.chat(system, messages, tools).await {
                Ok(resp) => match unusable(&resp) {
                    None => return Ok(resp),
                    Some(reason) => reason,
                },
                Err(e) => match e.downcast_ref::<IncompleteStream>() {
                    Some(incomplete) => incomplete.to_string(),
                    None => return Err(e),
                },
            };
            if attempt >= attempts {
                error!(
                    "Provider '{}': {reason}; giving up after {attempts} attempt(s)",
                    self.name()
                );
                return Err(anyhow!(
                    "provider '{}' returned no usable response after {attempts} attempt(s): \
                     {reason}",
                    self.name()
                ));
            }
            warn!(
                "Provider '{}': {reason}; retrying the same request (attempt {}/{attempts})",
                self.name(),
                attempt + 1
            );
            attempt += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::ToolCall;
    use serde_json::json;
    use std::sync::Mutex;

    /// Hands out queued results in order and counts the calls.
    struct Scripted {
        replies: Mutex<Vec<Result<ChatResponse>>>,
        calls: Mutex<usize>,
    }

    impl Scripted {
        fn new(mut replies: Vec<Result<ChatResponse>>) -> Arc<Self> {
            replies.reverse();
            Arc::new(Self {
                replies: Mutex::new(replies),
                calls: Mutex::new(0),
            })
        }

        fn calls(&self) -> usize {
            *self.calls.lock().unwrap()
        }
    }

    #[async_trait]
    impl Provider for Scripted {
        fn name(&self) -> &str {
            "scripted"
        }

        async fn chat(
            &self,
            _system: Option<&str>,
            _messages: &[ChatMessage],
            _tools: Option<&[ToolSpec]>,
        ) -> Result<ChatResponse> {
            *self.calls.lock().unwrap() += 1;
            self.replies
                .lock()
                .unwrap()
                .pop()
                .expect("more calls than scripted replies")
        }
    }

    fn reply(text: Option<&str>, stop: &str) -> Result<ChatResponse> {
        Ok(ChatResponse {
            text: text.map(str::to_string),
            tool_calls: vec![],
            prompt_usage: None,
            stop_reason: Some(stop.to_string()),
        })
    }

    fn incomplete() -> Result<ChatResponse> {
        Err(IncompleteStream {
            provider: "scripted".into(),
            detail: "connection closed".into(),
        }
        .into())
    }

    async fn run(inner: &Arc<Scripted>, retries: u32) -> Result<ChatResponse> {
        RetryProvider::new(inner.clone(), retries)
            .chat(None, &[ChatMessage::user("hi")], None)
            .await
    }

    #[tokio::test]
    async fn a_good_reply_is_returned_on_the_first_attempt() {
        let inner = Scripted::new(vec![reply(Some("hello"), "stop")]);
        let resp = run(&inner, 2).await.unwrap();
        assert_eq!(resp.text.as_deref(), Some("hello"));
        assert_eq!(inner.calls(), 1);
    }

    #[tokio::test]
    async fn a_truncated_reply_is_retried_until_one_finishes() {
        let inner = Scripted::new(vec![
            reply(None, "length"),
            reply(Some("half an ans"), "max_tokens"),
            reply(Some("the answer"), "stop"),
        ]);
        let resp = run(&inner, 2).await.unwrap();
        assert_eq!(resp.text.as_deref(), Some("the answer"));
        assert_eq!(inner.calls(), 3);
    }

    #[tokio::test]
    async fn an_incomplete_stream_and_an_empty_reply_are_retried() {
        let inner = Scripted::new(vec![
            incomplete(),
            reply(Some("  "), "stop"),
            reply(Some("ok"), "stop"),
        ]);
        assert_eq!(run(&inner, 2).await.unwrap().text.as_deref(), Some("ok"));
        assert_eq!(inner.calls(), 3);
    }

    #[tokio::test]
    async fn spent_retries_become_an_error_naming_the_cause() {
        let inner = Scripted::new(vec![reply(None, "length"), reply(None, "length")]);
        let err = run(&inner, 1)
            .await
            .expect_err("must fail once retries run out");
        let msg = format!("{err:#}");
        assert!(msg.contains("max_tokens"), "{msg}");
        assert!(msg.contains("2 attempt"), "{msg}");
        assert_eq!(inner.calls(), 2);
    }

    #[tokio::test]
    async fn zero_retries_fails_on_the_first_unusable_reply() {
        let inner = Scripted::new(vec![reply(None, "length")]);
        assert!(run(&inner, 0).await.is_err());
        assert_eq!(inner.calls(), 1);
    }

    #[tokio::test]
    async fn other_errors_are_not_retried() {
        let inner = Scripted::new(vec![Err(anyhow!("API error 401"))]);
        let err = run(&inner, 2).await.unwrap_err();
        assert!(err.to_string().contains("401"));
        assert_eq!(inner.calls(), 1);
    }

    #[tokio::test]
    async fn a_tool_call_without_text_is_a_usable_reply() {
        let inner = Scripted::new(vec![Ok(ChatResponse {
            text: None,
            tool_calls: vec![ToolCall {
                id: "c".into(),
                name: "shell".into(),
                input: json!({}),
            }],
            prompt_usage: None,
            stop_reason: Some("tool_calls".into()),
        })]);
        assert!(run(&inner, 2).await.unwrap().has_tool_calls());
        assert_eq!(inner.calls(), 1);
    }
}
