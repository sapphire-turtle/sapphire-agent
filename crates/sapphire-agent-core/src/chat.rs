//! Streaming client for the `chat` JSON-RPC method.
//!
//! The legacy text-only path used by the CLI REPL still lives in
//! [`crate::run`] (private `send_chat` helper) and prints raw text to
//! stdout. GUI clients want typed events instead — text delivered as
//! soon as the LLM finishes, audio chunks streamed in parallel for
//! playback — which is what [`chat_stream`] provides.
//!
//! The server emits progress notifications with `kind` =
//! `tool_start` / `tool_end` / `assistant_text` / `audio_chunk` /
//! `tts_error`, followed by a final JSON-RPC `result` carrying the
//! reply text. Only `assistant_text` / `audio_chunk` / `tts_error` are
//! new — they are emitted only when the client opts in via
//! `modalities: ["text", "audio"]`.

use anyhow::Result;
use base64::Engine;
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::mpsc;

static REQUEST_ID: AtomicU64 = AtomicU64::new(1);

fn next_id() -> u64 {
    REQUEST_ID.fetch_add(1, Ordering::Relaxed)
}

/// Output modalities the GUI is prepared to consume on a single chat
/// turn. The CLI text-only flow does not use this enum — it calls the
/// existing private `send_chat` helper that never asks for audio.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatModality {
    /// Final reply text (always returned via the JSON-RPC `result`).
    Text,
    /// Server-side TTS synthesis, streamed as `AudioChunk` events.
    /// Requires the session's room_profile to have a `voice_pipeline`;
    /// otherwise the server emits a single `TtsError` and continues
    /// with text-only.
    Audio,
}

impl ChatModality {
    fn as_str(self) -> &'static str {
        match self {
            ChatModality::Text => "text",
            ChatModality::Audio => "audio",
        }
    }
}

/// Streaming event emitted while a `chat` call is in flight. Mirrors
/// the shape of [`crate::voice::VoiceEvent`] but specialised for the
/// text-first chat path.
#[derive(Debug, Clone)]
pub enum ChatEvent {
    /// A tool call started during the LLM turn.
    ToolStart { name: String },
    /// A tool call finished during the LLM turn.
    ToolEnd { name: String },
    /// Assistant's full reply text. Emitted *before* audio chunks when
    /// audio was requested, so GUIs can render text immediately
    /// without waiting for TTS to finish.
    AssistantText { text: String },
    /// One chunk of synthesized speech (mono s16le @ 16 kHz). Only
    /// emitted when the request asked for `ChatModality::Audio`.
    AudioChunk { pcm: Vec<i16> },
    /// Best-effort TTS notice: emitted when audio was requested but the
    /// server couldn't produce it (no voice_pipeline configured, TTS
    /// provider missing / failed, etc.). Text still arrives via `Done`.
    TtsError { message: String },
    /// Final reply text from the JSON-RPC result. The chat turn is
    /// complete; no further events follow.
    Done { content: String },
    /// JSON-RPC error — the turn failed before producing text.
    Error { message: String },
}

/// Run one chat turn, streaming progress + result events into
/// `event_tx`. The future completes when the server sends its final
/// `result` (or `error`); closing `event_tx` is the caller's
/// responsibility (drop the receiver to unblock playback consumers).
///
/// `modalities` selects which output the server should produce. An
/// empty slice is treated as `[Text]` (server default) — historically
/// the CLI sent no `modalities` field at all, which the server reads
/// as text-only.
pub async fn chat_stream(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    session_id: &str,
    content: &str,
    modalities: &[ChatModality],
    event_tx: mpsc::Sender<ChatEvent>,
) -> Result<()> {
    let base = base.trim_end_matches('/');

    let mut params = json!({ "content": content });
    if !modalities.is_empty() {
        let mods: Vec<&str> = modalities.iter().map(|m| m.as_str()).collect();
        params["modalities"] = json!(mods);
    }
    let body = json!({
        "jsonrpc": "2.0",
        "id": next_id(),
        "method": "chat",
        "params": params,
    });

    let resp = client
        .post(format!("{base}/rpc"))
        .bearer_auth(token)
        .header("session-id", session_id)
        .header("accept", "text/event-stream")
        .json(&body)
        .send()
        .await?
        .error_for_status()?;

    let mut stream = resp.bytes_stream();
    let mut buf = String::new();

    while let Some(chunk) = stream.next().await {
        buf.push_str(&String::from_utf8_lossy(&chunk?));
        while let Some(pos) = buf.find("\n\n") {
            let raw = buf[..pos].to_string();
            buf.drain(..pos + 2);
            let Some(val) = parse_sse_data(&raw) else {
                continue;
            };
            if dispatch_event(&val, &event_tx).await {
                return Ok(());
            }
        }
    }
    Ok(())
}

fn parse_sse_data(raw: &str) -> Option<Value> {
    let data_line = raw.lines().find(|l| l.starts_with("data:"))?;
    let data = data_line.strip_prefix("data:").unwrap_or("").trim();
    serde_json::from_str(data).ok()
}

/// Translate one JSON-RPC SSE event into a [`ChatEvent`]. Returns
/// `true` when the turn is finished (final result or error) so the
/// outer read loop can stop.
async fn dispatch_event(val: &Value, tx: &mpsc::Sender<ChatEvent>) -> bool {
    if let Some(method) = val["method"].as_str() {
        let params = &val["params"];
        match method {
            "tool_start" => {
                let name = params["name"].as_str().unwrap_or("?").to_string();
                let _ = tx.send(ChatEvent::ToolStart { name }).await;
            }
            "tool_end" => {
                let name = params["name"].as_str().unwrap_or("?").to_string();
                let _ = tx.send(ChatEvent::ToolEnd { name }).await;
            }
            "notifications/progress" => match params["kind"].as_str() {
                Some("assistant_text") => {
                    let text = params["text"].as_str().unwrap_or("").to_string();
                    let _ = tx.send(ChatEvent::AssistantText { text }).await;
                }
                Some("audio_chunk") => {
                    if let Some(b64) = params["data"].as_str()
                        && let Ok(bytes) =
                            base64::engine::general_purpose::STANDARD.decode(b64.as_bytes())
                    {
                        // s16le pairs → i16.
                        let pcm: Vec<i16> = bytes
                            .as_chunks::<2>()
                            .0
                            .iter()
                            .map(|c| i16::from_le_bytes([c[0], c[1]]))
                            .collect();
                        let _ = tx.send(ChatEvent::AudioChunk { pcm }).await;
                    }
                }
                Some("tts_error") => {
                    let message = params["message"]
                        .as_str()
                        .unwrap_or("TTS failed")
                        .to_string();
                    let _ = tx.send(ChatEvent::TtsError { message }).await;
                }
                _ => {}
            },
            _ => {}
        }
        false
    } else if val.get("result").is_some() {
        let content = val["result"]["content"].as_str().unwrap_or("").to_string();
        let _ = tx.send(ChatEvent::Done { content }).await;
        true
    } else if let Some(err) = val.get("error") {
        let message = err["message"]
            .as_str()
            .unwrap_or("unknown error")
            .to_string();
        let _ = tx.send(ChatEvent::Error { message }).await;
        true
    } else {
        false
    }
}
