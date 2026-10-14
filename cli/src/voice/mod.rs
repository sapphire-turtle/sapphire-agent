//! Voice pipeline subsystem.
//!
//! Exposes STT / TTS provider abstractions and a registry that builds
//! concrete instances from `[stt_provider.*]` / `[tts_provider.*]` config
//! blocks. The `voice/pipeline_run` JSON-RPC method on `/rpc` (in
//! [`crate::serve`]) wires these into a per-request flow:
//!
//! ```text
//! base64 PCM ─► SttProvider::transcribe ─► transcript text
//!            ─► (same LLM turn-processing as the chat method) ─► reply text
//!            ─► TtsProvider::synthesize_stream ─► PCM chunks ─► SSE progress
//! ```
//!
//! Sample rate is fixed at 16 kHz mono s16le throughout the pipeline.
//! Providers expose their native rate via `sample_rate()` so callers can
//! validate at registration time; non-16kHz providers are not supported
//! in v1.

pub mod stt;
pub mod tts;

mod providers;

use std::collections::HashMap;
use std::sync::Arc;

use crate::config::{Config, SttProviderConfig, TtsProviderConfig};

// `providers` is a private module, so nothing outside `voice/` can name
// `providers::MockStt` directly. Re-exported here, one hop out, so
// `ambient::worker`'s tests can build a deterministic STT double without
// a real model. This only compiles because `providers/mod.rs` re-exports
// `MockStt` itself at `pub(crate)` rather than `pub(super)` — a `pub(X)
// use` can forward an item's existing visibility outward but never
// widen it, so the inner re-export has to already be crate-visible.
// `#[cfg(test)]`: this re-export's only consumer is `ambient::worker`'s
// test module, so a non-test build has no user for it at all — without
// the gate, `cargo check` flags it as an unused import.
#[cfg(test)]
pub(crate) use providers::MockStt;

pub use stt::SttProvider;
pub use tts::TtsProvider;

/// Fixed pipeline sample rate. Providers must produce/consume at this rate.
pub const PIPELINE_SAMPLE_RATE: u32 = 16_000;

/// Server-internal push event delivered to a `voice/subscribe` writer
/// task, which translates to wire-format SSE notifications. Mirrors the
/// public `VoicePushEvent` in `sapphire-agent-core` plus enough metadata
/// to reconstruct the heartbeat task name on the wire.
#[derive(Debug, Clone)]
pub enum VoicePushItem {
    /// Begin a new push (e.g. heartbeat fire).
    Start { task: Option<String> },
    /// Assistant text reply (echo of synthesized audio, for transcript).
    AssistantText(String),
    /// One synthesized audio chunk at [`PIPELINE_SAMPLE_RATE`].
    AudioChunk(Vec<i16>),
    /// Push complete; satellite should drain playback and enter
    /// follow-up listen.
    Done,
    /// Push failed.
    Error(String),
}

/// Holds every voice provider instantiated from config, keyed by the
/// user-chosen name (e.g. `"whisper_local"`, `"irodori"`).
pub struct VoiceProviders {
    stt: HashMap<String, Arc<dyn SttProvider>>,
    tts: HashMap<String, Arc<dyn TtsProvider>>,
}

impl VoiceProviders {
    /// Build the registry from a fully-validated `Config`. Returns an
    /// error if any provider fails to initialise (e.g. whisper model
    /// not found on disk).
    pub fn from_config(config: &Config) -> anyhow::Result<Self> {
        let mut stt: HashMap<String, Arc<dyn SttProvider>> = HashMap::new();
        for (name, cfg) in &config.stt_providers {
            let provider = build_stt(name, cfg)?;
            stt.insert(name.clone(), provider);
        }
        let mut tts: HashMap<String, Arc<dyn TtsProvider>> = HashMap::new();
        for (name, cfg) in &config.tts_providers {
            let provider = build_tts(name, cfg)?;
            tts.insert(name.clone(), provider);
        }
        Ok(Self { stt, tts })
    }

    pub fn stt(&self, name: &str) -> Option<Arc<dyn SttProvider>> {
        self.stt.get(name).cloned()
    }

    pub fn tts(&self, name: &str) -> Option<Arc<dyn TtsProvider>> {
        self.tts.get(name).cloned()
    }
}

fn build_stt(name: &str, cfg: &SttProviderConfig) -> anyhow::Result<Arc<dyn SttProvider>> {
    match cfg {
        SttProviderConfig::Mock { transcript } => Ok(Arc::new(providers::MockStt::new(
            name.to_string(),
            transcript.clone(),
        ))),
        SttProviderConfig::SherpaOnnx(cfg) => {
            #[cfg(feature = "voice-sherpa")]
            {
                Ok(Arc::new(providers::SherpaOnnxStt::new(
                    name.to_string(),
                    cfg.clone(),
                )?))
            }
            #[cfg(not(feature = "voice-sherpa"))]
            {
                let _ = cfg;
                anyhow::bail!(
                    "stt_provider '{name}': type = \"sherpa_onnx\" requires the \
                     `voice-sherpa` cargo feature to be enabled at build time"
                )
            }
        }
        SttProviderConfig::OpenAiWhisperApi { .. } => {
            anyhow::bail!(
                "stt_provider '{name}': type = \"openai_whisper_api\" is not yet implemented"
            )
        }
    }
}

fn build_tts(name: &str, cfg: &TtsProviderConfig) -> anyhow::Result<Arc<dyn TtsProvider>> {
    match cfg {
        TtsProviderConfig::Mock {
            duration_ms,
            frequency_hz,
        } => Ok(Arc::new(providers::MockTts::new(
            name.to_string(),
            *duration_ms,
            *frequency_hz,
        ))),
        TtsProviderConfig::OpenAiTts {
            api_key_env,
            base_url,
            model,
            voice,
        } => Ok(Arc::new(providers::OpenAiTts::new(
            name.to_string(),
            api_key_env.as_deref(),
            base_url.as_deref(),
            model.as_deref(),
            voice.as_deref(),
        )?)),
        TtsProviderConfig::SherpaOnnx(cfg) => {
            #[cfg(feature = "voice-sherpa")]
            {
                Ok(Arc::new(providers::SherpaOnnxTts::new(
                    name.to_string(),
                    cfg.clone(),
                )?))
            }
            #[cfg(not(feature = "voice-sherpa"))]
            {
                let _ = cfg;
                anyhow::bail!(
                    "tts_provider '{name}': type = \"sherpa_onnx\" requires the \
                     `voice-sherpa` cargo feature to be enabled at build time"
                )
            }
        }
    }
}
