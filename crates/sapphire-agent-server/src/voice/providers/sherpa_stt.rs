//! Local STT via the official sherpa-onnx Rust crate.
//!
//! Currently supports SenseVoice (recommended default — multilingual,
//! handles Japanese well) and Whisper. Adding more model families is
//! a matter of extending the match in [`SherpaOnnxStt::new`] and
//! introducing the relevant bundle-layout helpers.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use sherpa_onnx::{
    OfflineRecognizer, OfflineRecognizerConfig, OfflineSenseVoiceModelConfig,
    OfflineWhisperModelConfig,
};

use crate::config::{SherpaSttConfig, SherpaSttKind};
use crate::voice::providers::sherpa_download::{
    self, BundleCategory, ensure_bundle, path_string, pick_file,
};
use crate::voice::{PIPELINE_SAMPLE_RATE, SttProvider};

/// Recognizer is created once at startup; subsequent transcription
/// calls reuse it across utterances via short-lived streams.
pub(crate) struct SherpaOnnxStt {
    name: String,
    recognizer: Arc<OfflineRecognizer>,
    /// Language hint baked at construction time. Per-call language
    /// overrides supersede this when present.
    language: Option<String>,
    kind: SherpaSttKind,
}

// SAFETY: OfflineRecognizer's underlying C++ object is thread-safe for
// concurrent stream creation; the sherpa-onnx Rust wrapper does not
// implement Send/Sync explicitly because of the raw pointer. We wrap
// it in Arc and never expose the inner pointer.
unsafe impl Send for SherpaOnnxStt {}
unsafe impl Sync for SherpaOnnxStt {}

impl SherpaOnnxStt {
    pub(crate) fn new(name: String, cfg: SherpaSttConfig) -> anyhow::Result<Self> {
        let dir = ensure_bundle(
            cfg.model_dir.as_deref(),
            cfg.model.as_deref(),
            BundleCategory::Asr,
        )?;

        let recognizer = build_recognizer(&dir, &cfg)
            .map_err(|e| anyhow::anyhow!("stt_provider '{name}': {e:#}"))?;

        Ok(Self {
            name,
            recognizer: Arc::new(recognizer),
            language: cfg.language.clone(),
            kind: cfg.kind,
        })
    }
}

fn build_recognizer(dir: &Path, cfg: &SherpaSttConfig) -> anyhow::Result<OfflineRecognizer> {
    let mut rec_cfg = OfflineRecognizerConfig::default();
    rec_cfg.model_config.num_threads = cfg.num_threads;
    rec_cfg.model_config.provider = Some(cfg.provider.clone());

    match cfg.kind {
        SherpaSttKind::SenseVoice => {
            let model = pick_file(dir, &["model.int8.onnx", "model.onnx"])?;
            let tokens = pick_file(dir, &["tokens.txt"])?;
            rec_cfg.model_config.sense_voice = OfflineSenseVoiceModelConfig {
                model: Some(path_string(&model)),
                language: cfg.language.clone().or_else(|| Some("auto".into())),
                use_itn: true,
            };
            rec_cfg.model_config.tokens = Some(path_string(&tokens));
        }
        SherpaSttKind::Whisper => {
            // Whisper bundles in sherpa-onnx ship as
            // `<size>-encoder[.int8].onnx`, `<size>-decoder[.int8].onnx`,
            // and `<size>-tokens.txt`. Find them by suffix.
            let encoder = find_by_suffix(dir, "-encoder.int8.onnx")
                .or_else(|_| find_by_suffix(dir, "-encoder.onnx"))?;
            let decoder = find_by_suffix(dir, "-decoder.int8.onnx")
                .or_else(|_| find_by_suffix(dir, "-decoder.onnx"))?;
            let tokens = find_by_suffix(dir, "-tokens.txt")?;
            rec_cfg.model_config.whisper = OfflineWhisperModelConfig {
                encoder: Some(path_string(&encoder)),
                decoder: Some(path_string(&decoder)),
                language: cfg.language.clone(),
                task: Some("transcribe".into()),
                tail_paddings: 0,
                enable_token_timestamps: false,
                enable_segment_timestamps: false,
            };
            rec_cfg.model_config.tokens = Some(path_string(&tokens));
        }
    }

    OfflineRecognizer::create(&rec_cfg)
        .ok_or_else(|| anyhow::anyhow!("OfflineRecognizer::create failed"))
}

fn find_by_suffix(dir: &Path, suffix: &str) -> anyhow::Result<std::path::PathBuf> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        if name.to_string_lossy().ends_with(suffix) {
            return Ok(entry.path());
        }
    }
    anyhow::bail!("no file ending with '{suffix}' in {}", dir.display())
}

#[async_trait]
impl SttProvider for SherpaOnnxStt {
    fn name(&self) -> &str {
        &self.name
    }

    fn sample_rate(&self) -> u32 {
        PIPELINE_SAMPLE_RATE
    }

    async fn transcribe(&self, pcm: &[i16], language: Option<&str>) -> anyhow::Result<String> {
        // Convert i16 → normalized f32 for sherpa-onnx.
        let samples: Vec<f32> = pcm.iter().map(|s| *s as f32 / 32768.0).collect();
        let recognizer = Arc::clone(&self.recognizer);
        let configured_lang = self.language.clone();
        let kind = self.kind;
        let override_lang = language.map(String::from);

        // sherpa-onnx is synchronous and CPU-bound — keep it off the
        // tokio worker pool.
        tokio::task::spawn_blocking(move || -> anyhow::Result<String> {
            let stream = recognizer.create_stream();
            // Per-call language override. SenseVoice is the only family
            // that exposes runtime language via stream options; Whisper
            // is baked into the recognizer config at startup.
            if matches!(kind, SherpaSttKind::SenseVoice) {
                let lang = override_lang
                    .as_deref()
                    .or(configured_lang.as_deref())
                    .unwrap_or("auto");
                stream.set_option("language", lang);
            }
            stream.accept_waveform(PIPELINE_SAMPLE_RATE as i32, &samples);
            recognizer.decode(&stream);
            let result = stream
                .get_result()
                .ok_or_else(|| anyhow::anyhow!("recognizer returned no result"))?;
            Ok(result.text.trim().to_string())
        })
        .await
        .map_err(|e| anyhow::anyhow!("sherpa STT blocking task panicked: {e}"))?
    }
}

// Re-export the cache_dir helper for test diagnostics elsewhere.
#[allow(unused_imports)]
pub(crate) use sherpa_download::cache_dir;
