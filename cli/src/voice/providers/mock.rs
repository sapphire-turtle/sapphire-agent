//! Deterministic mock STT/TTS providers.
//!
//! Useful for end-to-end pipeline tests and for exercising the
//! `voice/pipeline_run` JSON-RPC method without setting up a real
//! model or external service. Selected via TOML config:
//!
//! ```toml
//! [stt_provider.test]
//! type = "mock"
//! transcript = "hello world"
//!
//! [tts_provider.test]
//! type = "mock"
//! duration_ms = 200
//! frequency_hz = 440
//! ```

use async_trait::async_trait;
use tokio::sync::mpsc;

use crate::voice::{PIPELINE_SAMPLE_RATE, SttProvider, TtsProvider};

pub(crate) struct MockStt {
    name: String,
    transcript: String,
}

impl MockStt {
    pub(crate) fn new(name: String, transcript: String) -> Self {
        Self { name, transcript }
    }
}

#[async_trait]
impl SttProvider for MockStt {
    fn name(&self) -> &str {
        &self.name
    }

    fn sample_rate(&self) -> u32 {
        PIPELINE_SAMPLE_RATE
    }

    async fn transcribe(&self, _pcm: &[i16], _language: Option<&str>) -> anyhow::Result<String> {
        Ok(self.transcript.clone())
    }
}

pub(crate) struct MockTts {
    name: String,
    duration_ms: u32,
    frequency_hz: u32,
}

impl MockTts {
    pub(crate) fn new(name: String, duration_ms: u32, frequency_hz: u32) -> Self {
        Self {
            name,
            duration_ms,
            frequency_hz,
        }
    }
}

#[async_trait]
impl TtsProvider for MockTts {
    fn name(&self) -> &str {
        &self.name
    }

    fn sample_rate(&self) -> u32 {
        PIPELINE_SAMPLE_RATE
    }

    async fn synthesize_stream(
        &self,
        _text: &str,
        pcm_tx: mpsc::Sender<Vec<i16>>,
    ) -> anyhow::Result<()> {
        // Generate a sine wave at `frequency_hz` lasting `duration_ms`.
        // Chunked at ~20ms (320 samples @ 16kHz) to exercise the
        // streaming code path.
        let total_samples = (PIPELINE_SAMPLE_RATE as u64 * self.duration_ms as u64 / 1000) as usize;
        let chunk_size = PIPELINE_SAMPLE_RATE as usize / 50; // 20ms
        let two_pi = std::f64::consts::TAU;
        let freq = self.frequency_hz as f64;
        let sr = PIPELINE_SAMPLE_RATE as f64;
        let amplitude = i16::MAX as f64 * 0.3;

        let mut sample_idx = 0;
        while sample_idx < total_samples {
            let end = (sample_idx + chunk_size).min(total_samples);
            let chunk: Vec<i16> = (sample_idx..end)
                .map(|i| {
                    let t = i as f64 / sr;
                    (amplitude * (two_pi * freq * t).sin()) as i16
                })
                .collect();
            sample_idx = end;
            if pcm_tx.send(chunk).await.is_err() {
                // Receiver dropped — caller cancelled. Not an error.
                return Ok(());
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mock_stt_returns_configured_transcript() {
        let stt = MockStt::new("test".into(), "hello".into());
        let pcm = vec![0i16; 1600];
        let t = stt.transcribe(&pcm, None).await.unwrap();
        assert_eq!(t, "hello");
    }

    #[tokio::test]
    async fn mock_tts_streams_expected_sample_count() {
        let tts = MockTts::new("test".into(), 100, 440);
        let (tx, mut rx) = mpsc::channel(64);
        let handle = tokio::spawn(async move {
            tts.synthesize_stream("ignored", tx).await.unwrap();
        });
        let mut total = 0usize;
        while let Some(chunk) = rx.recv().await {
            total += chunk.len();
        }
        handle.await.unwrap();
        // 100ms @ 16kHz = 1600 samples
        assert_eq!(total, 1600);
    }
}
