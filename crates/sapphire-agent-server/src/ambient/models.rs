//! Resolves the ambient subsystem's two ONNX models (Silero VAD re-gate,
//! speaker embedding) from config, and derives the cache-key model id from
//! the embedding model that was actually loaded.
//!
//! The model id matters more than it looks: reference-audio embeddings are
//! cached outside the workspace keyed by (file sha256 x model id), so that
//! swapping the embedding model recomputes every cached vector rather than
//! silently reusing vectors computed by a different model. See
//! `docs/superpowers/specs/2026-08-26-ambient-audio-ingest-design.md`.
//!
//! Both models are configured as an explicit directory only, not a
//! downloadable bundle name: unlike sherpa-onnx's ASR/TTS/KWS families
//! (which ship as tar.bz2 bundles resolved by
//! `voice::providers::sherpa_download::ensure_bundle`), Silero VAD and the
//! speaker-embedding models have only ever shipped as bare `.onnx` files —
//! there is nothing for a bundle-name field to auto-download.

use std::fs;
use std::path::{Path, PathBuf};

use crate::config::AmbientConfig;
use crate::image_cache::sha256_hex;

/// Everything the ambient worker needs to re-gate and attribute speech.
pub struct ResolvedModels {
    pub gate: Box<dyn crate::ambient::audio::SpeechGate>,
    pub embedder: Box<dyn crate::ambient::audio::SpeakerEmbedder>,
    /// Identifies the embedding model for the embedding cache key.
    pub model_id: String,
}

/// Resolve a configured model directory to one model file inside it — the
/// first of `candidates` that exists. Pure aside from filesystem reads — no
/// sherpa-onnx involved, so this is testable without the `voice-sherpa`
/// feature.
///
/// Outside tests, called only from `resolve`'s `voice-sherpa` branch below.
/// This function itself is *not* `#[cfg]`-gated, so a
/// `--no-default-features` build that omits `voice-sherpa` still compiles
/// it — with no caller, since that branch of `resolve` doesn't exist in
/// that build. Hence the `#[allow(dead_code)]`: still genuinely needed for
/// that one build configuration, not for a task that hasn't wired a
/// caller yet.
#[allow(dead_code)]
pub fn resolve_model_file(model_dir: Option<&str>, candidates: &[&str]) -> anyhow::Result<PathBuf> {
    let dir = model_dir.ok_or_else(|| anyhow::anyhow!("model_dir must be set"))?;
    let path = PathBuf::from(shellexpand::tilde(dir).into_owned());
    if !path.exists() {
        anyhow::bail!("model_dir does not exist: {}", path.display());
    }
    first_existing(&path, candidates)
}

/// Find the first of `candidates` that exists in `dir`.
///
/// Deliberately local rather than reused from
/// `voice::providers::sherpa_download::pick_file`, which does the same
/// thing: reaching that function would cost `sherpa_download` a permanent
/// feature-gate and visibility change (it currently lives behind
/// `voice-sherpa` and private to `voice`), whereas this is five lines.
fn first_existing(dir: &Path, candidates: &[&str]) -> anyhow::Result<PathBuf> {
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

/// A stable short id for a model file, for use as (part of) an embedding
/// cache key. Depends only on the file's contents, not its name or path,
/// so relocating a model directory never invalidates cached vectors.
///
/// Outside tests, called only from `resolve`'s `voice-sherpa` branch below;
/// see [`resolve_model_file`] for why `--no-default-features` (without
/// `voice-sherpa`) still needs the `#[allow(dead_code)]` here.
#[allow(dead_code)]
pub fn model_id_for(path: &Path) -> anyhow::Result<String> {
    let bytes = fs::read(path)
        .map_err(|e| anyhow::anyhow!("failed to read model file '{}': {e}", path.display()))?;
    let full = sha256_hex(&bytes);
    Ok(full[..16].to_string())
}

/// Called from `ambient::startup::build`, which is unreachable in this
/// exact form under `--no-default-features` (see the sibling `resolve`
/// below): only one of the two definitions compiles for a given feature
/// set, and `startup::build` calls whichever one is present.
#[cfg(feature = "voice-sherpa")]
pub fn resolve(cfg: &AmbientConfig) -> anyhow::Result<ResolvedModels> {
    use crate::ambient::audio::{SherpaEmbedder, SileroGate};

    let vad_path = resolve_model_file(
        cfg.vad_model_dir.as_deref(),
        &["silero_vad.onnx", "model.onnx"],
    )?;
    let embedding_path = resolve_model_file(
        cfg.embedding_model_dir.as_deref(),
        &["model.onnx", "model.int8.onnx"],
    )?;

    // Derived from the embedding model only: the VAD model never touches a
    // cached vector, so it must never perturb the cache key.
    let model_id = model_id_for(&embedding_path)?;

    let gate = SileroGate::new(vad_path.to_string_lossy().into_owned(), cfg.vad_threshold)?;
    let embedder = SherpaEmbedder::new(
        embedding_path.to_string_lossy().into_owned(),
        cfg.embedding_num_threads,
    )?;

    Ok(ResolvedModels {
        gate: Box::new(gate),
        embedder: Box::new(embedder),
        model_id,
    })
}

#[cfg(not(feature = "voice-sherpa"))]
pub fn resolve(_cfg: &AmbientConfig) -> anyhow::Result<ResolvedModels> {
    anyhow::bail!(
        "ambient capture needs the `voice-sherpa` cargo feature for speech gating and \
         speaker attribution; rebuild with it or set [ambient].enabled = false"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &std::path::Path, name: &str, bytes: &[u8]) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let p = dir.join(name);
        std::fs::write(&p, bytes).unwrap();
        p
    }

    #[test]
    fn resolve_model_file_finds_the_only_candidate_present() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "model.onnx", b"weights");
        let got = resolve_model_file(Some(tmp.path().to_str().unwrap()), &["model.onnx"]).unwrap();
        assert_eq!(got, tmp.path().join("model.onnx"));
    }

    #[test]
    fn resolve_model_file_takes_the_first_candidate_that_exists() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "model.int8.onnx", b"quantised");
        let got = resolve_model_file(
            Some(tmp.path().to_str().unwrap()),
            &["model.onnx", "model.int8.onnx"],
        )
        .unwrap();
        assert_eq!(got, tmp.path().join("model.int8.onnx"));
    }

    #[test]
    fn resolve_model_file_errors_naming_the_directory_when_nothing_matches() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path()).unwrap();
        let err = resolve_model_file(Some(tmp.path().to_str().unwrap()), &["model.onnx"])
            .unwrap_err()
            .to_string();
        assert!(
            err.contains(&tmp.path().display().to_string()),
            "error should name the directory searched: {err}"
        );
    }

    #[test]
    fn resolve_model_file_errors_when_dir_is_not_set() {
        assert!(resolve_model_file(None, &["model.onnx"]).is_err());
    }

    #[test]
    fn resolve_model_file_errors_naming_the_directory_when_it_does_not_exist() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("does-not-exist");
        let err = resolve_model_file(Some(missing.to_str().unwrap()), &["model.onnx"])
            .unwrap_err()
            .to_string();
        assert!(
            err.contains(&missing.display().to_string()),
            "error should name the missing directory: {err}"
        );
    }

    #[test]
    fn model_id_is_stable_for_identical_bytes_and_differs_for_different_bytes() {
        let tmp = tempfile::tempdir().unwrap();
        let a = write(tmp.path(), "a.onnx", b"weights-v1");
        let b = write(tmp.path(), "b.onnx", b"weights-v1");
        let c = write(tmp.path(), "c.onnx", b"weights-v2");

        let ida = model_id_for(&a).unwrap();
        assert_eq!(ida, model_id_for(&b).unwrap(), "same bytes, same id");
        assert_ne!(
            ida,
            model_id_for(&c).unwrap(),
            "different bytes, different id"
        );
        assert_eq!(ida.len(), 16, "short enough to read in a filename");
        assert!(ida.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn model_id_does_not_depend_on_the_file_name_or_path() {
        // The id keys an embedding cache. If it varied with the path, moving the
        // model directory would silently invalidate every cached vector.
        let tmp = tempfile::tempdir().unwrap();
        let a = write(&tmp.path().join("one"), "model.onnx", b"same");
        let b = write(&tmp.path().join("two"), "other-name.onnx", b"same");
        assert_eq!(model_id_for(&a).unwrap(), model_id_for(&b).unwrap());
    }
}
