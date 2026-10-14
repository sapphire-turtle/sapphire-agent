//! Text-to-speech provider abstraction.

use async_trait::async_trait;
use tokio::sync::mpsc;

/// Streaming text-to-speech. Mirrors the streaming side of
/// [`crate::provider::Provider`]: the provider pushes PCM frames into
/// `pcm_tx` as they are synthesized so the caller can start playback
/// before the full utterance is finished. Returns when synthesis is
/// complete; closing `pcm_tx` is the caller's responsibility (dropping
/// the sender ends the consumer's stream cleanly).
#[async_trait]
pub trait TtsProvider: Send + Sync {
    /// Provider name as recorded in config (e.g. `"irodori"`).
    fn name(&self) -> &str;

    /// PCM sample rate the provider produces, in Hz. Should match
    /// [`super::PIPELINE_SAMPLE_RATE`] — providers that synthesize at
    /// a different rate should resample internally rather than
    /// imposing a different rate on the pipeline.
    #[allow(dead_code)]
    fn sample_rate(&self) -> u32;

    /// Synthesize `text` and push PCM (mono s16le) frames into `pcm_tx`
    /// as they are produced. Frame size is provider-defined; consumers
    /// must not assume any particular chunking.
    async fn synthesize_stream(
        &self,
        text: &str,
        pcm_tx: mpsc::Sender<Vec<i16>>,
    ) -> anyhow::Result<()>;
}
