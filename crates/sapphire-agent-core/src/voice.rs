//! Client-side helpers for the `voice/pipeline_run` JSON-RPC method
//! exposed on the sapphire-agent control endpoint (`/rpc`).
//!
//! Wraps the request/response shape used by the server in `serve.rs`:
//! audio in (base64 s16le mono 16 kHz) → SSE progress events
//! (`stt_final`, `assistant_text`, `audio_chunk`) → final JSON result
//! (`transcript`, `assistant_text`).
//!
//! Audio I/O is intentionally not handled here — that's the
//! satellite's job (see `sapphire-agent-cli`'s voice subcommand). This
//! module deals only in `Vec<i16>` PCM and JSON-RPC over HTTP/SSE.

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

/// Pipeline sample rate. Must match the server-side constant in
/// `sapphire-agent::voice::PIPELINE_SAMPLE_RATE`. Duplicated here so
/// the RPC crate doesn't depend on the agent binary.
pub const PIPELINE_SAMPLE_RATE: u32 = 16_000;

/// Wake-word configuration fetched from the server's `voice/config`
/// method. The model is `None` when no wake word is configured on
/// the server; satellites in that case run in VAD-only mode.
#[derive(Debug, Clone, Default)]
pub struct WakeWordConfig {
    /// Inline openWakeWord classifier — `None` if voice.wake_word_model
    /// is unset server-side. Wake words must be custom-trained for
    /// AI names that aren't in any pre-trained KWS vocabulary, which
    /// makes openWakeWord the only realistic engine.
    pub model: Option<WakeWordModel>,
}

/// openWakeWord classifier ONNX, distributed inline so the satellite
/// never needs direct access to the agent's filesystem. The satellite
/// caches by `sha256` and reuses across runs.
#[derive(Debug, Clone)]
pub struct WakeWordModel {
    pub filename: String,
    pub sha256: String,
    /// Decoded model bytes. `voice_config()` does the base64 decode so
    /// callers stay in pure Rust types.
    pub bytes: Vec<u8>,
}

/// Fetch the server's global wake-word configuration. Returns an
/// empty `WakeWordConfig` when `[voice].wake_word_model` is unset.
///
/// `token` is sent as `Authorization: Bearer <token>` — the `/rpc`
/// endpoint is uniformly authenticated, so even this stateless lookup
/// requires the satellite's bearer.
pub async fn voice_config(
    client: &reqwest::Client,
    base: &str,
    token: &str,
) -> Result<WakeWordConfig> {
    let base = base.trim_end_matches('/');
    let body = json!({
        "jsonrpc": "2.0",
        "id": next_id(),
        "method": "voice/config",
        "params": {},
    });
    let val: Value = client
        .post(format!("{base}/rpc"))
        .bearer_auth(token)
        .json(&body)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    if let Some(err) = val.get("error") {
        let msg = err["message"].as_str().unwrap_or("unknown error");
        anyhow::bail!("voice/config: {msg}");
    }
    let model = parse_wake_word_model(&val["result"]["wake_word_model"])?;
    Ok(WakeWordConfig { model })
}

fn parse_wake_word_model(value: &Value) -> Result<Option<WakeWordModel>> {
    if value.is_null() {
        return Ok(None);
    }
    let format = match value["format"].as_str() {
        Some(f) => f,
        None => return Ok(None),
    };
    if format != "onnx_inline" {
        anyhow::bail!("voice/config: unknown wake_word_model format '{format}'");
    }
    let filename = value["filename"]
        .as_str()
        .unwrap_or("wake.onnx")
        .to_string();
    let sha256 = value["sha256"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("voice/config: onnx_inline missing 'sha256'"))?
        .to_string();
    let data_b64 = value["data_b64"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("voice/config: onnx_inline missing 'data_b64'"))?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data_b64.as_bytes())
        .map_err(|e| anyhow::anyhow!("voice/config: onnx_inline base64 decode: {e}"))?;
    Ok(Some(WakeWordModel {
        filename,
        sha256,
        bytes,
    }))
}

/// Streaming events emitted while a `voice/pipeline_run` call is in
/// flight. Consumers typically:
///   * push `AudioChunk` PCM into a playback queue as soon as it
///     arrives (low-latency speech)
///   * display `SttFinal` / `AssistantText` in a transcript view
///   * stop reading on `Done` or `Error`
#[derive(Debug, Clone)]
pub enum VoiceEvent {
    /// Pipeline stage started (e.g. "stt", "intent", "tts").
    StageStart { stage: String },
    /// Pipeline stage ended.
    StageEnd { stage: String },
    /// Final transcript from STT.
    SttFinal { text: String },
    /// Assistant's text reply (echo of what gets synthesized to audio).
    AssistantText { text: String },
    /// One chunk of synthesized speech (mono s16le @ 16 kHz).
    AudioChunk { pcm: Vec<i16> },
    /// Tool call started during the LLM turn.
    ToolStart { name: String },
    /// Tool call finished during the LLM turn.
    ToolEnd { name: String },
    /// Pipeline completed; carries the same transcript + reply text the
    /// caller can also reconstruct from prior `SttFinal` / `AssistantText`.
    Done {
        transcript: String,
        assistant_text: String,
    },
    /// Pipeline failed — message describes the JSON-RPC error.
    Error { message: String },
}

/// Server-initiated push delivered over `voice/subscribe`. One push
/// represents a single TTS-able message (typically a heartbeat fire) —
/// it begins with `PushStart`, streams `AudioChunk`s, optionally
/// includes an `AssistantText` for transcript logging, and ends with
/// `PushDone`. The subscription stays open across multiple pushes; the
/// satellite uses `PushDone` to flush playback and re-enter listening
/// state.
#[derive(Debug, Clone)]
pub enum VoicePushEvent {
    /// A new push is starting; `task` is the heartbeat task name when
    /// the push originates from cron, or `None` for ad-hoc pushes.
    PushStart { task: Option<String> },
    /// Assistant's text reply (echo of what gets synthesized to audio).
    AssistantText { text: String },
    /// One chunk of synthesized speech (mono s16le @ 16 kHz).
    AudioChunk { pcm: Vec<i16> },
    /// Push completed. The satellite should drain the playback queue
    /// and enter follow-up listening (so the user can reply to the
    /// notification without uttering the wake word).
    PushDone,
    /// Push failed. The satellite should log and continue waiting for
    /// the next push.
    Error { message: String },
}

/// Run one voice pipeline pass: upload `pcm` as a single utterance,
/// stream progress events into `event_tx`, and return when the
/// server emits its final JSON-RPC result. Closing `event_tx` is the
/// caller's responsibility — it happens automatically when the
/// returned future is dropped, but explicit closure helps the
/// playback consumer exit.
///
/// `device_id` + the room_profile resolved from `token` are the
/// conversation-routing key. The server derives a deterministic
/// session id from this pair so the satellite can resume the same
/// conversation across restarts / network blips without juggling
/// explicit session tokens.
#[allow(clippy::too_many_arguments)]
pub async fn voice_pipeline_run(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    device_id: &str,
    pcm: &[i16],
    language: Option<&str>,
    device: Option<&crate::DeviceMetadata>,
    event_tx: mpsc::Sender<VoiceEvent>,
) -> Result<()> {
    let base = base.trim_end_matches('/');

    // Encode PCM (mono s16le) as base64.
    let mut bytes = Vec::with_capacity(pcm.len() * 2);
    for s in pcm {
        bytes.extend_from_slice(&s.to_le_bytes());
    }
    let audio_b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);

    let mut params = json!({
        "audio": audio_b64,
        "device_id": device_id,
    });
    if let Some(l) = language {
        params["language"] = json!(l);
    }
    if let Some(d) = device.and_then(crate::DeviceMetadata::to_params) {
        params["device"] = d;
    }
    let body = json!({
        "jsonrpc": "2.0",
        "id": next_id(),
        "method": "voice/pipeline_run",
        "params": params,
    });

    let resp = client
        .post(format!("{base}/rpc"))
        .bearer_auth(token)
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
            let raw: String = buf.drain(..pos + 2).collect();
            let Some(value) = parse_sse_data(&raw) else {
                continue;
            };
            if dispatch_event(&value, &event_tx).await {
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

/// Open a long-lived subscription for server-initiated voice pushes.
/// `event_tx` receives one [`VoicePushEvent`] per server notification
/// until the connection closes. Caller is expected to spawn this on a
/// background task and reconnect (with backoff) when the future
/// returns — the typical flow is "until satellite shuts down."
///
/// Routing is by `(device_id, room_profile)` — the same key
/// `voice/pipeline_run` uses to derive its session id; `room_profile`
/// comes from the bearer `token`. A new subscription supersedes any
/// prior subscription for the same key, so reconnecting after a
/// network blip just re-binds the channel.
pub async fn voice_subscribe(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    device_id: &str,
    event_tx: mpsc::Sender<VoicePushEvent>,
) -> Result<()> {
    let base = base.trim_end_matches('/');
    let body = json!({
        "jsonrpc": "2.0",
        "id": next_id(),
        "method": "voice/subscribe",
        "params": {
            "device_id": device_id,
        },
    });

    let resp = client
        .post(format!("{base}/rpc"))
        .bearer_auth(token)
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
            let raw: String = buf.drain(..pos + 2).collect();
            let Some(value) = parse_sse_data(&raw) else {
                continue;
            };
            if dispatch_push_event(&value, &event_tx).await {
                return Ok(());
            }
        }
    }
    Ok(())
}

/// Map one parsed JSON-RPC frame on the subscribe stream to a
/// [`VoicePushEvent`]. Returns `true` when the stream should be
/// considered closed (final result / error / `unsubscribe` notice).
async fn dispatch_push_event(val: &Value, tx: &mpsc::Sender<VoicePushEvent>) -> bool {
    if let Some(method) = val["method"].as_str() {
        let params = &val["params"];
        let evt = match method {
            "notifications/voice_push" => {
                let kind = params["kind"].as_str().unwrap_or("");
                match kind {
                    "push_start" => Some(VoicePushEvent::PushStart {
                        task: params["task"].as_str().map(String::from),
                    }),
                    "assistant_text" => {
                        params["text"]
                            .as_str()
                            .map(|s| VoicePushEvent::AssistantText {
                                text: s.to_string(),
                            })
                    }
                    "audio_chunk" => params["data"].as_str().and_then(decode_push_audio_chunk),
                    "push_done" => Some(VoicePushEvent::PushDone),
                    "error" => Some(VoicePushEvent::Error {
                        message: params["message"]
                            .as_str()
                            .unwrap_or("unknown error")
                            .to_string(),
                    }),
                    _ => None,
                }
            }
            _ => None,
        };
        if let Some(evt) = evt {
            let _ = tx.send(evt).await;
        }
        return false;
    }
    if let Some(err) = val.get("error") {
        let message = err["message"]
            .as_str()
            .unwrap_or("unknown error")
            .to_string();
        let _ = tx.send(VoicePushEvent::Error { message }).await;
        return true;
    }
    // Final result on the subscribe channel means the server is closing us.
    val.get("result").is_some()
}

fn decode_push_audio_chunk(b64: &str) -> Option<VoicePushEvent> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64.as_bytes())
        .ok()?;
    if bytes.len() % 2 != 0 {
        return None;
    }
    let pcm: Vec<i16> = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| i16::from_le_bytes([c[0], c[1]]))
        .collect();
    Some(VoicePushEvent::AudioChunk { pcm })
}

/// Map one parsed JSON-RPC frame to a `VoiceEvent` and push it. Returns
/// `true` when the stream should be considered complete (final result
/// or error received).
async fn dispatch_event(val: &Value, tx: &mpsc::Sender<VoiceEvent>) -> bool {
    // Notification frames.
    if let Some(method) = val["method"].as_str() {
        let params = &val["params"];
        let evt = match method {
            "notifications/progress" => {
                let kind = params["kind"].as_str().unwrap_or("");
                match kind {
                    "stage" => {
                        let stage = params["stage"].as_str().unwrap_or("").to_string();
                        match params["status"].as_str() {
                            Some("start") => Some(VoiceEvent::StageStart { stage }),
                            Some("end") => Some(VoiceEvent::StageEnd { stage }),
                            _ => None,
                        }
                    }
                    "stt_final" => params["text"].as_str().map(|s| VoiceEvent::SttFinal {
                        text: s.to_string(),
                    }),
                    "assistant_text" => {
                        params["text"].as_str().map(|s| VoiceEvent::AssistantText {
                            text: s.to_string(),
                        })
                    }
                    "audio_chunk" => params["data"].as_str().and_then(decode_audio_chunk),
                    _ => None,
                }
            }
            "tool_start" => params["name"].as_str().map(|s| VoiceEvent::ToolStart {
                name: s.to_string(),
            }),
            "tool_end" => params["name"].as_str().map(|s| VoiceEvent::ToolEnd {
                name: s.to_string(),
            }),
            _ => None,
        };
        if let Some(evt) = evt {
            let _ = tx.send(evt).await;
        }
        return false;
    }

    // Final result / error frames carry an `id`.
    if val.get("result").is_some() {
        let result = &val["result"];
        let transcript = result["transcript"].as_str().unwrap_or("").to_string();
        let assistant_text = result["assistant_text"].as_str().unwrap_or("").to_string();
        let _ = tx
            .send(VoiceEvent::Done {
                transcript,
                assistant_text,
            })
            .await;
        return true;
    }
    if let Some(err) = val.get("error") {
        let message = err["message"]
            .as_str()
            .unwrap_or("unknown error")
            .to_string();
        let _ = tx.send(VoiceEvent::Error { message }).await;
        return true;
    }
    false
}

fn decode_audio_chunk(b64: &str) -> Option<VoiceEvent> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64.as_bytes())
        .ok()?;
    if bytes.len() % 2 != 0 {
        return None;
    }
    let pcm: Vec<i16> = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| i16::from_le_bytes([c[0], c[1]]))
        .collect();
    Some(VoiceEvent::AudioChunk { pcm })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn audio_chunk_round_trip() {
        let pcm: Vec<i16> = vec![0, 100, -100, 32767, -32768];
        let bytes: Vec<u8> = pcm.iter().flat_map(|s| s.to_le_bytes()).collect();
        let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
        let evt = decode_audio_chunk(&b64).unwrap();
        match evt {
            VoiceEvent::AudioChunk { pcm: out } => assert_eq!(out, pcm),
            _ => panic!("wrong variant"),
        }
    }

    #[tokio::test]
    async fn dispatch_stt_final_emits_event() {
        let (tx, mut rx) = mpsc::channel(4);
        let frame = json!({
            "jsonrpc": "2.0",
            "method": "notifications/progress",
            "params": {"kind": "stt_final", "text": "hello"},
        });
        let done = dispatch_event(&frame, &tx).await;
        assert!(!done);
        match rx.recv().await.unwrap() {
            VoiceEvent::SttFinal { text } => assert_eq!(text, "hello"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn dispatch_final_result_signals_done() {
        let (tx, mut rx) = mpsc::channel(4);
        let frame = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {"transcript": "hi", "assistant_text": "hello there"},
        });
        let done = dispatch_event(&frame, &tx).await;
        assert!(done);
        match rx.recv().await.unwrap() {
            VoiceEvent::Done {
                transcript,
                assistant_text,
            } => {
                assert_eq!(transcript, "hi");
                assert_eq!(assistant_text, "hello there");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }
}
