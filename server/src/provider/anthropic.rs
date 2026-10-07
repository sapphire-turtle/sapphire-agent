use crate::config::AnthropicConfig;
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

const ANTHROPIC_API_URL: &str = "https://api.anthropic.com/v1/messages";
const ANTHROPIC_VERSION: &str = "2023-06-01";

pub struct AnthropicProvider {
    api_key: String,
    model: String,
    light_model: Option<String>,
    max_tokens: u32,
    client: Client,
    stream_idle_timeout: Option<Duration>,
}

impl AnthropicProvider {
    pub fn new(cfg: &AnthropicConfig) -> Result<Self> {
        Ok(Self {
            api_key: cfg.resolve_api_key()?,
            model: cfg.model.clone(),
            light_model: cfg.light_model.clone(),
            max_tokens: cfg.max_tokens,
            client: http::client(http::secs(cfg.connect_timeout_secs)),
            stream_idle_timeout: http::secs(cfg.stream_idle_timeout_secs),
        })
    }

    /// Choose model based on message content.
    /// Uses `light_model` for casual chat, `model` for coding-related requests.
    fn select_model(&self, messages: &[ChatMessage]) -> &str {
        let Some(light) = &self.light_model else {
            return &self.model;
        };
        let last_user_text = messages
            .iter()
            .rev()
            .find(|m| m.role == Role::User)
            .and_then(|m| m.text())
            .unwrap_or_default();
        if is_coding_related(&last_user_text) {
            &self.model
        } else {
            light
        }
    }
}

/// Heuristic: return true if the text looks like a coding/technical request.
fn is_coding_related(text: &str) -> bool {
    if text.contains("```") {
        return true;
    }
    let lower = text.to_lowercase();
    let keywords = [
        "code",
        "implement",
        "function",
        "method",
        "class",
        "struct",
        "enum",
        "trait",
        "bug",
        "error",
        "debug",
        "fix",
        "compile",
        "refactor",
        "test",
        "algorithm",
        "api",
        "library",
        "crate",
        "cargo",
        "npm",
        "syntax",
        "variable",
        "type",
        "rust",
        "python",
        "javascript",
        "typescript",
        "java",
        "go ",
        " sql",
        "bash",
        "script",
        "コード",
        "実装",
        "関数",
        "バグ",
        "エラー",
        "デバッグ",
    ];
    keywords.iter().any(|kw| lower.contains(kw))
}

// ---------------------------------------------------------------------------
// Request types
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct Request<'a> {
    model: &'a str,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<&'a str>,
    messages: Vec<ApiMessage>,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<ApiToolSpec<'a>>>,
}

#[derive(Debug, Serialize)]
struct ApiToolSpec<'a> {
    name: &'a str,
    description: &'a str,
    input_schema: &'a Value,
}

/// Map a turn's tool specs to the wire format, treating `Some(&[])` the
/// same as `None`.
///
/// `Some(&[])` is a legitimate *definition* — `tools: []` in an agent's
/// frontmatter means "answer from the prompt alone", the summarise/judge
/// case (see `crate::agents::AgentDef::tools`) — but
/// `#[serde(skip_serializing_if = "Option::is_none")]` on `Request::tools`
/// only skips `None`, not an empty `Vec` wrapped in `Some`, so that
/// legitimate definition used to serialize as `"tools": []`. Anthropic's
/// API tolerates that shape, but OpenAI-compatible backends reject it
/// outright — folding the two here keeps this provider's wire format
/// consistent with `openai_compatible`'s rather than relying on one
/// backend's leniency.
fn api_tools(tools: Option<&[ToolSpec]>) -> Option<Vec<ApiToolSpec<'_>>> {
    tools.filter(|specs| !specs.is_empty()).map(|specs| {
        specs
            .iter()
            .map(|s| ApiToolSpec {
                name: &s.name,
                description: &s.description,
                input_schema: &s.input_schema,
            })
            .collect()
    })
}

#[derive(Debug, Serialize)]
struct ApiMessage {
    role: String,
    content: ApiContent,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
enum ApiContent {
    /// Simple string (user/assistant text only — wire-format shorthand).
    Text(String),
    /// Array of typed content blocks (required for tool use/results).
    Parts(Vec<ApiPart>),
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ApiPart {
    Text {
        text: String,
    },
    Image {
        source: ApiImageSource,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
    },
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ApiImageSource {
    Base64 { media_type: String, data: String },
}

// ---------------------------------------------------------------------------
// SSE response types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
#[allow(dead_code)]
enum SseEvent {
    #[serde(rename = "message_start")]
    MessageStart { message: MessageStartData },
    #[serde(rename = "content_block_start")]
    ContentBlockStart {
        index: usize,
        content_block: ContentBlockMeta,
    },
    #[serde(rename = "content_block_delta")]
    ContentBlockDelta { index: usize, delta: Delta },
    #[serde(rename = "content_block_stop")]
    ContentBlockStop { index: usize },
    #[serde(rename = "message_delta")]
    MessageDelta { delta: MessageDeltaData },
    #[serde(rename = "message_stop")]
    MessageStop,
    #[serde(rename = "error")]
    Error { error: ApiError },
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct MessageStartData {
    id: String,
    model: String,
    usage: Option<Usage>,
}

/// The prompt half of Anthropic's usage block, as `message_start` reports it.
///
/// Cached input is billed differently but read all the same, so all three
/// counters add up to what the model actually had in front of it — which is
/// the number the context budget cares about.
#[derive(Debug, Deserialize)]
struct Usage {
    #[serde(default)]
    input_tokens: u32,
    #[serde(default)]
    cache_creation_input_tokens: u32,
    #[serde(default)]
    cache_read_input_tokens: u32,
}

impl Usage {
    fn prompt_tokens(&self) -> u32 {
        self.input_tokens
            .saturating_add(self.cache_creation_input_tokens)
            .saturating_add(self.cache_read_input_tokens)
    }
}

#[derive(Debug, Deserialize)]
struct ContentBlockMeta {
    #[serde(rename = "type")]
    kind: String,
    /// Present for `tool_use` blocks.
    id: Option<String>,
    name: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum Delta {
    #[serde(rename = "text_delta")]
    Text { text: String },
    #[serde(rename = "input_json_delta")]
    InputJson { partial_json: String },
}

#[derive(Debug, Deserialize)]
struct MessageDeltaData {
    stop_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ApiError {
    message: String,
}

// ---------------------------------------------------------------------------
// Block accumulator (for streaming)
// ---------------------------------------------------------------------------

enum Block {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input_json: String,
    },
}

// ---------------------------------------------------------------------------
// Conversion helpers
// ---------------------------------------------------------------------------

fn chat_message_to_api(msg: &ChatMessage) -> ApiMessage {
    let role = match msg.role {
        Role::User => "user",
        Role::Assistant => "assistant",
    };

    // If there's exactly one Text part and no other parts, use the wire shorthand.
    if msg.parts.len() == 1
        && let ContentPart::Text(text) = &msg.parts[0]
    {
        return ApiMessage {
            role: role.to_string(),
            content: ApiContent::Text(text.clone()),
        };
    }

    let parts: Vec<ApiPart> = msg
        .parts
        .iter()
        .map(|p| match p {
            ContentPart::Text(t) => ApiPart::Text { text: t.clone() },
            ContentPart::Image {
                media_type,
                data_base64,
            } => ApiPart::Image {
                source: ApiImageSource::Base64 {
                    media_type: media_type.clone(),
                    data: data_base64.clone(),
                },
            },
            // ImageRef reaches this provider only when the image cache
            // missed at re-hydration time. Surface it as a text marker
            // so the model still has the hash for context-window
            // references like "the picture above".
            ContentPart::ImageRef { media_type, sha256 } => ApiPart::Text {
                text: format!("[image: {media_type} sha256={sha256} (cache miss)]"),
            },
            ContentPart::ToolUse { id, name, input } => ApiPart::ToolUse {
                id: id.clone(),
                name: name.clone(),
                input: input.clone(),
            },
            ContentPart::ToolResult {
                tool_use_id,
                content,
            } => ApiPart::ToolResult {
                tool_use_id: tool_use_id.clone(),
                content: content.clone(),
            },
            // Only reachable when a read path failed to hydrate. Keep
            // the call — dropping it would orphan the `tool_result` that
            // answers it — and let the model see that it asked for
            // something it can no longer read back.
            ContentPart::ToolUseRef { id, name, .. } => ApiPart::ToolUse {
                id: id.clone(),
                name: name.clone(),
                input: crate::session_storage::missing_input(),
            },
            // Only reachable when a read path failed to hydrate. Keep
            // the pairing — that is what the API validates — and let the
            // model call the tool again if it needs the content.
            ContentPart::ToolResultRef { tool_use_id, .. } => ApiPart::ToolResult {
                tool_use_id: tool_use_id.clone(),
                content: crate::session_storage::MISSING_RESULT.to_string(),
            },
        })
        .collect();

    ApiMessage {
        role: role.to_string(),
        content: ApiContent::Parts(parts),
    }
}

// ---------------------------------------------------------------------------
// Provider implementation
// ---------------------------------------------------------------------------

#[async_trait]
impl Provider for AnthropicProvider {
    fn name(&self) -> &str {
        "anthropic"
    }

    async fn chat(
        &self,
        system: Option<&str>,
        messages: &[ChatMessage],
        tools: Option<&[ToolSpec]>,
    ) -> Result<ChatResponse> {
        let api_messages: Vec<ApiMessage> = messages.iter().map(chat_message_to_api).collect();

        let api_tools = api_tools(tools);

        let model = self.select_model(messages);

        let body = Request {
            model,
            max_tokens: self.max_tokens,
            system,
            messages: api_messages,
            stream: true,
            tools: api_tools,
        };

        debug!("Sending request to Anthropic API (model={model})");

        // Every read below runs under the idle deadline — the wait for the
        // headers included, since an upstream can stall before them just as
        // well as after. See `crate::provider::http`.
        let idle = self.stream_idle_timeout;
        let stalled = || "Anthropic API".to_string();
        let request = self
            .client
            .post(ANTHROPIC_API_URL)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("content-type", "application/json")
            .json(&body)
            .send();
        let response = http::idle(idle, stalled, request)
            .await?
            .context("Failed to send request to Anthropic API")?;

        if !response.status().is_success() {
            let status = response.status();
            let body = http::idle(idle, stalled, response.text())
                .await
                .ok()
                .and_then(Result::ok)
                .unwrap_or_default();
            bail!("Anthropic API error {status}: {body}");
        }

        // Parse SSE stream, tracking content blocks by index.
        let mut stream = response.bytes_stream();
        let mut buffer = String::new();
        // BTreeMap preserves insertion order by index.
        let mut blocks: BTreeMap<usize, Block> = BTreeMap::new();
        let mut stop_reason: Option<String> = None;
        let mut prompt_tokens: Option<u32> = None;

        while let Some(chunk) = http::idle(idle, stalled, stream.next()).await? {
            let chunk = chunk.map_err(|e| IncompleteStream {
                provider: self.name().to_string(),
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
                    if data == "[DONE]" {
                        break;
                    }
                    match serde_json::from_str::<SseEvent>(data) {
                        Ok(SseEvent::ContentBlockStart {
                            index,
                            content_block,
                        }) => match content_block.kind.as_str() {
                            "text" => {
                                blocks.insert(
                                    index,
                                    Block::Text {
                                        text: String::new(),
                                    },
                                );
                            }
                            "tool_use" => {
                                blocks.insert(
                                    index,
                                    Block::ToolUse {
                                        id: content_block.id.unwrap_or_default(),
                                        name: content_block.name.unwrap_or_default(),
                                        input_json: String::new(),
                                    },
                                );
                            }
                            _ => {}
                        },
                        Ok(SseEvent::ContentBlockDelta { index, delta }) => match delta {
                            Delta::Text { text } => {
                                if let Some(Block::Text { text: acc }) = blocks.get_mut(&index) {
                                    acc.push_str(&text);
                                }
                            }
                            Delta::InputJson { partial_json } => {
                                if let Some(Block::ToolUse { input_json, .. }) =
                                    blocks.get_mut(&index)
                                {
                                    input_json.push_str(&partial_json);
                                }
                            }
                        },
                        Ok(SseEvent::MessageStart { message }) => {
                            prompt_tokens = message.usage.map(|u| u.prompt_tokens());
                        }
                        Ok(SseEvent::MessageDelta { delta }) => {
                            if let Some(reason) = delta.stop_reason {
                                stop_reason = Some(reason);
                            }
                        }
                        Ok(SseEvent::Error { error }) => {
                            bail!("Anthropic stream error: {}", error.message);
                        }
                        Ok(_) => {}
                        Err(e) => {
                            debug!("Failed to parse SSE event: {e} | data: {data}");
                        }
                    }
                }
            }
        }

        // `message_delta` carries the stop reason just before
        // `message_stop`; a stream that closed without one did not finish.
        // See the matching check in `openai_compatible`.
        if stop_reason.is_none() {
            return Err(IncompleteStream {
                provider: self.name().to_string(),
                detail: "the stream closed without a stop_reason".to_string(),
            }
            .into());
        }

        // Assemble final response from accumulated blocks.
        let mut text_parts: Vec<String> = Vec::new();
        let mut tool_calls: Vec<ToolCall> = Vec::new();

        for block in blocks.into_values() {
            match block {
                Block::Text { text } if !text.is_empty() => {
                    text_parts.push(text);
                }
                Block::ToolUse {
                    id,
                    name,
                    input_json,
                } => {
                    let input = if input_json.is_empty() {
                        json!({})
                    } else {
                        serde_json::from_str(&input_json).unwrap_or(json!({}))
                    };
                    tool_calls.push(ToolCall { id, name, input });
                }
                _ => {}
            }
        }

        let text = if text_parts.is_empty() {
            None
        } else {
            Some(text_parts.join(""))
        };
        Ok(ChatResponse {
            text,
            tool_calls,
            prompt_usage: prompt_tokens.map(|tokens| PromptUsage {
                provider: self.name().to_string(),
                tokens,
            }),
            stop_reason,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `tools: []` is a legitimate agent definition (an agent that
    /// answers from its prompt alone), and `Some(&[])` must serialize
    /// exactly like `None`: no `tools` key in the request body at all —
    /// see `api_tools`'s doc for why, even though Anthropic's own API
    /// happens to tolerate `"tools": []`.
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
            max_tokens: 1,
            system: None,
            messages: Vec::new(),
            stream: true,
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
        assert_eq!(mapped[0].name, "get_weather");
    }
    /// Cached input is read even when it is not re-billed, so all three
    /// counters are part of what the model had in front of it — and the
    /// context budget cares about what was read, not what it cost.
    #[test]
    fn prompt_tokens_sum_the_cached_and_uncached_input() {
        let event: SseEvent = serde_json::from_str(
            r#"{"type":"message_start","message":{"id":"m","model":"claude","usage":
               {"input_tokens":100,"cache_creation_input_tokens":20,
                "cache_read_input_tokens":4000,"output_tokens":1}}}"#,
        )
        .unwrap();
        let SseEvent::MessageStart { message } = event else {
            panic!("expected message_start");
        };
        assert_eq!(message.usage.unwrap().prompt_tokens(), 4120);
    }

    /// An older or trimmed `message_start` must not break the parse; it just
    /// teaches the calibration nothing.
    #[test]
    fn a_message_start_without_usage_still_parses() {
        let event: SseEvent = serde_json::from_str(
            r#"{"type":"message_start","message":{"id":"m","model":"claude"}}"#,
        )
        .unwrap();
        let SseEvent::MessageStart { message } = event else {
            panic!("expected message_start");
        };
        assert!(message.usage.is_none());
    }
}
