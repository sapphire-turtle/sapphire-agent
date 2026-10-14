//! VAD re-gating and speaker embedding, behind traits.
//!
//! Both real implementations need sherpa-onnx models, which makes them
//! useless in unit tests and slow to build. The traits let every later
//! task — the worker, the registry, promotion — be tested with cheap
//! doubles, exactly as `MockStt` already does for transcription.

use anyhow::Result;

// Every consumer of this constant in this file — `samples_to_ms` and the
// `sherpa_impl` module — is itself gated `#[cfg(any(test, feature =
// "voice-sherpa"))]` or narrower, so the import needs the same gate: a
// `--no-default-features` non-test build has no user for it at all.
#[cfg(any(test, feature = "voice-sherpa"))]
use crate::voice::PIPELINE_SAMPLE_RATE;

/// Speech surviving the re-gate.
#[derive(Debug, Clone, PartialEq)]
pub struct GatedSpeech {
    pub pcm: Vec<i16>,
    /// Duration of the **speech**, not of the submitted segment. This is
    /// the value compared against `min_embed_ms` and accumulated into a
    /// candidate's promotion total.
    pub speech_ms: u32,
}

/// Second-pass VAD. The capture device runs a cheap classical VAD tuned to
/// over-capture, because on-device the point of gating is to let the radio
/// sleep, not to be accurate. This trims what it sent.
pub trait SpeechGate: Send + Sync {
    /// Speech in `pcm`, or `None` when the segment holds none.
    fn gate(&self, pcm: &[i16]) -> Option<GatedSpeech>;
}

pub trait SpeakerEmbedder: Send + Sync {
    fn embed(&self, pcm: &[i16]) -> Result<Vec<f32>>;
}

/// Convert a sample count at [`PIPELINE_SAMPLE_RATE`] into milliseconds.
///
/// Two independent callers, neither of which exists in a
/// `--no-default-features` non-test build: [`PassthroughGate::gate`] below
/// (`#[cfg(test)]` — see its doc comment for why it is test-only, not a
/// production fallback) and `SileroGate::gate` under
/// `#[cfg(feature = "voice-sherpa")]`. The gate here has to cover both.
#[cfg(any(test, feature = "voice-sherpa"))]
pub fn samples_to_ms(samples: usize) -> u32 {
    ((samples as u64 * 1000) / PIPELINE_SAMPLE_RATE as u64) as u32
}

/// Keeps everything it is given. Test double only — despite the name, this
/// is **not** wired as a production fallback for a missing VAD model:
/// `ambient::models::resolve` treats an unset `vad_model_dir` as a hard
/// startup error, on purpose (a silently-degraded gate that keeps silence
/// and noise would corrupt speaker attribution quietly instead of failing
/// loudly). `#[cfg(test)]` reflects that this type has no production
/// caller, only `ambient::worker`'s tests.
#[cfg(test)]
pub struct PassthroughGate;

#[cfg(test)]
impl PassthroughGate {
    pub fn new() -> Self {
        Self
    }
}

#[cfg(test)]
impl Default for PassthroughGate {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
impl SpeechGate for PassthroughGate {
    fn gate(&self, pcm: &[i16]) -> Option<GatedSpeech> {
        if pcm.is_empty() {
            return None;
        }
        Some(GatedSpeech {
            speech_ms: samples_to_ms(pcm.len()),
            pcm: pcm.to_vec(),
        })
    }
}

/// Drops everything. Test double for "the re-gate found no speech".
#[cfg(test)]
pub struct SilentGate;

#[cfg(test)]
impl SpeechGate for SilentGate {
    fn gate(&self, _pcm: &[i16]) -> Option<GatedSpeech> {
        None
    }
}

/// Returns one fixed vector regardless of input. Test double.
#[cfg(test)]
pub struct FixedEmbedder {
    vector: Vec<f32>,
}

#[cfg(test)]
impl FixedEmbedder {
    pub fn new(vector: Vec<f32>) -> Self {
        Self { vector }
    }
}

#[cfg(test)]
impl SpeakerEmbedder for FixedEmbedder {
    fn embed(&self, _pcm: &[i16]) -> Result<Vec<f32>> {
        Ok(self.vector.clone())
    }
}

#[cfg(feature = "voice-sherpa")]
mod sherpa_impl {
    use super::*;
    use sherpa_onnx::{
        SileroVadModelConfig, SpeakerEmbeddingExtractor, SpeakerEmbeddingExtractorConfig,
        VadModelConfig, VoiceActivityDetector,
    };

    /// Silero VAD re-gate. Concatenates every detected speech run, so a
    /// segment with a pause in the middle yields one `GatedSpeech` with the
    /// silence removed.
    ///
    /// Holds the loaded model for the process lifetime — `gate` calls
    /// `reset()` rather than reloading, since a reconnect burst can be
    /// hundreds of segments and every one of `VoiceActivityDetector`'s
    /// methods takes `&self` (the C library keeps its state behind the
    /// pointer), so one instance can serve every call.
    pub struct SileroGate {
        vad: VoiceActivityDetector,
    }

    impl SileroGate {
        pub fn new(model_path: String, threshold: f32) -> anyhow::Result<Self> {
            let config = VadModelConfig {
                silero_vad: SileroVadModelConfig {
                    model: Some(model_path),
                    threshold,
                    ..Default::default()
                },
                sample_rate: PIPELINE_SAMPLE_RATE as i32,
                ..Default::default()
            };
            let vad = VoiceActivityDetector::create(&config, 30.0)
                .ok_or_else(|| anyhow::anyhow!("failed to load Silero VAD model"))?;
            Ok(Self { vad })
        }
    }

    impl SpeechGate for SileroGate {
        fn gate(&self, pcm: &[i16]) -> Option<GatedSpeech> {
            // Clear state left over from the previous segment; the
            // detector itself is reused, not rebuilt.
            self.vad.reset();

            let samples: Vec<f32> = pcm.iter().map(|s| *s as f32 / 32768.0).collect();
            let mut kept: Vec<i16> = Vec::new();
            for window in samples.chunks(512) {
                self.vad.accept_waveform(window);
                while let Some(seg) = self.vad.front() {
                    kept.extend(seg.samples().iter().map(|s| (s * 32768.0).round() as i16));
                    self.vad.pop();
                }
            }
            self.vad.flush();
            while let Some(seg) = self.vad.front() {
                kept.extend(seg.samples().iter().map(|s| (s * 32768.0).round() as i16));
                self.vad.pop();
            }
            if kept.is_empty() {
                return None;
            }
            Some(GatedSpeech {
                speech_ms: samples_to_ms(kept.len()),
                pcm: kept,
            })
        }
    }

    pub struct SherpaEmbedder {
        extractor: SpeakerEmbeddingExtractor,
    }

    impl SherpaEmbedder {
        pub fn new(model_path: String, num_threads: i32) -> anyhow::Result<Self> {
            let config = SpeakerEmbeddingExtractorConfig {
                model: Some(model_path),
                num_threads,
                ..Default::default()
            };
            let extractor = SpeakerEmbeddingExtractor::create(&config)
                .ok_or_else(|| anyhow::anyhow!("failed to load speaker embedding model"))?;
            Ok(Self { extractor })
        }
    }

    impl SpeakerEmbedder for SherpaEmbedder {
        fn embed(&self, pcm: &[i16]) -> anyhow::Result<Vec<f32>> {
            let stream = self
                .extractor
                .create_stream()
                .ok_or_else(|| anyhow::anyhow!("cannot create embedding stream"))?;
            let samples: Vec<f32> = pcm.iter().map(|s| *s as f32 / 32768.0).collect();
            stream.accept_waveform(PIPELINE_SAMPLE_RATE as i32, &samples);
            stream.input_finished();
            if !self.extractor.is_ready(&stream) {
                anyhow::bail!("not enough audio for a speaker embedding");
            }
            self.extractor
                .compute(&stream)
                .ok_or_else(|| anyhow::anyhow!("embedding computation failed"))
        }
    }
}

// `sherpa_impl` is a private module (no `pub` above); this re-export is
// what lets `ambient::models::resolve`'s `voice-sherpa` branch name
// `crate::ambient::audio::{SherpaEmbedder, SileroGate}` from outside it.
#[cfg(feature = "voice-sherpa")]
pub use sherpa_impl::{SherpaEmbedder, SileroGate};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passthrough_gate_reports_duration_from_sample_count() {
        let gate = PassthroughGate::new();
        // 24000 samples at 16 kHz is 1500 ms.
        let g = gate.gate(&vec![0; 24_000]).expect("speech present");
        assert_eq!(g.speech_ms, 1500);
        assert_eq!(g.pcm.len(), 24_000);
    }

    #[test]
    fn passthrough_gate_reports_nothing_for_an_empty_segment() {
        assert!(PassthroughGate::new().gate(&[]).is_none());
    }

    #[test]
    fn silent_gate_drops_everything() {
        // Models a re-gate that found no speech at all.
        assert!(SilentGate.gate(&vec![0; 24_000]).is_none());
    }

    #[test]
    fn fixed_embedder_returns_the_configured_vector() {
        let e = FixedEmbedder::new(vec![1.0, 0.0, 0.0]);
        assert_eq!(e.embed(&[0; 100]).unwrap(), vec![1.0, 0.0, 0.0]);
    }
}
