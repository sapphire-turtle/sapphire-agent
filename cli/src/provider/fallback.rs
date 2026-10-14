//! Refusal-fallback wrapper.
//!
//! Wraps a primary provider together with a fallback. The primary is tried
//! first; if it appears to refuse the request (content-policy stop reason
//! or a recognised apology pattern in the text) or errors out entirely,
//! the same call is replayed against the fallback.
//!
//! This is the layer that lets the daily-log / memory-compaction code keep
//! running on the Anthropic provider for everyday content but quietly hand
//! off to a permissive local model when a session contains material
//! Anthropic refuses to summarise.

use crate::provider::{ChatMessage, ChatResponse, Provider, ToolSpec};
use anyhow::Result;
use async_trait::async_trait;
use std::sync::Arc;
use tracing::{info, warn};

pub struct FallbackProvider {
    primary: Arc<dyn Provider>,
    fallback: Arc<dyn Provider>,
}

impl FallbackProvider {
    pub fn new(primary: Arc<dyn Provider>, fallback: Arc<dyn Provider>) -> Self {
        Self { primary, fallback }
    }
}

/// Heuristic check for a refusal response.
///
/// First, a provider-reported `stop_reason` of `"refusal"` (Anthropic)
/// or `"content_filter"` (OpenAI-compatible) is the strongest signal
/// and matches no legitimate response.
///
/// Otherwise, fall back to a short text-only response (no tool calls,
/// capped length) whose leading sentence matches an apology pattern.
/// The leading-pattern + length + no-tools combination keeps false
/// positives down on legitimate "I can't find that file" style answers.
pub fn is_refusal(resp: &ChatResponse) -> bool {
    if matches!(
        resp.stop_reason.as_deref(),
        Some("refusal") | Some("content_filter")
    ) {
        return true;
    }
    if !resp.tool_calls.is_empty() {
        return false;
    }
    let Some(text) = resp.text.as_deref() else {
        return false;
    };
    // Long responses are almost certainly genuine answers, not apologies.
    if text.chars().count() > 600 {
        return false;
    }
    let head = text
        .trim_start()
        .chars()
        .take(120)
        .collect::<String>()
        .to_lowercase();
    const PATTERNS: &[&str] = &[
        "i can't help",
        "i cannot help",
        "i can't assist",
        "i cannot assist",
        "i'm not able to",
        "i am not able to",
        "i'm unable to",
        "i am unable to",
        "i won't",
        "i will not",
        "申し訳",
        "お手伝いできません",
        "対応できません",
    ];
    PATTERNS.iter().any(|p| head.contains(p))
}

#[async_trait]
impl Provider for FallbackProvider {
    fn name(&self) -> &str {
        self.primary.name()
    }

    fn calibration_keys(&self) -> Vec<&str> {
        let mut keys = self.primary.calibration_keys();
        keys.extend(self.fallback.calibration_keys());
        keys
    }

    async fn chat(
        &self,
        system: Option<&str>,
        messages: &[ChatMessage],
        tools: Option<&[ToolSpec]>,
    ) -> Result<ChatResponse> {
        match self.primary.chat(system, messages, tools).await {
            Ok(resp) if is_refusal(&resp) => {
                info!(
                    "Primary provider '{}' refused; retrying via fallback '{}'",
                    self.primary.name(),
                    self.fallback.name()
                );
                self.fallback.chat(system, messages, tools).await
            }
            Ok(resp) => Ok(resp),
            Err(e) => {
                warn!(
                    "Primary provider '{}' errored ({e:#}); retrying via fallback '{}'",
                    self.primary.name(),
                    self.fallback.name()
                );
                self.fallback.chat(system, messages, tools).await
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{ChatMessage, ChatResponse, ToolCall};
    use async_trait::async_trait;
    use serde_json::json;
    use std::sync::Mutex;

    /// Test double: returns a queued response on each call and records the
    /// number of times it was invoked.
    struct ScriptedProvider {
        name: String,
        responses: Mutex<Vec<Result<ChatResponse, String>>>,
        calls: Mutex<usize>,
    }

    impl ScriptedProvider {
        fn new(name: &str, responses: Vec<Result<ChatResponse, String>>) -> Arc<Self> {
            Arc::new(Self {
                name: name.to_string(),
                responses: Mutex::new(responses),
                calls: Mutex::new(0),
            })
        }

        fn calls(&self) -> usize {
            *self.calls.lock().unwrap()
        }
    }

    #[async_trait]
    impl Provider for ScriptedProvider {
        fn name(&self) -> &str {
            &self.name
        }

        async fn chat(
            &self,
            _system: Option<&str>,
            _messages: &[ChatMessage],
            _tools: Option<&[ToolSpec]>,
        ) -> Result<ChatResponse> {
            *self.calls.lock().unwrap() += 1;
            let next = self.responses.lock().unwrap().remove(0);
            next.map_err(|e| anyhow::anyhow!(e))
        }
    }

    fn refusal_response() -> ChatResponse {
        ChatResponse {
            prompt_usage: None,
            text: Some("I can't help with that request.".into()),
            tool_calls: vec![],
            stop_reason: Some("refusal".into()),
        }
    }

    fn ok_response(text: &str) -> ChatResponse {
        ChatResponse {
            prompt_usage: None,
            text: Some(text.into()),
            tool_calls: vec![],
            stop_reason: Some("end_turn".into()),
        }
    }

    #[test]
    fn refusal_stop_reason_is_detected() {
        assert!(is_refusal(&refusal_response()));
    }

    #[test]
    fn content_filter_stop_reason_is_detected() {
        let resp = ChatResponse {
            prompt_usage: None,
            text: Some("...".into()),
            tool_calls: vec![],
            stop_reason: Some("content_filter".into()),
        };
        assert!(is_refusal(&resp));
    }

    #[test]
    fn apology_pattern_without_stop_reason_is_detected() {
        let resp = ChatResponse {
            prompt_usage: None,
            text: Some("I'm unable to assist with that one.".into()),
            tool_calls: vec![],
            stop_reason: Some("end_turn".into()),
        };
        assert!(is_refusal(&resp));
    }

    #[test]
    fn legitimate_short_answer_is_not_a_refusal() {
        let resp = ChatResponse {
            prompt_usage: None,
            text: Some("The file is at src/main.rs.".into()),
            tool_calls: vec![],
            stop_reason: Some("end_turn".into()),
        };
        assert!(!is_refusal(&resp));
    }

    #[test]
    fn long_response_is_not_a_refusal_even_if_apology_appears() {
        let mut text =
            "I'm unable to immediately answer, but here's a detailed walkthrough: ".to_string();
        text.push_str(&"x".repeat(800));
        let resp = ChatResponse {
            prompt_usage: None,
            text: Some(text),
            tool_calls: vec![],
            stop_reason: Some("end_turn".into()),
        };
        assert!(!is_refusal(&resp));
    }

    #[test]
    fn tool_call_short_circuits_text_heuristic() {
        let resp = ChatResponse {
            prompt_usage: None,
            text: Some("I can't help directly, calling tool…".into()),
            tool_calls: vec![ToolCall {
                id: "x".into(),
                name: "y".into(),
                input: json!({}),
            }],
            stop_reason: Some("tool_use".into()),
        };
        assert!(!is_refusal(&resp));
    }

    #[tokio::test]
    async fn fallback_takes_over_on_refusal() {
        let primary = ScriptedProvider::new("primary", vec![Ok(refusal_response())]);
        let fallback = ScriptedProvider::new("fallback", vec![Ok(ok_response("done"))]);
        let p: Arc<dyn Provider> = primary.clone();
        let f: Arc<dyn Provider> = fallback.clone();
        let wrap = FallbackProvider::new(p, f);
        let resp = wrap.chat(None, &[], None).await.unwrap();
        assert_eq!(resp.text.as_deref(), Some("done"));
        assert_eq!(primary.calls(), 1);
        assert_eq!(fallback.calls(), 1);
    }

    #[tokio::test]
    async fn fallback_takes_over_on_error() {
        let primary = ScriptedProvider::new("primary", vec![Err("boom".to_string())]);
        let fallback = ScriptedProvider::new("fallback", vec![Ok(ok_response("done"))]);
        let p: Arc<dyn Provider> = primary.clone();
        let f: Arc<dyn Provider> = fallback.clone();
        let wrap = FallbackProvider::new(p, f);
        let resp = wrap.chat(None, &[], None).await.unwrap();
        assert_eq!(resp.text.as_deref(), Some("done"));
        assert_eq!(primary.calls(), 1);
        assert_eq!(fallback.calls(), 1);
    }

    #[tokio::test]
    async fn primary_success_skips_fallback() {
        let primary = ScriptedProvider::new("primary", vec![Ok(ok_response("hi"))]);
        let fallback = ScriptedProvider::new("fallback", vec![Ok(ok_response("nope"))]);
        let p: Arc<dyn Provider> = primary.clone();
        let f: Arc<dyn Provider> = fallback.clone();
        let wrap = FallbackProvider::new(p, f);
        let resp = wrap.chat(None, &[], None).await.unwrap();
        assert_eq!(resp.text.as_deref(), Some("hi"));
        assert_eq!(primary.calls(), 1);
        assert_eq!(fallback.calls(), 0);
    }
    /// The wrapper answers to its primary's name, so calibration would learn
    /// the fallback's token prices under the primary's key. Both names have
    /// to be on the record.
    #[test]
    fn a_fallback_chain_reports_both_tokenizers() {
        let chain = FallbackProvider::new(
            ScriptedProvider::new("primary", vec![Ok(ok_response("hi"))]),
            ScriptedProvider::new("secondary", vec![Ok(ok_response("hi"))]),
        );
        assert_eq!(chain.name(), "primary");
        assert_eq!(chain.calibration_keys(), vec!["primary", "secondary"]);
    }
}
