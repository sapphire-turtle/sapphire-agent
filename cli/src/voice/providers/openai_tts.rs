//! OpenAI Audio Speech (`POST /v1/audio/speech`) TTS client.
//!
//! Works against OpenAI's public endpoint and any self-hosted server
//! that speaks the same request shape (e.g. the user's Irodori-TTS
//! fork at `https://irodori-tts-api.home.fireturtle.net`). The
//! request body is the standard `{model, input, voice,
//! response_format}` JSON; we always ask for `wav` so the shared
//! `wav_stream` helper can decode + resample to the pipeline rate.
//!
//! Auth header is conditional: `api_key_env` is optional, so
//! self-hosted endpoints without authentication can be used as-is.

use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::mpsc;
use tracing::debug;

use crate::voice::providers::wav_stream;
use crate::voice::{PIPELINE_SAMPLE_RATE, TtsProvider};

const SYNTHESIZE_TIMEOUT: Duration = Duration::from_secs(120);
const DEFAULT_BASE_URL: &str = "https://api.openai.com";
const DEFAULT_MODEL: &str = "tts-1";
const DEFAULT_VOICE: &str = "alloy";

pub(crate) struct OpenAiTts {
    name: String,
    base_url: String,
    model: String,
    voice: String,
    /// Pre-resolved Authorization header value, if any. Resolved at
    /// construction time so a missing env var fails fast at startup
    /// rather than on the first synthesize call.
    auth_header: Option<String>,
    client: reqwest::Client,
}

impl OpenAiTts {
    pub(crate) fn new(
        name: String,
        api_key_env: Option<&str>,
        base_url: Option<&str>,
        model: Option<&str>,
        voice: Option<&str>,
    ) -> anyhow::Result<Self> {
        let auth_header = match api_key_env {
            Some(var) => match std::env::var(var) {
                Ok(k) if !k.is_empty() => Some(format!("Bearer {k}")),
                Ok(_) => anyhow::bail!("tts_provider '{name}': env var '{var}' is set but empty"),
                Err(_) => anyhow::bail!("tts_provider '{name}': env var '{var}' is not set"),
            },
            None => None,
        };
        let base_url = base_url
            .unwrap_or(DEFAULT_BASE_URL)
            .trim_end_matches('/')
            .to_string();
        let client = reqwest::Client::builder()
            .timeout(SYNTHESIZE_TIMEOUT)
            .build()?;
        Ok(Self {
            name,
            base_url,
            model: model.unwrap_or(DEFAULT_MODEL).to_string(),
            voice: voice.unwrap_or(DEFAULT_VOICE).to_string(),
            auth_header,
            client,
        })
    }
}

#[async_trait]
impl TtsProvider for OpenAiTts {
    fn name(&self) -> &str {
        &self.name
    }

    fn sample_rate(&self) -> u32 {
        // We resample on the way out, so the pipeline always sees
        // 16 kHz regardless of OpenAI's native rate (24 kHz for WAV).
        PIPELINE_SAMPLE_RATE
    }

    async fn synthesize_stream(
        &self,
        text: &str,
        pcm_tx: mpsc::Sender<Vec<i16>>,
    ) -> anyhow::Result<()> {
        let url = format!("{}/v1/audio/speech", self.base_url);
        let body = serde_json::json!({
            "model": self.model,
            "input": text,
            "voice": self.voice,
            "response_format": "wav",
        });
        debug!(
            "openai_tts: POST {url} (model={}, voice={}, chars={})",
            self.model,
            self.voice,
            text.len()
        );

        let mut req = self.client.post(&url).json(&body);
        if let Some(h) = &self.auth_header {
            req = req.header(reqwest::header::AUTHORIZATION, h);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("openai_tts POST {url} failed: {e}"))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("openai_tts returned {status}: {body}");
        }
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| anyhow::anyhow!("openai_tts body read: {e}"))?
            .to_vec();

        wav_stream::stream_wav(bytes, pcm_tx).await
    }
}
