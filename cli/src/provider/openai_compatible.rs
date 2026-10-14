//! OpenAI-compatible chat completions provider.
//!
//! Targets `POST {base_url}/chat/completions` with SSE streaming. Works
//! against llama.cpp's `llama-server`, Ollama (`/v1`), vLLM, and the OpenAI
//! API itself. Tool calls follow the OpenAI `tools` / `tool_calls` shape.

use crate::provider::retry::IncompleteStream;
use crate::provider::{
    ChatMessage, ChatResponse, ContentPart, PromptUsage, Provider, Role, ToolCall, ToolSpec, http,
};
use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use futures_util::StreamExt;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::time::Duration;
use tracing::debug;

/// Configuration for an OpenAI-compatible endpoint.
///
/// Not yet wired into the top-level `Config` — that's part of the
/// multi-provider routing work. Construct directly for now.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct OpenAICompatibleConfig {
    /// Base URL up to and including the API version segment, e.g.
    /// `http://127.0.0.1:8080/v1` for llama.cpp or
    /// `https://api.openai.com/v1` for OpenAI proper.
    pub base_url: String,
    /// Bearer token. Optional — llama.cpp's local server does not require one.
    #[serde(default)]
    pub api_key: Option<String>,
    /// Model identifier passed in the request body. For llama.cpp this can
    /// be any string the server accepts (often ignored when only one model
    /// is loaded).
    pub model: String,
    /// Provider name surfaced via `Provider::name()`. Defaults to
    /// `"openai_compatible"`.
    #[serde(default)]
    pub provider_name: Option<String>,
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u32,
    /// Seconds establishing a connection may take. `0` disables. See
    /// `crate::provider::http::default_connect_timeout_secs`.
    #[serde(default = "crate::provider::http::default_connect_timeout_secs")]
    pub connect_timeout_secs: u64,
    /// Seconds the endpoint may go without sending anything — before its
    /// response headers or between two stream chunks — before the request
    /// fails. `0` disables. See
    /// `crate::provider::http::default_stream_idle_timeout_secs`.
    #[serde(default = "crate::provider::http::default_stream_idle_timeout_secs")]
    pub stream_idle_timeout_secs: u64,
    /// How many times a response that came back unusable — cut off at
    /// `max_tokens`, empty, or a stream that ended early — is retried
    /// before the call fails. `0` disables. See `crate::provider::retry`.
    #[serde(default = "crate::provider::retry::default_incomplete_retries")]
    pub incomplete_retries: u32,
}

fn default_max_tokens() -> u32 {
    8192
}

pub struct OpenAICompatibleProvider {
    base_url: String,
    api_key: Option<String>,
    model: String,
    name: String,
    max_tokens: u32,
    client: Client,
    stream_idle_timeout: Option<Duration>,
}

impl OpenAICompatibleProvider {
    pub fn new(cfg: &OpenAICompatibleConfig) -> Self {
        let base_url = cfg.base_url.trim_end_matches('/').to_string();
        Self {
            base_url,
            api_key: cfg.api_key.clone(),
            model: cfg.model.clone(),
            name: cfg
                .provider_name
                .clone()
                .unwrap_or_else(|| "openai_compatible".to_string()),
            max_tokens: cfg.max_tokens,
            client: http::client(http::secs(cfg.connect_timeout_secs)),
            stream_idle_timeout: http::secs(cfg.stream_idle_timeout_secs),
        }
    }

    fn endpoint(&self) -> String {
        format!("{}/chat/completions", self.base_url)
    }
}

// ---------------------------------------------------------------------------
// Wire-format types — request
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct Request<'a> {
    model: &'a str,
    messages: Vec<ApiMessage>,
    max_tokens: u32,
    stream: bool,
    /// Ask for the usage block a streaming response otherwise omits. It
    /// arrives as a final chunk carrying no choices, and it is the only way
    /// to learn what the server actually counted for a prompt we can only
    /// estimate.
    stream_options: StreamOptions,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<ApiToolSpec<'a>>>,
}

#[derive(Debug, Serialize)]
struct StreamOptions {
    include_usage: bool,
}

#[derive(Debug, Serialize)]
struct ApiToolSpec<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    function: ApiToolFunction<'a>,
}

#[derive(Debug, Serialize)]
struct ApiToolFunction<'a> {
    name: &'a str,
    description: &'a str,
    parameters: &'a Value,
}

/// Map a turn's tool specs to the wire format, treating `Some(&[])` the
/// same as `None`.
///
/// `Some(&[])` is a legitimate *definition* — `tools: []` in an agent's
/// frontmatter means "answer from the prompt alone", the summarise/judge
/// case (see `crate::agents::AgentDef::tools`) — but `#[serde(skip_serializing_if
/// = "Option::is_none")]` on `Request::tools` only skips `None`, not an
/// empty `Vec` wrapped in `Some`, so that legitimate definition used to
/// serialize as `"tools": []`. The OpenAI Chat Completions API (and every
/// backend that speaks its dialect — llama.cpp, Ollama, vLLM) rejects an
/// empty `tools` array outright, so the one shape the docs describe most
/// carefully 400'd on every OpenAI-compatible backend. There is no
/// difference in meaning between "no tools" and "an empty tools array"
/// on the wire, so folding the two here is correct, not a workaround.
fn api_tools(tools: Option<&[ToolSpec]>) -> Option<Vec<ApiToolSpec<'_>>> {
    tools.filter(|specs| !specs.is_empty()).map(|specs| {
        specs
            .iter()
            .map(|s| ApiToolSpec {
                kind: "function",
                function: ApiToolFunction {
                    name: &s.name,
                    description: &s.description,
                    parameters: &s.input_schema,
                },
            })
            .collect()
    })
}

#[derive(Debug, Serialize)]
struct ApiMessage {
    role: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<ApiContent>,
    /// Present on assistant messages that emit tool calls.
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<ApiAssistantToolCall>>,
    /// Present on `tool` role messages — links the result to a prior call.
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
enum ApiContent {
    /// Plain string — used for simple text-only messages and tool results.
    Text(String),
    /// Array of typed parts — required when images are present.
    Parts(Vec<ApiPart>),
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ApiPart {
    Text { text: String },
    ImageUrl { image_url: ApiImageUrl },
}

#[derive(Debug, Serialize)]
struct ApiImageUrl {
    /// `data:<media_type>;base64,<data>` for inline images.
    url: String,
}

#[derive(Debug, Serialize)]
struct ApiAssistantToolCall {
    id: String,
    #[serde(rename = "type")]
    kind: &'static str,
    function: ApiAssistantToolFunction,
}

#[derive(Debug, Serialize)]
struct ApiAssistantToolFunction {
    name: String,
    /// JSON-encoded string of the arguments object.
    arguments: String,
}

// ---------------------------------------------------------------------------
// Wire-format types — streaming response
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct StreamChunk {
    #[serde(default)]
    choices: Vec<StreamChoice>,
    /// Only present on the extra final chunk `include_usage` asks for, and
    /// only on servers that honour it.
    usage: Option<Usage>,
}

#[derive(Debug, Deserialize)]
struct Usage {
    #[serde(default)]
    prompt_tokens: u32,
}

#[derive(Debug, Deserialize)]
struct StreamChoice {
    #[serde(default)]
    delta: StreamDelta,
    /// Populated on the final chunk: `"stop"`, `"length"`,
    /// `"content_filter"`, `"tool_calls"`, etc.
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct StreamDelta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<StreamToolCallDelta>>,
}

#[derive(Debug, Deserialize)]
struct StreamToolCallDelta {
    /// Position within the assistant message's tool_calls array. Used as the
    /// accumulator key — the same call streams in across multiple chunks at
    /// the same `index`.
    index: usize,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<StreamFunctionDelta>,
}

#[derive(Debug, Deserialize)]
struct StreamFunctionDelta {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

// ---------------------------------------------------------------------------
// Conversion: ChatMessage -> ApiMessage(s)
// ---------------------------------------------------------------------------

/// Convert one logical `ChatMessage` into one or more wire-level `ApiMessage`s.
///
/// OpenAI's protocol splits tool results into their own `role: "tool"`
/// messages — one per result — whereas the internal `ChatMessage` collapses
/// them into a single User message with multiple `ToolResult` parts.
fn chat_message_to_api(msg: &ChatMessage) -> Vec<ApiMessage> {
    match msg.role {
        Role::User => convert_user_message(msg),
        Role::Assistant => vec![convert_assistant_message(msg)],
    }
}

fn convert_user_message(msg: &ChatMessage) -> Vec<ApiMessage> {
    let mut out = Vec::new();
    let mut text_parts: Vec<ApiPart> = Vec::new();

    for part in &msg.parts {
        match part {
            ContentPart::Text(t) => text_parts.push(ApiPart::Text { text: t.clone() }),
            ContentPart::Image {
                media_type,
                data_base64,
            } => text_parts.push(ApiPart::ImageUrl {
                image_url: ApiImageUrl {
                    url: format!("data:{media_type};base64,{data_base64}"),
                },
            }),
            // ImageRef reaches this provider only when the image cache
            // missed at re-hydration time. Surface it as a text marker
            // so the model still has the hash for context-window
            // references like "the picture above".
            ContentPart::ImageRef { media_type, sha256 } => text_parts.push(ApiPart::Text {
                text: format!("[image: {media_type} sha256={sha256} (cache miss)]"),
            }),
            ContentPart::ToolResult {
                tool_use_id,
                content,
            } => {
                out.push(ApiMessage {
                    role: "tool",
                    content: Some(ApiContent::Text(content.clone())),
                    tool_calls: None,
                    tool_call_id: Some(tool_use_id.clone()),
                });
            }
            ContentPart::ToolUse { .. } | ContentPart::ToolUseRef { .. } => {
                // Should not appear on User messages — silently skip.
            }
            // See the Anthropic provider's arm: hydration failed
            // upstream, and the pairing matters more than the content.
            ContentPart::ToolResultRef { tool_use_id, .. } => {
                out.push(ApiMessage {
                    role: "tool",
                    content: Some(ApiContent::Text(
                        crate::session_storage::MISSING_RESULT.to_string(),
                    )),
                    tool_calls: None,
                    tool_call_id: Some(tool_use_id.clone()),
                });
            }
        }
    }

    if !text_parts.is_empty() {
        let content = if text_parts.len() == 1 {
            if let ApiPart::Text { text } = &text_parts[0] {
                ApiContent::Text(text.clone())
            } else {
                ApiContent::Parts(text_parts)
            }
        } else {
            ApiContent::Parts(text_parts)
        };
        out.push(ApiMessage {
            role: "user",
            content: Some(content),
            tool_calls: None,
            tool_call_id: None,
        });
    }

    out
}

fn convert_assistant_message(msg: &ChatMessage) -> ApiMessage {
    let mut text: Option<String> = None;
    let mut tool_calls: Vec<ApiAssistantToolCall> = Vec::new();

    for part in &msg.parts {
        match part {
            ContentPart::Text(t) => {
                text.get_or_insert_with(String::new).push_str(t);
            }
            ContentPart::ToolUse { id, name, input } => {
                tool_calls.push(ApiAssistantToolCall {
                    id: id.clone(),
                    kind: "function",
                    function: ApiAssistantToolFunction {
                        name: name.clone(),
                        arguments: serde_json::to_string(input)
                            .unwrap_or_else(|_| "{}".to_string()),
                    },
                });
            }
            // See the Anthropic provider's arm: hydration failed
            // upstream, and the call matters more than its arguments.
            ContentPart::ToolUseRef { id, name, .. } => {
                tool_calls.push(ApiAssistantToolCall {
                    id: id.clone(),
                    kind: "function",
                    function: ApiAssistantToolFunction {
                        name: name.clone(),
                        arguments: serde_json::to_string(&crate::session_storage::missing_input())
                            .unwrap_or_else(|_| "{}".to_string()),
                    },
                });
            }
            // Assistant should not carry images or tool results.
            ContentPart::Image { .. }
            | ContentPart::ImageRef { .. }
            | ContentPart::ToolResult { .. }
            | ContentPart::ToolResultRef { .. } => {}
        }
    }

    ApiMessage {
        role: "assistant",
        content: text.map(ApiContent::Text),
        tool_calls: if tool_calls.is_empty() {
            None
        } else {
            Some(tool_calls)
        },
        tool_call_id: None,
    }
}

// ---------------------------------------------------------------------------
// Streaming accumulator
// ---------------------------------------------------------------------------

#[derive(Default)]
struct ToolCallAccum {
    id: String,
    name: String,
    arguments: String,
}

// ---------------------------------------------------------------------------
// Provider impl
// ---------------------------------------------------------------------------

#[async_trait]
impl Provider for OpenAICompatibleProvider {
    fn name(&self) -> &str {
        &self.name
    }

    async fn chat(
        &self,
        system: Option<&str>,
        messages: &[ChatMessage],
        tools: Option<&[ToolSpec]>,
    ) -> Result<ChatResponse> {
        let mut api_messages: Vec<ApiMessage> = Vec::new();
        if let Some(sys) = system {
            api_messages.push(ApiMessage {
                role: "system",
                content: Some(ApiContent::Text(sys.to_string())),
                tool_calls: None,
                tool_call_id: None,
            });
        }
        for m in messages {
            api_messages.extend(chat_message_to_api(m));
        }

        let api_tools = api_tools(tools);

        let body = Request {
            model: &self.model,
            messages: api_messages,
            max_tokens: self.max_tokens,
            stream: true,
            stream_options: StreamOptions {
                include_usage: true,
            },
            tools: api_tools,
        };

        let url = self.endpoint();
        debug!(
            "Sending request to OpenAI-compatible endpoint (url={url}, model={})",
            self.model
        );

        let mut req = self
            .client
            .post(&url)
            .header("content-type", "application/json")
            .json(&body);
        if let Some(key) = &self.api_key {
            req = req.bearer_auth(key);
        }

        // Every read below runs under the idle deadline — the wait for the
        // headers included, since an upstream can stall before them just as
        // well as after. See `crate::provider::http`.
        let idle = self.stream_idle_timeout;
        let stalled = || format!("OpenAI-compatible endpoint {url}");
        let response = http::idle(idle, stalled, req.send())
            .await?
            .with_context(|| format!("Failed to send request to {url}"))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = http::idle(idle, stalled, response.text())
                .await
                .ok()
                .and_then(Result::ok)
                .unwrap_or_default();
            bail!("OpenAI-compatible API error {status}: {body}");
        }

        // Parse SSE stream.
        let mut stream = response.bytes_stream();
        let mut buffer = String::new();
        let mut text_acc = String::new();
        let mut tool_acc: BTreeMap<usize, ToolCallAccum> = BTreeMap::new();
        let mut stop_reason: Option<String> = None;
        let mut prompt_tokens: Option<u32> = None;
        let mut saw_done = false;

        while let Some(chunk) = http::idle(idle, stalled, stream.next()).await? {
            let chunk = chunk.map_err(|e| IncompleteStream {
                provider: self.name.clone(),
                detail: format!("error reading the SSE stream: {e}"),
            })?;
            buffer.push_str(&String::from_utf8_lossy(&chunk));

            while let Some(pos) = buffer.find("\n\n") {
                let event_str = buffer[..pos].to_string();
                buffer.drain(..pos + 2);

                for line in event_str.lines() {
                    let Some(data) = line.strip_prefix("data: ") else {
                        continue;
                    };
                    let data = data.trim();
                    if data == "[DONE]" {
                        saw_done = true;
                        break;
                    }
                    let parsed: StreamChunk = match serde_json::from_str(data) {
                        Ok(v) => v,
                        Err(e) => {
                            debug!("Failed to parse SSE chunk: {e} | data: {data}");
                            continue;
                        }
                    };
                    if let Some(usage) = parsed.usage {
                        prompt_tokens = Some(usage.prompt_tokens);
                    }
                    for choice in parsed.choices {
                        if let Some(reason) = choice.finish_reason {
                            stop_reason = Some(reason);
                        }
                        if let Some(t) = choice.delta.content {
                            text_acc.push_str(&t);
                        }
                        if let Some(deltas) = choice.delta.tool_calls {
                            for d in deltas {
                                let entry = tool_acc.entry(d.index).or_default();
                                if let Some(id) = d.id
                                    && !id.is_empty()
                                {
                                    entry.id = id;
                                }
                                if let Some(f) = d.function {
                                    if let Some(n) = f.name
                                        && !n.is_empty()
                                    {
                                        entry.name = n;
                                    }
                                    if let Some(a) = f.arguments {
                                        entry.arguments.push_str(&a);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        // A server that closed the stream without a finish reason or the
        // `[DONE]` sentinel did not finish its answer — it died, or was
        // killed, part way through. Reported as an error rather than as
        // whatever had arrived, which is usually nothing and used to end
        // the turn silently. `RetryProvider` retries it.
        if stop_reason.is_none() && !saw_done {
            return Err(IncompleteStream {
                provider: self.name.clone(),
                detail: "the stream closed without a finish_reason or [DONE]".to_string(),
            }
            .into());
        }

        let tool_calls: Vec<ToolCall> = tool_acc
            .into_values()
            .filter(|t| !t.name.is_empty())
            .map(|t| {
                let input: Value = if t.arguments.is_empty() {
                    json!({})
                } else {
                    serde_json::from_str(&t.arguments).unwrap_or(json!({}))
                };
                ToolCall {
                    id: t.id,
                    name: t.name,
                    input,
                }
            })
            .collect();

        let text = if text_acc.is_empty() {
            None
        } else {
            Some(text_acc)
        };

        Ok(ChatResponse {
            text,
            tool_calls,
            prompt_usage: prompt_tokens.map(|tokens| PromptUsage {
                provider: self.name.clone(),
                tokens,
            }),
            stop_reason,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::ChatMessage;

    #[test]
    fn user_text_becomes_string_content() {
        let msg = ChatMessage::user("hello");
        let api = chat_message_to_api(&msg);
        assert_eq!(api.len(), 1);
        assert_eq!(api[0].role, "user");
        let json = serde_json::to_value(&api[0]).unwrap();
        assert_eq!(json["content"], "hello");
    }

    #[test]
    fn user_with_image_uses_parts_array() {
        let msg = ChatMessage::user_with_images(
            "describe this",
            vec![("image/png".to_string(), "AAAA".to_string())],
        );
        let api = chat_message_to_api(&msg);
        assert_eq!(api.len(), 1);
        let json = serde_json::to_value(&api[0]).unwrap();
        let parts = json["content"].as_array().expect("content should be array");
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0]["type"], "image_url");
        assert_eq!(parts[0]["image_url"]["url"], "data:image/png;base64,AAAA");
        assert_eq!(parts[1]["type"], "text");
        assert_eq!(parts[1]["text"], "describe this");
    }

    #[test]
    fn tool_results_split_into_separate_tool_messages() {
        let msg = ChatMessage::tool_results_with_images(
            vec![
                ("call_a".to_string(), "result_a".to_string()),
                ("call_b".to_string(), "result_b".to_string()),
            ],
            vec![],
        );
        let api = chat_message_to_api(&msg);
        assert_eq!(api.len(), 2);
        assert_eq!(api[0].role, "tool");
        assert_eq!(api[0].tool_call_id.as_deref(), Some("call_a"));
        assert_eq!(api[1].role, "tool");
        assert_eq!(api[1].tool_call_id.as_deref(), Some("call_b"));
    }

    #[test]
    fn assistant_with_tool_use_serializes_tool_calls() {
        let msg = ChatMessage::assistant_with_tools(
            Some("I'll check.".to_string()),
            vec![ToolCall {
                id: "call_1".to_string(),
                name: "get_weather".to_string(),
                input: json!({"city": "Tokyo"}),
            }],
        );
        let api = chat_message_to_api(&msg);
        assert_eq!(api.len(), 1);
        let json = serde_json::to_value(&api[0]).unwrap();
        assert_eq!(json["role"], "assistant");
        assert_eq!(json["content"], "I'll check.");
        let calls = json["tool_calls"].as_array().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["id"], "call_1");
        assert_eq!(calls[0]["function"]["name"], "get_weather");
        // arguments is a JSON-encoded string per OpenAI spec.
        let args: Value =
            serde_json::from_str(calls[0]["function"]["arguments"].as_str().unwrap()).unwrap();
        assert_eq!(args, json!({"city": "Tokyo"}));
    }

    #[test]
    fn endpoint_strips_trailing_slash() {
        let p = OpenAICompatibleProvider::new(&OpenAICompatibleConfig {
            base_url: "http://localhost:8080/v1/".to_string(),
            api_key: None,
            model: "gemma".to_string(),
            provider_name: None,
            max_tokens: 4096,
            connect_timeout_secs: 15,
            stream_idle_timeout_secs: 300,
            incomplete_retries: 2,
        });
        assert_eq!(p.endpoint(), "http://localhost:8080/v1/chat/completions");
    }

    #[test]
    fn provider_name_default_and_override() {
        let default = OpenAICompatibleProvider::new(&OpenAICompatibleConfig {
            base_url: "http://x/v1".to_string(),
            api_key: None,
            model: "m".to_string(),
            provider_name: None,
            max_tokens: 1,
            connect_timeout_secs: 15,
            stream_idle_timeout_secs: 300,
            incomplete_retries: 2,
        });
        assert_eq!(default.name(), "openai_compatible");

        let custom = OpenAICompatibleProvider::new(&OpenAICompatibleConfig {
            base_url: "http://x/v1".to_string(),
            api_key: None,
            model: "m".to_string(),
            provider_name: Some("llama_cpp".to_string()),
            max_tokens: 1,
            connect_timeout_secs: 15,
            stream_idle_timeout_secs: 300,
            incomplete_retries: 2,
        });
        assert_eq!(custom.name(), "llama_cpp");
    }

    #[test]
    fn role_is_assistant_when_no_text_only_tool_use() {
        let msg = ChatMessage::assistant_with_tools(
            None,
            vec![ToolCall {
                id: "c".into(),
                name: "n".into(),
                input: json!({}),
            }],
        );
        let api = chat_message_to_api(&msg);
        let json = serde_json::to_value(&api[0]).unwrap();
        assert_eq!(json["role"], "assistant");
        assert!(json.get("content").is_none() || json["content"].is_null());
        assert!(json["tool_calls"].is_array());
    }

    /// `tools: []` is a legitimate agent definition (an agent that
    /// answers from its prompt alone), but this is the OpenAI Chat
    /// Completions wire format, and that API rejects `"tools": []`
    /// outright. `Some(&[])` must serialize exactly like `None`: no
    /// `tools` key in the request body at all.
    #[test]
    fn an_empty_tool_list_is_not_sent_as_an_empty_tools_array() {
        let empty: &[ToolSpec] = &[];

        assert!(
            api_tools(Some(empty)).is_none(),
            "Some(&[]) must map to None, not Some(vec![])"
        );
        assert!(api_tools(None).is_none());

        let body = Request {
            model: "m",
            messages: Vec::new(),
            max_tokens: 1,
            stream: true,
            stream_options: StreamOptions {
                include_usage: true,
            },
            tools: api_tools(Some(empty)),
        };
        let json = serde_json::to_value(&body).unwrap();
        assert!(
            json.get("tools").is_none(),
            "the wire body must omit `tools` entirely, not send `[]`: {json}"
        );
    }

    /// A non-empty spec list still reaches the wire, so the fix above
    /// isn't accidentally swallowing real tool lists too.
    #[test]
    fn a_non_empty_tool_list_still_serializes() {
        let specs = [ToolSpec {
            name: "get_weather".into(),
            description: "…".into(),
            input_schema: json!({"type": "object"}),
        }];
        let mapped = api_tools(Some(&specs)).expect("a non-empty list must stay Some");
        assert_eq!(mapped.len(), 1);
        assert_eq!(mapped[0].function.name, "get_weather");
    }
    /// The usage block only arrives if we ask for it, and it arrives on a
    /// chunk carrying no choices — which must not be mistaken for an empty
    /// response.
    #[test]
    fn the_request_asks_for_usage_and_the_final_chunk_carries_it() {
        let body = Request {
            model: "m",
            messages: Vec::new(),
            max_tokens: 1,
            stream: true,
            stream_options: StreamOptions {
                include_usage: true,
            },
            tools: None,
        };
        let json = serde_json::to_value(&body).unwrap();
        assert_eq!(json["stream_options"]["include_usage"], true);

        let chunk: StreamChunk = serde_json::from_str(
            r#"{"choices":[],"usage":{"prompt_tokens":151673,"completion_tokens":0}}"#,
        )
        .unwrap();
        assert!(chunk.choices.is_empty());
        assert_eq!(chunk.usage.unwrap().prompt_tokens, 151673);
    }

    /// A provider pointed at `addr`, with an idle deadline short enough for a
    /// test to wait out.
    fn stalling_target(addr: std::net::SocketAddr) -> OpenAICompatibleProvider {
        let mut p = OpenAICompatibleProvider::new(&OpenAICompatibleConfig {
            base_url: format!("http://{addr}/v1"),
            api_key: None,
            model: "m".to_string(),
            provider_name: None,
            max_tokens: 1,
            connect_timeout_secs: 15,
            stream_idle_timeout_secs: 300,
            incomplete_retries: 2,
        });
        p.stream_idle_timeout = Some(Duration::from_millis(200));
        p
    }

    /// Accept one connection, drain the request, write `preamble`, then hold
    /// the socket open without another byte — the shape of an upstream that
    /// stalls (#258). Returns the address to point a provider at.
    async fn stalling_server(preamble: &'static str) -> std::net::SocketAddr {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 8192];
            let _ = socket.read(&mut buf).await;
            socket.write_all(preamble.as_bytes()).await.unwrap();
            std::future::pending::<()>().await;
        });
        addr
    }

    /// The hang #258 was filed for: headers and the first chunk arrive, then
    /// the stream goes silent. The turn has to fail at the idle deadline
    /// rather than wait on `stream.next()` forever.
    #[tokio::test]
    async fn a_stream_that_stalls_mid_response_fails_at_the_idle_deadline() {
        let addr = stalling_server(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
             transfer-encoding: chunked\r\n\r\n\
             22\r\ndata: {\"choices\":[{\"delta\":{}}]}\n\n\r\n",
        )
        .await;
        let result = tokio::time::timeout(
            Duration::from_secs(10),
            stalling_target(addr).chat(None, &[ChatMessage::user("hi")], None),
        )
        .await
        .expect("the provider's own deadline must fire before the test's");
        let err = result.expect_err("a stalled stream must be an error, not a reply");
        assert!(
            format!("{err:#}").contains("sent nothing"),
            "the error must say the stream stalled: {err:#}"
        );
    }

    /// The same stall one step earlier: the connection is accepted but the
    /// headers never come. `send()` is under the deadline too.
    #[tokio::test]
    async fn an_upstream_that_never_sends_headers_fails_at_the_idle_deadline() {
        let addr = stalling_server("").await;
        let result = tokio::time::timeout(
            Duration::from_secs(10),
            stalling_target(addr).chat(None, &[ChatMessage::user("hi")], None),
        )
        .await
        .expect("the provider's own deadline must fire before the test's");
        let err = result.expect_err("a silent upstream must be an error, not a reply");
        assert!(
            format!("{err:#}").contains("sent nothing"),
            "the error must say the upstream stalled: {err:#}"
        );
    }

    /// A server that dies part way through its answer: one content chunk,
    /// then the stream ends cleanly with no `finish_reason` and no
    /// `[DONE]`. That is not a reply, and must not be read as an empty one.
    #[tokio::test]
    async fn a_stream_that_ends_without_a_finish_reason_is_incomplete() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 8192];
            let _ = socket.read(&mut buf).await;
            let event = "data: {\"choices\":[{\"delta\":{\"content\":\"hal\"}}]}\n\n";
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                 transfer-encoding: chunked\r\n\r\n{:x}\r\n{event}\r\n0\r\n\r\n",
                event.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        });
        let err = stalling_target(addr)
            .chat(None, &[ChatMessage::user("hi")], None)
            .await
            .expect_err("a stream cut off mid-answer must be an error");
        assert!(
            err.downcast_ref::<IncompleteStream>().is_some(),
            "must be an IncompleteStream so RetryProvider retries it: {err:#}"
        );
    }

    /// A server that ignores `stream_options` sends chunks with no usage at
    /// all; that has to stay a parse, not an error.
    #[test]
    fn a_chunk_without_usage_still_parses() {
        let chunk: StreamChunk =
            serde_json::from_str(r#"{"choices":[{"delta":{"content":"hi"}}]}"#).unwrap();
        assert!(chunk.usage.is_none());
    }
}
