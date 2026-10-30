//! Local TTS via the official sherpa-onnx Rust crate.
//!
//! Supports VITS, Matcha, and Kokoro at the moment. Each family has a
//! different on-disk layout that the bundle-discovery code below
//! resolves against the extracted directory.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use sherpa_onnx::{
    GenerationConfig, OfflineTts, OfflineTtsConfig, OfflineTtsKokoroModelConfig,
    OfflineTtsMatchaModelConfig, OfflineTtsModelConfig, OfflineTtsVitsModelConfig,
};
use tokio::sync::mpsc;

use crate::config::{SherpaTtsConfig, SherpaTtsKind};
use crate::voice::providers::sherpa_download::{
    BundleCategory, ensure_bundle, path_string, pick_file,
};
use crate::voice::{PIPELINE_SAMPLE_RATE, TtsProvider};

pub(crate) struct SherpaOnnxTts {
    name: String,
    tts: Arc<OfflineTts>,
    speaker_id: i32,
    speed: f32,
    /// Native sample rate of the TTS model. We resample to the
    /// pipeline rate (16 kHz) before emitting chunks.
    native_rate: u32,
}

// SAFETY: see SherpaOnnxStt — same reasoning, single Arc owner, no
// raw-pointer exposure.
unsafe impl Send for SherpaOnnxTts {}
unsafe impl Sync for SherpaOnnxTts {}

impl SherpaOnnxTts {
    pub(crate) fn new(name: String, cfg: SherpaTtsConfig) -> anyhow::Result<Self> {
        let dir = ensure_bundle(
            cfg.model_dir.as_deref(),
            cfg.model.as_deref(),
            BundleCategory::Tts,
        )?;
        let tts =
            build_tts(&dir, &cfg).map_err(|e| anyhow::anyhow!("tts_provider '{name}': {e:#}"))?;
        let native_rate = tts.sample_rate() as u32;
        Ok(Self {
            name,
            tts: Arc::new(tts),
            speaker_id: cfg.speaker_id,
            speed: cfg.speed,
            native_rate,
        })
    }
}

fn build_tts(dir: &Path, cfg: &SherpaTtsConfig) -> anyhow::Result<OfflineTts> {
    let mut model_config = OfflineTtsModelConfig {
        num_threads: cfg.num_threads,
        debug: false,
        provider: Some(cfg.provider.clone()),
        ..Default::default()
    };

    match cfg.kind {
        SherpaTtsKind::Vits => {
            let model = pick_onnx(dir)?;
            let tokens = optional(dir, "tokens.txt");
            let lexicon = optional(dir, "lexicon.txt");
            let data_dir = optional_dir(dir, "espeak-ng-data");
            let dict_dir = optional_dir(dir, "dict");
            model_config.vits = OfflineTtsVitsModelConfig {
                model: Some(path_string(&model)),
                tokens: tokens.as_deref().map(path_string),
                lexicon: lexicon.as_deref().map(path_string),
                data_dir: data_dir.as_deref().map(path_string),
                dict_dir: dict_dir.as_deref().map(path_string),
                noise_scale: 0.667,
                noise_scale_w: 0.8,
                length_scale: 1.0,
            };
        }
        SherpaTtsKind::Matcha => {
            // Matcha bundles ship the acoustic model under a name like
            // `model-steps-3.onnx`; the vocoder (often `vocos-*.onnx`)
            // sits alongside or in the cache root.
            let acoustic = find_by_substring(dir, "model-steps").or_else(|_| pick_onnx(dir))?;
            let vocoder =
                find_by_substring(dir, "vocos").or_else(|_| find_by_substring(dir, "vocoder"))?;
            let tokens = pick_file(dir, &["tokens.txt"])?;
            let lexicon = optional(dir, "lexicon.txt");
            let dict_dir = optional_dir(dir, "dict");
            model_config.matcha = OfflineTtsMatchaModelConfig {
                acoustic_model: Some(path_string(&acoustic)),
                vocoder: Some(path_string(&vocoder)),
                tokens: Some(path_string(&tokens)),
                lexicon: lexicon.as_deref().map(path_string),
                data_dir: None,
                dict_dir: dict_dir.as_deref().map(path_string),
                noise_scale: 0.667,
                length_scale: 1.0,
            };
        }
        SherpaTtsKind::Kokoro => {
            let model = pick_onnx(dir)?;
            let voices = pick_file(dir, &["voices.bin"])?;
            let tokens = pick_file(dir, &["tokens.txt"])?;
            let data_dir = optional_dir(dir, "espeak-ng-data");
            let lexicon = optional(dir, "lexicon.txt");
            let dict_dir = optional_dir(dir, "dict");
            model_config.kokoro = OfflineTtsKokoroModelConfig {
                model: Some(path_string(&model)),
                voices: Some(path_string(&voices)),
                tokens: Some(path_string(&tokens)),
                data_dir: data_dir.as_deref().map(path_string),
                lexicon: lexicon.as_deref().map(path_string),
                dict_dir: dict_dir.as_deref().map(path_string),
                length_scale: 1.0,
                lang: None,
            };
        }
    }

    let tts_cfg = OfflineTtsConfig {
        model: model_config,
        max_num_sentences: 2,
        silence_scale: 0.2,
        ..Default::default()
    };
    OfflineTts::create(&tts_cfg).ok_or_else(|| anyhow::anyhow!("OfflineTts::create failed"))
}

fn pick_onnx(dir: &Path) -> anyhow::Result<PathBuf> {
    // Prefer quantized variants when both ship.
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let s = name.to_string_lossy();
        if s.ends_with(".int8.onnx") {
            return Ok(entry.path());
        }
    }
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let s = name.to_string_lossy();
        if s.ends_with(".onnx") {
            return Ok(entry.path());
        }
    }
    anyhow::bail!("no .onnx file in {}", dir.display())
}

fn find_by_substring(dir: &Path, needle: &str) -> anyhow::Result<PathBuf> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let s = entry.file_name().to_string_lossy().into_owned();
        if s.contains(needle) && s.ends_with(".onnx") {
            return Ok(entry.path());
        }
    }
    anyhow::bail!("no .onnx file matching '{needle}' in {}", dir.display())
}

fn optional(dir: &Path, name: &str) -> Option<PathBuf> {
    let p = dir.join(name);
    if p.exists() { Some(p) } else { None }
}

fn optional_dir(dir: &Path, name: &str) -> Option<PathBuf> {
    let p = dir.join(name);
    if p.is_dir() { Some(p) } else { None }
}

#[async_trait]
impl TtsProvider for SherpaOnnxTts {
    fn name(&self) -> &str {
        &self.name
    }

    fn sample_rate(&self) -> u32 {
        // We resample on the way out, so the pipeline always sees 16 kHz.
        PIPELINE_SAMPLE_RATE
    }

    async fn synthesize_stream(
        &self,
        text: &str,
        pcm_tx: mpsc::Sender<Vec<i16>>,
    ) -> anyhow::Result<()> {
        let tts = Arc::clone(&self.tts);
        let speaker_id = self.speaker_id;
        let speed = self.speed;
        let native_rate = self.native_rate;
        let text = text.to_string();

        // sherpa-onnx synthesis is synchronous and CPU-bound.
        let (samples, model_rate) =
            tokio::task::spawn_blocking(move || -> anyhow::Result<(Vec<f32>, u32)> {
                let gen_cfg = GenerationConfig {
                    sid: speaker_id,
                    speed,
                    ..Default::default()
                };
                let audio = tts
                    .generate_with_config(&text, &gen_cfg, None::<fn(&[f32], f32) -> bool>)
                    .ok_or_else(|| anyhow::anyhow!("OfflineTts::generate returned None"))?;
                Ok((audio.samples().to_vec(), audio.sample_rate() as u32))
            })
            .await
            .map_err(|e| anyhow::anyhow!("sherpa TTS blocking task panicked: {e}"))??;

        let _ = native_rate; // covered by model_rate, kept for symmetry
        // f32 → i16 + resample to pipeline rate.
        let i16_samples: Vec<i16> = samples
            .iter()
            .map(|f| (f.clamp(-1.0, 1.0) * i16::MAX as f32) as i16)
            .collect();
        let resampled = resample_to_pipeline(&i16_samples, model_rate);

        // 20 ms chunks at the pipeline rate so the consumer can pace
        // playback uniformly.
        let chunk_size = (PIPELINE_SAMPLE_RATE as usize) / 50;
        for chunk in resampled.chunks(chunk_size) {
            if pcm_tx.send(chunk.to_vec()).await.is_err() {
                break; // consumer dropped — caller cancelled
            }
        }
        Ok(())
    }
}

/// Linear-interpolation resample to the fixed pipeline rate.
/// Quality is fine for speech.
fn resample_to_pipeline(input: &[i16], src_rate: u32) -> Vec<i16> {
    if input.is_empty() || src_rate == PIPELINE_SAMPLE_RATE {
        return input.to_vec();
    }
    let ratio = src_rate as f64 / PIPELINE_SAMPLE_RATE as f64;
    let out_len = ((input.len() as f64) / ratio).round() as usize;
    let mut out = Vec::with_capacity(out_len);
    for i in 0..out_len {
        let src_pos = i as f64 * ratio;
        let idx = src_pos.floor() as usize;
        let frac = src_pos - idx as f64;
        let s0 = input[idx.min(input.len() - 1)] as f64;
        let s1 = input[(idx + 1).min(input.len() - 1)] as f64;
        let v = s0 * (1.0 - frac) + s1 * frac;
        out.push(v.round().clamp(i16::MIN as f64, i16::MAX as f64) as i16);
    }
    out
}
