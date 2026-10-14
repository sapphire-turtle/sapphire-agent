//! Speech-to-text provider abstraction.

use async_trait::async_trait;

/// One-shot speech-to-text. v1 is buffer-based: the caller hands over a
/// complete utterance (PTT-bounded or VAD-bounded on the client) and gets
/// the final transcript back as a single string.
///
/// Streaming transcription (partial hypotheses while the user is still
/// speaking) is intentionally out of scope for v1 — none of our planned
/// initial providers (whisper-rs, OpenAI Whisper API) expose a true
/// streaming surface that would matter at the round-trip latencies we
/// already accept for sub-10-second utterances.
#[async_trait]
pub trait SttProvider: Send + Sync {
    /// Provider name as recorded in config (e.g. `"whisper_local"`).
    fn name(&self) -> &str;

    /// PCM sample rate the provider expects, in Hz. Pipeline contract
    /// pins this at [`super::PIPELINE_SAMPLE_RATE`] (16 kHz); providers
    /// that need a different rate should resample internally rather
    /// than imposing a different rate on the pipeline.
    #[allow(dead_code)]
    fn sample_rate(&self) -> u32;

    /// Transcribe a single utterance. `pcm` is mono s16le at the
    /// provider's `sample_rate()`. `language` is an optional BCP-47
    /// hint; providers may ignore it (e.g. multilingual whisper with
    /// auto-detect).
    async fn transcribe(&self, pcm: &[i16], language: Option<&str>) -> anyhow::Result<String>;
}
