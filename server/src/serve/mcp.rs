//! MCP (Model Context Protocol) server endpoint.
//!
//! Exposes:
//!   - `POST /mcp` — JSON-RPC 2.0 dispatch for MCP `initialize`,
//!     `tools/list`, `tools/call`, and `notifications/initialized`.
//!
//! Tools published here are aimed at external AI clients (Claude Code
//! and similar) that want to share context with sapphire-agent:
//!
//!   - `write_report` — record a unit of work the external AI just
//!     finished. Stored as a per-project session under
//!     `sessions/<namespace>/mcp/<ULID>.jsonl` and returned with a
//!     short acknowledgement from sapphire-agent.
//!   - `recall_memory` — at session start, fetch the project's prior
//!     compact summary plus the most recent reports so the external
//!     AI can pick up where the user left off — possibly across
//!     hosts.
//!
//! Auth: `Authorization: Bearer <token>` resolved through `DeviceAuth`
//! (same mechanism as `/a2a`). The match implicitly resolves which
//! room_profile this MCP session writes/reads as, so clients don't name
//! the profile. Configure a room_profile with `rooms = []` (no chat
//! rooms) for MCP-only use.
//!
//! Trust boundary (asymmetric):
//!   - Writes flow into the room_profile's namespace as ordinary
//!     session traffic and are included in daily digests / MEMORY.md
//!     compaction.
//!   - Reads (`recall_memory`) are scoped strictly to the requested
//!     project's session — namespace-wide memory is never returned,
//!     so general agent self-memory cannot leak back into the
//!     external AI's context.

use std::sync::Arc;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::{Json, response::Response};
use serde_json::{Value, json};
use tracing::warn;

use super::{ServeState, extract_bearer};
use crate::provider::ChatMessage;
use crate::session::ReportMeta;

/// MCP protocol revision this server implements. Matches the version
/// the in-process MCP *client* (`src/mcp_client/`) speaks, so the same
/// negotiation logic is exercised end-to-end during local testing.
const PROTOCOL_VERSION: &str = "2025-03-26";

/// JSON-RPC error codes. Standard 2.0 codes plus an
/// application-level "auth required" code so token failures still
/// come back inside a JSON-RPC envelope when the request already
/// passed JSON parsing.
mod codes {
    pub const PARSE_ERROR: i32 = -32700;
    pub const INVALID_REQUEST: i32 = -32600;
    pub const METHOD_NOT_FOUND: i32 = -32601;
    pub const INVALID_PARAMS: i32 = -32602;
    pub const AUTH_REQUIRED: i32 = -32001;
}

// ---------------------------------------------------------------------------
// Envelope
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub async fn handle_mcp_post(
    State(state): State<Arc<ServeState>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let envelope: JsonRpcEnvelope = match serde_json::from_slice(&body) {
        Ok(env) => env,
        Err(e) => {
            return jsonrpc_error(
                Value::Null,
                codes::PARSE_ERROR,
                &format!("invalid JSON: {e}"),
            );
        }
    };
    let req_id = envelope.id.clone().unwrap_or(Value::Null);
    if envelope.jsonrpc.as_deref() != Some("2.0") {
        return jsonrpc_error(
            req_id,
            codes::INVALID_REQUEST,
            "jsonrpc field must be \"2.0\"",
        );
    }

    // Notifications carry no id and expect no response body. MCP sends
    // `notifications/initialized` after `initialize`; future spec
    // revisions may add more. Anything we don't recognize is silently
    // accepted because per JSON-RPC notifications never error back.
    if envelope.id.is_none() {
        return (StatusCode::ACCEPTED, "").into_response();
    }

    // Bearer auth: empty/missing token → 401 (HTTP layer), unknown
    // token → JSON-RPC AUTH_REQUIRED so well-behaved clients can
    // distinguish "no creds sent" from "creds rejected".
    let bearer = match extract_bearer(&headers) {
        Some(b) => b,
        None => return (StatusCode::UNAUTHORIZED, "missing bearer token").into_response(),
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
            return jsonrpc_error(
                req_id,
                codes::AUTH_REQUIRED,
                "unknown or revoked bearer token",
            );
        }
    };

    match envelope.method.as_str() {
        "initialize" => handle_initialize(req_id),
        "tools/list" => handle_tools_list(req_id),
        "tools/call" => handle_tools_call(state, req_id, envelope.params, profile_name).await,
        other => jsonrpc_error(
            req_id,
            codes::METHOD_NOT_FOUND,
            &format!("MCP method '{other}' is not implemented"),
        ),
    }
}

// ---------------------------------------------------------------------------
// initialize
// ---------------------------------------------------------------------------

fn handle_initialize(req_id: Value) -> Response {
    let result = json!({
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": {
            "tools": { "listChanged": false }
        },
        "serverInfo": {
            "name": "sapphire-agent",
            "version": env!("CARGO_PKG_VERSION"),
        }
    });
    jsonrpc_result(req_id, result)
}

// ---------------------------------------------------------------------------
// tools/list
// ---------------------------------------------------------------------------

fn handle_tools_list(req_id: Value) -> Response {
    let tools = json!([
        {
            "name": "write_report",
            "description":
                "Report a unit of work back to sapphire-agent. The agent files it under \
                 the named project's memory and replies with a brief acknowledgement \
                 that can be shown to the user. Call this when a user-visible task is \
                 complete or at the end of a coding session.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "project": {
                        "type": "string",
                        "description":
                            "Logical project key (typically the repository name). \
                             Stable across hosts and tools — re-using the same value \
                             continues the same project's session."
                    },
                    "summary": {
                        "type": "string",
                        "description": "One-line summary of what was just done."
                    },
                    "body": {
                        "type": "string",
                        "description": "Optional longer description, decisions, follow-ups."
                    },
                    "files": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Optional list of file paths touched by this work."
                    },
                    "source": {
                        "type": "string",
                        "description":
                            "Identifier for the calling tool. Defaults to \"claude-code\" \
                             when omitted."
                    },
                    "hostname": {
                        "type": "string",
                        "description":
                            "Originating host. Recommended when the same project may be \
                             worked on from multiple machines."
                    }
                },
                "required": ["project", "summary"]
            }
        },
        {
            "name": "recall_memory",
            "description":
                "Recall prior context for a project from sapphire-agent. Returns the \
                 project's compacted summary plus the most recent reports. Call this \
                 at the start of a session to pick up where work was left off, \
                 possibly on another host.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "project": {
                        "type": "string",
                        "description":
                            "Project key to recall. Should match what `write_report` \
                             was called with for the same project."
                    },
                    "limit": {
                        "type": "integer",
                        "description":
                            "Maximum number of recent reports to return verbatim. \
                             Older content is reflected in the project_summary. \
                             Defaults to 20.",
                        "minimum": 1
                    }
                },
                "required": ["project"]
            }
        }
    ]);
    jsonrpc_result(req_id, json!({ "tools": tools }))
}

// ---------------------------------------------------------------------------
// tools/call dispatcher
// ---------------------------------------------------------------------------

async fn handle_tools_call(
    state: Arc<ServeState>,
    req_id: Value,
    params: Option<Value>,
    profile_name: String,
) -> Response {
    let params = params.unwrap_or(Value::Null);
    let name = match params.get("name").and_then(|v| v.as_str()) {
        Some(n) => n.to_string(),
        None => {
            return jsonrpc_error(
                req_id,
                codes::INVALID_PARAMS,
                "tools/call requires a `name` field",
            );
        }
    };
    let args = params.get("arguments").cloned().unwrap_or(Value::Null);

    match name.as_str() {
        "write_report" => {
            let result = match call_write_report(state, profile_name, args).await {
                Ok(v) => v,
                Err(e) => tool_text_error(&format!("write_report failed: {e}")),
            };
            jsonrpc_result(req_id, result)
        }
        "recall_memory" => {
            let result = match call_recall_memory(state, profile_name, args).await {
                Ok(v) => v,
                Err(e) => tool_text_error(&format!("recall_memory failed: {e}")),
            };
            jsonrpc_result(req_id, result)
        }
        other => jsonrpc_result(req_id, tool_text_error(&format!("unknown tool '{other}'"))),
    }
}

// ---------------------------------------------------------------------------
// write_report
// ---------------------------------------------------------------------------

/// Default `source` tag stamped onto reports that don't pass one.
/// Chosen because Claude Code is the primary intended client and
/// makes the wire shorter for that case.
const DEFAULT_SOURCE: &str = "claude-code";

/// System prompt for the ねぎらい (acknowledgement) reply. Kept
/// short so the model focuses on warmth and brevity rather than
/// reproducing the report content. Language is left to the model
/// because reports themselves may come in any language.
const ACK_SYSTEM_PROMPT: &str = "You are sapphire-agent, the user's personal partner AI. Their external AI \
     assistant (such as Claude Code) is reporting a unit of coding work it just \
     completed on the user's behalf. Acknowledge the report warmly and briefly \
     (1-3 sentences). Speak naturally — your reply is shown both to the assistant \
     and to the user. Reply in the same language as the report's content.";

async fn call_write_report(
    state: Arc<ServeState>,
    profile_name: String,
    args: Value,
) -> anyhow::Result<Value> {
    let project = require_string(&args, "project")?;
    let summary = require_string(&args, "summary")?;
    let body = optional_string(&args, "body");
    let files = optional_string_array(&args, "files");
    let source = optional_string(&args, "source").unwrap_or_else(|| DEFAULT_SOURCE.to_string());
    let hostname = optional_string(&args, "hostname");

    let namespace = state
        .config
        .namespace_for_room_profile(&profile_name)
        .to_string();

    let session_id = state
        .mcp_session_for_project_or_create(&namespace, &project)
        .await?;

    let report_text = render_report(
        &summary,
        body.as_deref(),
        files.as_deref(),
        &source,
        hostname.as_deref(),
    );
    let report_meta = ReportMeta {
        source: source.clone(),
        hostname: hostname.clone(),
        summary: summary.clone(),
        body: body.clone(),
        files: files.clone(),
    };

    state
        .mcp_session_store
        .append_report(&session_id, &report_text, report_meta)?;

    // Compose the ねぎらい. The model only sees the rendered report —
    // we don't feed prior reports back here because the goal is a
    // quick acknowledgement, not a session-aware response. Recall
    // happens through `recall_memory`, not as a side effect of write.
    let provider = state.registry.for_profile(&state.config, &profile_name);
    let chat_response = provider
        .chat(
            Some(ACK_SYSTEM_PROMPT),
            &[ChatMessage::user(report_text.clone())],
            None,
        )
        .await;

    let ack_text = match chat_response {
        Ok(resp) => resp
            .text
            .filter(|t| !t.trim().is_empty())
            .unwrap_or_else(|| default_ack(&project)),
        Err(e) => {
            // Don't fail the whole write — the report is already on
            // disk. Falling back to a canned ack keeps the contract
            // (always return a message) while the user still sees
            // their work was filed.
            warn!("MCP write_report: ack LLM call failed: {e}");
            default_ack(&project)
        }
    };

    // Persist the ack as the assistant's reply so the session reads
    // back as a normal conversation in recall_memory output.
    if let Err(e) = state
        .mcp_session_store
        .append(&session_id, &ChatMessage::assistant(ack_text.clone()))
    {
        warn!("MCP write_report: failed to persist ack to session {session_id}: {e}");
    }

    Ok(tool_text_ok(&ack_text))
}

fn render_report(
    summary: &str,
    body: Option<&str>,
    files: Option<&[String]>,
    source: &str,
    hostname: Option<&str>,
) -> String {
    // Inline the source/hostname so the rendered text alone is
    // self-describing — useful both for the ack-LLM context and for
    // recall_memory's later replay.
    let header = match hostname {
        Some(h) => format!("[Report from {source} on {h}]"),
        None => format!("[Report from {source}]"),
    };
    let mut out = format!("{header}\nSummary: {summary}");
    if let Some(b) = body
        && !b.trim().is_empty()
    {
        out.push_str("\n\nDetails:\n");
        out.push_str(b);
    }
    if let Some(fs) = files
        && !fs.is_empty()
    {
        out.push_str("\n\nFiles touched:");
        for f in fs {
            out.push_str(&format!("\n- {f}"));
        }
    }
    out
}

fn default_ack(project: &str) -> String {
    format!("ありがとう、{project} の作業お疲れさま。記録しておいたよ。")
}

// ---------------------------------------------------------------------------
// recall_memory
// ---------------------------------------------------------------------------

/// Cap on `limit` regardless of what the caller asks. Picked to keep
/// the rendered briefing under a few KB even with verbose reports —
/// callers wanting more should rely on `project_summary` instead.
const RECALL_LIMIT_MAX: usize = 100;
const RECALL_LIMIT_DEFAULT: usize = 20;

async fn call_recall_memory(
    state: Arc<ServeState>,
    profile_name: String,
    args: Value,
) -> anyhow::Result<Value> {
    let project = require_string(&args, "project")?;
    let limit = args
        .get("limit")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize)
        .unwrap_or(RECALL_LIMIT_DEFAULT)
        .clamp(1, RECALL_LIMIT_MAX);

    let namespace = state
        .config
        .namespace_for_room_profile(&profile_name)
        .to_string();

    let session_id = state.mcp_session_for_project(&namespace, &project).await;

    let Some(session_id) = session_id else {
        // Unknown project is the normal first-call shape — return an
        // empty briefing rather than an error so the client can
        // proceed (often into a fresh `write_report` cycle).
        return Ok(tool_text_ok(&render_empty_briefing(&project)));
    };

    let (messages, summary_line) = state
        .mcp_session_store
        .load_session_full(&session_id)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "project '{project}' is indexed but session file {session_id} is missing"
            )
        })?;

    let project_summary = summary_line.map(|s| s.summary).unwrap_or_default();

    // Take the last `limit` user reports. Each has `report_meta`
    // populated (assistant ack messages don't, so they're skipped —
    // the recall view is about prior work, not the agent's prior
    // replies).
    let reports: Vec<_> = messages
        .into_iter()
        .filter(|m| m.report_meta.is_some())
        .collect();
    let recent: Vec<_> = reports
        .into_iter()
        .rev()
        .take(limit)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();

    Ok(tool_text_ok(&render_briefing(
        &project,
        &project_summary,
        &recent,
    )))
}

fn render_empty_briefing(project: &str) -> String {
    format!(
        "# Recall for project: {project}\n\n\
         No prior reports have been filed for this project yet. \
         Use `write_report` to file the first one when work is complete."
    )
}

fn render_briefing(
    project: &str,
    project_summary: &str,
    reports: &[crate::session::StoredMessage],
) -> String {
    let mut out = format!("# Recall for project: {project}\n");

    if !project_summary.trim().is_empty() {
        out.push_str("\n## Project summary (older history, compacted)\n\n");
        out.push_str(project_summary);
        out.push('\n');
    }

    if reports.is_empty() {
        out.push_str(
            "\n## Recent reports\n\n(no recent reports — the project summary above is the full record)\n",
        );
        return out;
    }

    out.push_str(&format!(
        "\n## Recent reports ({} entries)\n",
        reports.len()
    ));
    for (idx, msg) in reports.iter().enumerate() {
        let Some(meta) = &msg.report_meta else {
            continue;
        };
        let ts = msg.timestamp.format("%Y-%m-%d %H:%M UTC");
        let origin = match &meta.hostname {
            Some(h) => format!("{} on {}", meta.source, h),
            None => meta.source.clone(),
        };
        out.push_str(&format!("\n### Report {} — {ts} ({origin})\n", idx + 1));
        out.push_str(&format!("Summary: {}\n", meta.summary));
        if let Some(body) = &meta.body
            && !body.trim().is_empty()
        {
            out.push_str("\nDetails:\n");
            out.push_str(body);
            out.push('\n');
        }
        if let Some(files) = &meta.files
            && !files.is_empty()
        {
            out.push_str("\nFiles:\n");
            for f in files {
                out.push_str(&format!("- {f}\n"));
            }
        }
    }

    out
}

// ---------------------------------------------------------------------------
// Argument helpers
// ---------------------------------------------------------------------------

fn require_string(args: &Value, field: &str) -> anyhow::Result<String> {
    args.get(field)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("missing required string argument '{field}'"))
}

fn optional_string(args: &Value, field: &str) -> Option<String> {
    args.get(field)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty())
}

fn optional_string_array(args: &Value, field: &str) -> Option<Vec<String>> {
    args.get(field).and_then(|v| v.as_array()).map(|arr| {
        arr.iter()
            .filter_map(|v| v.as_str().map(|s| s.to_string()))
            .collect()
    })
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn jsonrpc_result(id: Value, result: Value) -> Response {
    let body = json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result,
    });
    (StatusCode::OK, Json(body)).into_response()
}

fn jsonrpc_error(id: Value, code: i32, message: &str) -> Response {
    let body = json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message },
    });
    (StatusCode::OK, Json(body)).into_response()
}

/// Build a tools/call result that surfaces an in-tool error to the
/// model. MCP convention is to return `isError: true` with a text
/// content part, NOT a JSON-RPC error — the latter is reserved for
/// protocol-level failures.
fn tool_text_error(message: &str) -> Value {
    json!({
        "content": [{
            "type": "text",
            "text": message,
        }],
        "isError": true,
    })
}

/// Successful tool result carrying a single text part. Used by tools
/// whose output is a short rendered string (the ねぎらい reply, the
/// recall payload).
fn tool_text_ok(text: &str) -> Value {
    json!({
        "content": [{
            "type": "text",
            "text": text,
        }],
        "isError": false,
    })
}
