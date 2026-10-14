//! Model bundle auto-download for sherpa-onnx providers.
//!
//! sherpa-onnx ships pre-trained ASR/TTS/KWS/VAD models on GitHub as
//! tar.bz2 archives under
//! `https://github.com/k2-fsa/sherpa-onnx/releases/download/<category>/<bundle>.tar.bz2`.
//! When a config block references a bundle by name (e.g.
//! `model = "sherpa-onnx-sense-voice-zh-en-ja-ko-yue-2024-07-17"`) and
//! the corresponding directory is missing from the local cache, we
//! download and extract it.
//!
//! Bundles live under
//! `~/.local/share/sapphire-agent/voice-models/<bundle-name>/`. The
//! caller resolves model file paths inside that directory based on the
//! model family's expected layout.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use bzip2::read::BzDecoder;
use tracing::info;

/// Where bundles are extracted. Override via the `SAPPHIRE_VOICE_CACHE_DIR`
/// env var (useful in tests / containerised deployments).
pub fn cache_dir() -> PathBuf {
    if let Ok(custom) = std::env::var("SAPPHIRE_VOICE_CACHE_DIR") {
        return PathBuf::from(shellexpand::tilde(&custom).into_owned());
    }
    if let Some(dirs) = directories::ProjectDirs::from("", "", "sapphire-agent") {
        return dirs.data_local_dir().join("voice-models");
    }
    PathBuf::from(".sapphire-agent/voice-models")
}

/// Categories on the sherpa-onnx releases page. Each ASR/TTS/KWS/VAD
/// bundle is published under exactly one of these tags.
#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
pub enum BundleCategory {
    Asr,
    Tts,
    Kws,
    Vad,
}

impl BundleCategory {
    fn tag(&self) -> &'static str {
        match self {
            BundleCategory::Asr => "asr-models",
            BundleCategory::Tts => "tts-models",
            BundleCategory::Kws => "kws-models",
            BundleCategory::Vad => "vad-models",
        }
    }
}

/// Resolve either an explicit `model_dir` or a bundle `model` name to a
/// concrete extracted directory on disk. When the directory is missing
/// and `model` is a bundle name, the bundle is downloaded from
/// `category`'s GitHub releases tag and extracted to the cache dir.
pub fn ensure_bundle(
    model_dir: Option<&str>,
    bundle: Option<&str>,
    category: BundleCategory,
) -> anyhow::Result<PathBuf> {
    if let Some(dir) = model_dir {
        let p = PathBuf::from(shellexpand::tilde(dir).into_owned());
        if !p.exists() {
            anyhow::bail!("model_dir does not exist: {}", p.display());
        }
        return Ok(p);
    }
    let name = bundle.ok_or_else(|| {
        anyhow::anyhow!("either `model_dir` or `model` (bundle name) must be set")
    })?;
    if name.contains('/') || name.contains('~') || name.starts_with('.') {
        anyhow::bail!("`model` looks like a path ('{name}'); use `model_dir` for explicit paths");
    }
    let dest = cache_dir().join(name);
    if dest.exists() {
        return Ok(dest);
    }
    download_and_extract(name, category, &dest)?;
    Ok(dest)
}

fn download_and_extract(bundle: &str, category: BundleCategory, dest: &Path) -> anyhow::Result<()> {
    let url = format!(
        "https://github.com/k2-fsa/sherpa-onnx/releases/download/{}/{}.tar.bz2",
        category.tag(),
        bundle
    );
    let cache = cache_dir();
    fs::create_dir_all(&cache)
        .map_err(|e| anyhow::anyhow!("failed to create cache dir '{}': {e}", cache.display()))?;

    info!(
        "Downloading sherpa-onnx bundle '{bundle}' from {url} (one-time, may take several minutes)"
    );
    let resp = reqwest::blocking::Client::builder()
        .timeout(None)
        .build()?
        .get(&url)
        .send()?
        .error_for_status()?;
    let bytes = resp.bytes()?;
    let decompressor = BzDecoder::new(io::Cursor::new(bytes));
    let mut archive = tar::Archive::new(decompressor);

    // Extract under a sibling .partial dir then rename so an interrupted
    // unpack doesn't leave a half-populated directory that looks valid.
    let partial = dest.with_extension("partial");
    if partial.exists() {
        fs::remove_dir_all(&partial).map_err(|e| {
            anyhow::anyhow!(
                "failed to clear stale partial dir '{}': {e}",
                partial.display()
            )
        })?;
    }
    fs::create_dir_all(&partial)?;

    // Tar archives typically contain a single top-level directory named
    // after the bundle; strip it so files land directly under our dest.
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?;
        let stripped: PathBuf = path
            .components()
            .skip(1) // drop the leading bundle-name directory
            .collect();
        if stripped.as_os_str().is_empty() {
            continue;
        }
        let out_path = partial.join(stripped);
        if let Some(parent) = out_path.parent() {
            fs::create_dir_all(parent)?;
        }
        entry.unpack(&out_path)?;
    }

    fs::rename(&partial, dest).map_err(|e| {
        anyhow::anyhow!(
            "failed to rename '{}' → '{}': {e}",
            partial.display(),
            dest.display()
        )
    })?;
    info!("sherpa-onnx bundle ready at {}", dest.display());
    Ok(())
}

/// Find a single file in `dir` matching one of `candidates`, in order.
/// Returns the first match as an absolute path. Used to pick e.g.
/// `model.int8.onnx` if available, falling back to `model.onnx`.
pub fn pick_file(dir: &Path, candidates: &[&str]) -> anyhow::Result<PathBuf> {
    for c in candidates {
        let p = dir.join(c);
        if p.exists() {
            return Ok(p);
        }
    }
    anyhow::bail!(
        "no expected file found in {} (looked for: {})",
        dir.display(),
        candidates.join(", ")
    )
}

/// Return the path as a string suitable for sherpa-onnx config fields.
pub fn path_string(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}
