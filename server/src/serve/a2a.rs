//! A2A (Agent2Agent Protocol) v1 server endpoints.
//!
//! Exposes:
//!   - `POST /a2a` — JSON-RPC 2.0 dispatch. Currently only `SendMessage`
//!     is implemented (synchronous: receive a `Message`, drive the
//!     internal chat turn to completion, return a terminal `Task`).
//!   - `GET /.well-known/agent-card.json` — A2A agent card discovery.
//!
//! Auth: `Authorization: Bearer <token>` resolved through `DeviceAuth` —
//! token -> device -> room profile. The match resolves the request's
//! `room_profile` implicitly — clients don't need to (and can't) name
//! it. A token leak therefore exposes one device's profile, not all.
//!
//! Out of scope (v1): `SendStreamingMessage` (SSE), `GetTask`,
//! `CancelTask`, `SubscribeToTask`, push notifications, `FilePart` `url`
//! (privacy/security: requires explicit operator opt-in), `DataPart`.
//! `FilePart` inline (`raw` base64) for `image/*` is supported and routes
//! through the existing multimodal provider path. Wire-format types come
//! from `a2a-lf`; the JSON-RPC dispatch is hand-rolled here to share
//! `ServeState` with the existing `/rpc` endpoint.

use std::sync::Arc;

use a2a::{
    Message, Part, PartContent, Role, SendMessageRequest, SendMessageResponse, Task, TaskState,
    TaskStatus, new_task_id,
};
use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use base64::{Engine, engine::general_purpose::STANDARD as BASE64_STANDARD};
use chrono::Utc;
use serde_json::{Value, json};

use super::{ServeState, extract_bearer};
use crate::provider::ChatMessage;

/// Method name for the only A2A method we implement. The A2A v1 spec
/// uses PascalCase JSON-RPC method names (`SendMessage`, `GetTask`,
/// …), unlike the v0.3 slash form (`message/send`).
const METHOD_SEND_MESSAGE: &str = "SendMessage";

/// JSON-RPC error codes used by this handler. Standard 2.0 codes plus
/// the A2A-spec-aligned application range (-32000 … -32099 reserved).
mod codes {
    pub const PARSE_ERROR: i32 = -32700;
    pub const INVALID_REQUEST: i32 = -32600;
    pub const METHOD_NOT_FOUND: i32 = -32601;
    pub const INVALID_PARAMS: i32 = -32602;
    pub const INTERNAL_ERROR: i32 = -32603;
    /// A2A "authenticated request required" application-level code.
    pub const AUTH_REQUIRED: i32 = -32001;
}

// ---------------------------------------------------------------------------
// Agent Card
// ---------------------------------------------------------------------------

/// `GET /.well-known/agent-card.json` — A2A discovery endpoint.
///
/// Returns 404 when A2A is disabled so probes don't reveal that the
/// server exists at all when the operator hasn't opted in.
pub async fn handle_agent_card(State(state): State<Arc<ServeState>>) -> impl IntoResponse {
    let cfg = match state.config.a2a.as_ref() {
        Some(cfg) if cfg.enabled => cfg,
        _ => return (StatusCode::NOT_FOUND, "A2A disabled").into_response(),
    };

    let name = cfg
        .agent_name
        .clone()
        .unwrap_or_else(|| "sapphire-agent".to_string());
    let description = cfg.agent_description.clone().unwrap_or_else(|| {
        "Personal partner AI agent with persistent character, memory, and tools.".to_string()
    });
    let url = cfg.public_url.clone().unwrap_or_default();

    let card = json!({
        "name": name,
        "description": description,
        "version": env!("CARGO_PKG_VERSION"),
        "supportedInterfaces": [
            {
                "url": url,
                "protocolBinding": "jsonrpc",
                "protocolVersion": a2a::VERSION,
            }
        ],
        "capabilities": {
            "streaming": false,
            "pushNotifications": false,
            "extendedAgentCard": false,
        },
        "defaultInputModes": ["text/plain", "image/jpeg", "image/png", "image/gif", "image/webp"],
        "defaultOutputModes": ["text/plain"],
        "skills": [
            {
                "id": "chat",
                "name": "Chat with the agent",
                "description":
                    "Hold a multi-turn conversation; the agent remembers context across calls \
                     within the same contextId and applies its server-side persona / memory. \
                     Inline images (FilePart raw) are accepted for vision-capable backends.",
                "tags": ["chat", "conversation", "vision"],
                "inputModes": ["text/plain", "image/jpeg", "image/png", "image/gif", "image/webp"],
                "outputModes": ["text/plain"]
            }
        ],
        "securitySchemes": {
            "bearer": {
                "httpAuthSecurityScheme": {
                    "scheme": "Bearer"
                }
            }
        },
        "securityRequirements": [
            { "bearer": [] }
        ]
    });

    (StatusCode::OK, Json(card)).into_response()
}

// ---------------------------------------------------------------------------
// POST /a2a — JSON-RPC 2.0 dispatch
// ---------------------------------------------------------------------------

/// JSON-RPC 2.0 envelope. Kept local so we don't depend on `a2a::JsonRpcId`
/// (which would force us to convert IDs in/out of the existing
/// serde_json `Value` flow).
#[derive(Debug, serde::Deserialize)]
struct JsonRpcEnvelope {
    #[serde(default)]
    jsonrpc: Option<String>,
    #[serde(default)]
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Option<Value>,
}

pub async fn handle_a2a_post(
    State(state): State<Arc<ServeState>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> axum::response::Response {
    // 0. Feature gate
    let enabled = state.config.a2a.as_ref().is_some_and(|c| c.enabled);
    if !enabled {
        return (StatusCode::NOT_FOUND, "A2A disabled").into_response();
    }

    // 1. Parse envelope. We parse manually so a malformed body returns
    //    a JSON-RPC parse-error response rather than a 400 — A2A
    //    clients expect a JSON-RPC envelope on every reply.
    let envelope: JsonRpcEnvelope = match serde_json::from_slice(&body) {
        Ok(env) => env,
        Err(e) => {
            return jsonrpc_error_response(
                Value::Null,
                codes::PARSE_ERROR,
                &format!("invalid JSON: {e}"),
            );
        }
    };
    let req_id = envelope.id.clone().unwrap_or(Value::Null);
    if envelope.jsonrpc.as_deref() != Some("2.0") {
        return jsonrpc_error_response(
            req_id,
            codes::INVALID_REQUEST,
            "jsonrpc field must be \"2.0\"",
        );
    }

    // 2. Bearer auth → room_profile reverse lookup. Empty/missing bearer
    //    returns 401 at the HTTP layer (no JSON-RPC envelope) per A2A
    //    spec: auth failures are an HTTP-level concern, not an
    //    application error.
    let bearer = match extract_bearer(&headers) {
        Some(b) => b,
        None => {
            return (StatusCode::UNAUTHORIZED, "missing bearer token").into_response();
        }
    };
    let profile_name = match state.device_auth.resolve(&bearer).await {
        Ok(r) => r.room_profile,
        Err(crate::device_auth::AuthError::Unavailable) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                "cannot check the bearer token now",
            )
                .into_response();
        }
        Err(crate::device_auth::AuthError::Refused) => {
            return jsonrpc_error_response(
                req_id,
                codes::AUTH_REQUIRED,
                "unknown or revoked bearer token",
            );
        }
    };

    // 3. Method dispatch
    match envelope.method.as_str() {
        METHOD_SEND_MESSAGE => {
            handle_send_message(state, req_id, envelope.params, profile_name).await
        }
        other => jsonrpc_error_response(
            req_id,
            codes::METHOD_NOT_FOUND,
            &format!("A2A method '{other}' is not implemented"),
        ),
    }
}

// ---------------------------------------------------------------------------
// SendMessage
// ---------------------------------------------------------------------------

async fn handle_send_message(
    state: Arc<ServeState>,
    req_id: Value,
    params: Option<Value>,
    profile_name: String,
) -> axum::response::Response {
    let raw = params.unwrap_or(Value::Null);
    let request: SendMessageRequest = match serde_json::from_value(raw) {
        Ok(r) => r,
        Err(e) => {
            return jsonrpc_error_response(
                req_id,
                codes::INVALID_PARAMS,
                &format!("malformed SendMessageRequest: {e}"),
            );
        }
    };

    // Extract text + inline image parts. `FilePart` with `url` is rejected
    // (server-side URL fetch is a privacy/security choice that should
    // require explicit operator opt-in); `DataPart` is rejected in v1
    // until we have a structured-tool routing story.
    let (user_text, images) = match collect_text_and_images(&request.message.parts) {
        Ok(v) => v,
        Err(e) => {
            return jsonrpc_error_response(req_id, codes::INVALID_PARAMS, &e);
        }
    };
    if user_text.trim().is_empty() && images.is_empty() {
        return jsonrpc_error_response(
            req_id,
            codes::INVALID_PARAMS,
            "message must contain at least one non-empty text part or an inline image",
        );
    }
    let user_msg = if images.is_empty() {
        ChatMessage::user(&user_text)
    } else {
        ChatMessage::user_with_images(&user_text, images)
    };

    // contextId: client-supplied (resume) or server-generated (new). The
    // params-level field wins over message-level if both are present —
    // A2A spec lets either carry it.
    let context_id = request
        .message
        .context_id
        .clone()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(a2a::new_context_id);

    // Internal session id: deterministic from (profile, contextId) so a
    // crash-restart with the same contextId resumes the same JSONL
    // session. Prefix `a2a-` makes the kind self-documenting in logs;
    // hyphens (not colons) keep the resulting `<session_id>.jsonl`
    // filename portable to Windows.
    let session_id = format!("a2a-{profile_name}-{context_id}");

    // Pin the profile so `run_llm_turn`'s provider/namespace resolution
    // picks up the right room_profile.
    state
        .session_room_profiles
        .lock()
        .await
        .insert(session_id.clone(), profile_name.clone());

    // run_llm_turn's tool_start/tool_end/error progress is discarded here:
    // A2A v1 is synchronous and doesn't surface intermediate progress to
    // the caller.
    let outcome = super::run_llm_turn(
        Arc::clone(&state),
        session_id.clone(),
        user_msg,
        Arc::new(super::NullProgress),
        None,
    )
    .await;

    // Build the terminal Task. Failed turns surface as TASK_STATE_FAILED
    // with a diagnostic message part, matching what A2A clients expect.
    let (state_enum, reply_text) = match outcome.text {
        Some(t) if !t.is_empty() => (TaskState::Completed, t),
        Some(_) => (TaskState::Failed, "(empty response)".to_string()),
        None => (
            TaskState::Failed,
            "agent failed to generate a reply".to_string(),
        ),
    };

    // Mint the new task id once and reference it from both the Task and
    // the inner reply Message. Per A2A v1.0, `Message.taskId` identifies
    // the task this message belongs to — without it, clients that key
    // off `result.status.message.taskId` see `None` and may treat the
    // reply as orphaned.
    let task_id = new_task_id();
    let reply_message = Message {
        message_id: a2a::new_message_id(),
        context_id: Some(context_id.clone()),
        task_id: Some(task_id.clone()),
        role: Role::Agent,
        parts: vec![Part::text(reply_text)],
        metadata: None,
        extensions: None,
        reference_task_ids: None,
    };

    let task = Task {
        id: task_id,
        context_id,
        status: TaskStatus {
            state: state_enum,
            message: Some(reply_message),
            timestamp: Some(Utc::now()),
        },
        artifacts: None,
        history: None,
        metadata: None,
    };

    let response = SendMessageResponse::Task(task);
    let result = match serde_json::to_value(&response) {
        Ok(v) => v,
        Err(e) => {
            return jsonrpc_error_response(
                req_id,
                codes::INTERNAL_ERROR,
                &format!("failed to serialize Task: {e}"),
            );
        }
    };

    let body = json!({
        "jsonrpc": "2.0",
        "id": req_id,
        "result": result,
    });
    (StatusCode::OK, Json(body)).into_response()
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Extract text and inline images from `parts`. Returns
/// `(joined_text, Vec<(media_type, base64)>)` on success.
///
/// Rejects (with a caller-facing error string for JSON-RPC -32602):
/// - `PartContent::Url`: server-side URL fetch is a privacy/security
///   surface that needs explicit operator opt-in.
/// - `PartContent::Data`: structured-input routing is out of scope for v1.
/// - `PartContent::Raw` with a non-`image/*` `mediaType`: only the
///   multimodal provider path (image) is wired through today.
/// - `PartContent::Raw` without any `mediaType`: ambiguous; the spec
///   makes it optional but we need it to dispatch.
fn collect_text_and_images(parts: &[Part]) -> Result<(String, Vec<(String, String)>), String> {
    let mut text = String::new();
    let mut images: Vec<(String, String)> = Vec::new();
    for p in parts {
        match &p.content {
            PartContent::Text(s) => {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(s);
            }
            PartContent::Raw(bytes) => {
                let Some(media_type) = p.media_type.as_deref() else {
                    return Err(
                        "FilePart with raw content requires a mediaType (e.g. image/jpeg)"
                            .to_string(),
                    );
                };
                if !media_type.starts_with("image/") {
                    return Err(format!(
                        "unsupported mediaType '{media_type}' for inline FilePart \
                         (v1 only routes image/* to the multimodal provider)"
                    ));
                }
                images.push((media_type.to_string(), BASE64_STANDARD.encode(bytes)));
            }
            PartContent::Url(_) => {
                return Err("FilePart with url is not supported (server-side URL fetch \
                     requires explicit operator opt-in)"
                    .to_string());
            }
            PartContent::Data(_) => {
                return Err("DataPart is not supported in A2A v1 of this agent".to_string());
            }
        }
    }
    Ok((text, images))
}

fn jsonrpc_error_response(id: Value, code: i32, message: &str) -> axum::response::Response {
    let body = json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message },
    });
    (StatusCode::OK, Json(body)).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn collect_text_only() {
        let parts = vec![Part::text("hello"), Part::text("world")];
        let (text, images) = collect_text_and_images(&parts).unwrap();
        assert_eq!(text, "hello\nworld");
        assert!(images.is_empty());
    }

    #[test]
    fn collect_text_plus_inline_image() {
        let img_bytes = b"\xff\xd8\xff\xe0fake-jpeg".to_vec();
        let parts = vec![
            Part::text("look at this"),
            Part::raw(img_bytes.clone()).with_media_type("image/jpeg"),
        ];
        let (text, images) = collect_text_and_images(&parts).unwrap();
        assert_eq!(text, "look at this");
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].0, "image/jpeg");
        // Round-trip: the encoded blob must decode back to the original bytes.
        let decoded = BASE64_STANDARD.decode(&images[0].1).unwrap();
        assert_eq!(decoded, img_bytes);
    }

    #[test]
    fn raw_part_without_media_type_is_rejected() {
        let parts = vec![Part::raw(vec![1, 2, 3])];
        let err = collect_text_and_images(&parts).unwrap_err();
        assert!(err.contains("mediaType"), "got: {err}");
    }

    #[test]
    fn raw_part_with_non_image_media_type_is_rejected() {
        let parts = vec![Part::raw(vec![1, 2, 3]).with_media_type("application/pdf")];
        let err = collect_text_and_images(&parts).unwrap_err();
        assert!(err.contains("application/pdf"), "got: {err}");
    }

    #[test]
    fn url_part_is_rejected() {
        let parts = vec![Part::url("https://example.com/foo.jpg").with_media_type("image/jpeg")];
        let err = collect_text_and_images(&parts).unwrap_err();
        assert!(err.contains("url"), "got: {err}");
    }

    #[test]
    fn data_part_is_rejected() {
        let parts = vec![Part::data(json!({"k": "v"}))];
        let err = collect_text_and_images(&parts).unwrap_err();
        assert!(err.to_lowercase().contains("datapart"), "got: {err}");
    }
}
