//! Registered speakers, loaded from workspace reference audio.
//!
//! `voices/<name>/*.wav`. The directory name is the speaker's **display
//! name**. Its **id** — the value transcripts store — comes from the
//! optional [`ID_MARKER`] file inside the directory, falling back to the
//! directory name when there is none.
//!
//! That split is what makes the spec's rename transparency real. If the
//! directory name *were* the id, renaming `voices/blithe-otter-42/` to
//! `voices/tanaka-san/` would **change** the id: afterwards one person has
//! two unrelated ids, nothing links them, and every transcript written
//! before the rename is unreachable under the new name. `speaker_promote`
//! with a name is worse still — it would fork the identity mid-run, with
//! the live registry and every past transcript saying `blithe-otter-42`
//! while `voices/tanaka-san/` exists alongside it.
//!
//! So promotion records the candidate's grain-id in the marker, and the
//! directory name is free to change. A directory the user created by hand
//! (`me/`, `agent/`) has no marker and keeps using its name as its id,
//! which is correct: nothing else ever referred to it.
//!
//! Embeddings are cached outside the workspace, keyed by (reference file
//! sha256 x model id), so renaming a directory triggers no recomputation
//! and swapping the embedding model recomputes automatically. No
//! model-dependent data ever lands in the workspace, which was the
//! requirement that shaped this split.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tracing::warn;

use crate::ambient::audio::SpeakerEmbedder;
use crate::image_cache::sha256_hex;

#[derive(Debug, Clone, PartialEq)]
pub struct SpeakerMatch {
    pub id: String,
    pub score: f32,
}

/// Name of the id marker file inside a `voices/<name>/` directory.
///
/// Newline-separated speaker ids. The **first** line is canonical — the id
/// new speech from this voice is attributed to. Any further lines are
/// aliases: ids that used to be separate and were merged into this speaker
/// (see [`super::candidates::CandidateStore::promote`]). All of them
/// resolve to the directory's current name.
pub const ID_MARKER: &str = "id";

/// Ids recorded for one speaker directory, canonical first.
///
/// Empty when the directory has no marker — the caller then falls back to
/// the directory name, which is what a hand-made `voices/me/` relies on.
pub fn speaker_ids_in(dir: &Path) -> Vec<String> {
    let Ok(body) = std::fs::read_to_string(dir.join(ID_MARKER)) else {
        return Vec::new();
    };
    body.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

/// Speaker id -> the directory name it currently lives under.
///
/// Deliberately a scan rather than a startup-time snapshot: the whole
/// point of the marker file is that the user may rename a directory at any
/// moment, and a transcript read must reflect the name as it is *now*. The
/// scan is a handful of `read_dir` entries and one small file each.
#[derive(Debug, Clone, Default)]
pub struct SpeakerNames {
    by_id: HashMap<String, String>,
}

impl SpeakerNames {
    pub fn scan(voices_dir: &Path) -> Self {
        let mut by_id = HashMap::new();
        let Ok(entries) = std::fs::read_dir(voices_dir) else {
            return Self { by_id };
        };
        for entry in entries.flatten() {
            if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            // A directory always answers to its own name, marker or not.
            by_id.insert(name.clone(), name.clone());
            for id in speaker_ids_in(&entry.path()) {
                by_id.insert(id, name.clone());
            }
        }
        Self { by_id }
    }

    /// Display name for `id`, or `id` itself when nothing claims it — an
    /// unpromoted candidate has no directory and is shown by its grain-id.
    pub fn display_name(&self, id: &str) -> String {
        self.by_id
            .get(id)
            .cloned()
            .unwrap_or_else(|| id.to_string())
    }

    /// Every id belonging to the speaker `who` names — `who` being either a
    /// display name or any one of that speaker's ids.
    ///
    /// Resolving through [`Self::display_name`] first is what makes both
    /// spellings work, and what makes a filter given a *pre-rename* id
    /// still return everything that person said: the id maps to the
    /// directory, and the directory maps back to all of its ids.
    ///
    /// Falls back to `[who]` when nothing claims it, so filtering by the
    /// grain-id of a candidate that was never promoted still works.
    pub fn ids_for(&self, who: &str) -> Vec<String> {
        let dir = self.display_name(who);
        let matched: Vec<String> = self
            .by_id
            .iter()
            .filter(|(_, name)| *name == &dir)
            .map(|(id, _)| id.clone())
            .collect();
        if matched.is_empty() {
            vec![who.to_string()]
        } else {
            matched
        }
    }
}

pub struct SpeakerRegistry {
    voices_dir: PathBuf,
    emb_cache_dir: PathBuf,
    model_id: String,
    threshold: f32,
    /// Speaker id -> its centroid embedding.
    speakers: HashMap<String, Vec<f32>>,
}

impl SpeakerRegistry {
    pub fn open(
        voices_dir: PathBuf,
        emb_cache_dir: PathBuf,
        model_id: String,
        threshold: f32,
    ) -> Result<Self> {
        std::fs::create_dir_all(&emb_cache_dir)
            .with_context(|| format!("creating embedding cache dir {emb_cache_dir:?}"))?;
        Ok(Self {
            voices_dir,
            emb_cache_dir,
            model_id,
            threshold,
            speakers: HashMap::new(),
        })
    }

    /// Scan `voices/`, embedding each reference file (or reading its cached
    /// vector) and averaging per speaker.
    ///
    /// A speaker whose files cannot be read is warned about and skipped.
    /// One unreadable WAV must not take ambient ingest down with it.
    pub fn load_reference_audio(&mut self, embedder: &dyn SpeakerEmbedder) -> Result<()> {
        let entries = match std::fs::read_dir(&self.voices_dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e).context("reading voices dir"),
        };
        for entry in entries.flatten() {
            if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let dir_name = entry.file_name().to_string_lossy().into_owned();
            // The id is the marker's first line when there is one; the
            // directory name only ever supplies the *display* name, plus
            // the id for a directory the user made by hand.
            let id = speaker_ids_in(&entry.path())
                .into_iter()
                .next()
                .unwrap_or_else(|| dir_name.clone());
            let mut vectors = Vec::new();
            for file in std::fs::read_dir(entry.path())
                .into_iter()
                .flatten()
                .flatten()
            {
                let path = file.path();
                if path.extension().and_then(|e| e.to_str()) != Some("wav") {
                    continue;
                }
                match self.embedding_for(&path, embedder) {
                    Ok(v) => vectors.push(v),
                    Err(e) => warn!("speaker {dir_name}: skipping {path:?}: {e}"),
                }
            }
            if vectors.is_empty() {
                warn!("speaker {dir_name}: no usable reference audio; speaker disabled");
                continue;
            }
            match centroid(&vectors) {
                Ok(c) => {
                    self.speakers.insert(id, c);
                }
                // Averaging vectors of different lengths cannot produce a
                // meaningful centroid; the old code took the first
                // vector's length and truncated the rest, which skewed the
                // result towards whichever file `read_dir` happened to
                // return first. Disabling the speaker is the same
                // treatment unreadable reference audio already gets.
                Err(e) => warn!("speaker {dir_name}: {e}; speaker disabled"),
            }
        }
        Ok(())
    }

    /// Cached embedding for one reference file, computing and storing it on
    /// a miss. The cache key is the file's content hash and the model id.
    fn embedding_for(&self, path: &Path, embedder: &dyn SpeakerEmbedder) -> Result<Vec<f32>> {
        let bytes = std::fs::read(path).with_context(|| format!("reading {path:?}"))?;
        let key = format!("{}.{}.emb", sha256_hex(&bytes), self.model_id);
        let cached = self.emb_cache_dir.join(&key);
        if let Ok(raw) = std::fs::read(&cached) {
            return Ok(decode_embedding(&raw));
        }
        let pcm = read_wav_mono_16k(&bytes)?;
        let vector = embedder.embed(&pcm)?;
        if let Err(e) = std::fs::write(&cached, encode_embedding(&vector)) {
            warn!("could not cache embedding at {cached:?}: {e}");
        }
        Ok(vector)
    }

    /// Register an embedding at runtime — used for auto-enrolled candidates,
    /// so a newly seen voice matches on its next segment.
    pub fn add_runtime(&mut self, id: String, embedding: Vec<f32>) {
        self.speakers.insert(id, embedding);
    }

    /// Best speaker above the threshold, or `None`.
    pub fn match_speaker(&self, embedding: &[f32]) -> Option<SpeakerMatch> {
        let mut best: Option<SpeakerMatch> = None;
        for (id, speaker_vector) in &self.speakers {
            let score = cosine_similarity(embedding, speaker_vector);
            if score < self.threshold {
                continue;
            }
            if best.as_ref().is_none_or(|b| score > b.score) {
                best = Some(SpeakerMatch {
                    id: id.clone(),
                    score,
                });
            }
        }
        best
    }
}

pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    dot / (na * nb)
}

/// Mean of `vectors`, which must all share one dimension.
///
/// A mismatch is an explicit error rather than a truncation: silently
/// averaging over the first vector's length produces a plausible-looking
/// centroid that is wrong, and the only way it arises — reference vectors
/// computed by two different embedding models — is exactly the case where
/// a wrong centroid misattributes speech to the wrong person.
fn centroid(vectors: &[Vec<f32>]) -> Result<Vec<f32>> {
    let dim = vectors[0].len();
    if let Some(other) = vectors.iter().find(|v| v.len() != dim) {
        anyhow::bail!(
            "reference embeddings disagree on dimension ({dim} vs {})",
            other.len()
        );
    }
    let mut out = vec![0.0; dim];
    for v in vectors {
        for (i, x) in v.iter().enumerate() {
            out[i] += x;
        }
    }
    for x in &mut out {
        *x /= vectors.len() as f32;
    }
    Ok(out)
}

/// Serialize an embedding to the little-endian f32 bytes stored in the
/// cache. `pub` because Task 9 (candidate store) reuses the same on-disk
/// vector encoding.
pub fn encode_embedding(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|f| f.to_le_bytes()).collect()
}

/// Inverse of [`encode_embedding`]. `pub` for the same reason.
pub fn decode_embedding(raw: &[u8]) -> Vec<f32> {
    raw.as_chunks::<4>()
        .0
        .iter()
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

fn read_wav_mono_16k(bytes: &[u8]) -> Result<Vec<i16>> {
    let mut reader = hound::WavReader::new(std::io::Cursor::new(bytes))?;
    let spec = reader.spec();
    anyhow::ensure!(spec.channels == 1, "reference audio must be mono");
    anyhow::ensure!(
        spec.sample_rate == crate::voice::PIPELINE_SAMPLE_RATE,
        "reference audio must be {} Hz, got {}",
        crate::voice::PIPELINE_SAMPLE_RATE,
        spec.sample_rate
    );
    Ok(reader.samples::<i16>().collect::<Result<Vec<_>, _>>()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ambient::audio::FixedEmbedder;

    /// Write a 1-second 16 kHz mono WAV of silence.
    fn write_wav(path: &std::path::Path) {
        write_wav_of(path, 0);
    }

    /// Write a 1-second 16 kHz mono WAV of a constant sample value, so two
    /// reference files can differ in content (and therefore in cache key).
    fn write_wav_of(path: &std::path::Path, sample: i16) {
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 16_000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut w = hound::WavWriter::create(path, spec).unwrap();
        for _ in 0..16_000 {
            w.write_sample(sample).unwrap();
        }
        w.finalize().unwrap();
    }

    fn voices_with(names: &[&str]) -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let voices = tmp.path().join("voices");
        for n in names {
            let dir = voices.join(n);
            std::fs::create_dir_all(&dir).unwrap();
            write_wav(&dir.join("sample.wav"));
        }
        (tmp, voices)
    }

    #[test]
    fn cosine_similarity_is_one_for_identical_vectors() {
        assert!((cosine_similarity(&[1.0, 2.0, 3.0], &[1.0, 2.0, 3.0]) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn cosine_similarity_is_zero_for_orthogonal_vectors() {
        assert!(cosine_similarity(&[1.0, 0.0], &[0.0, 1.0]).abs() < 1e-6);
    }

    #[test]
    fn matches_a_registered_speaker_above_the_threshold() {
        let (tmp, voices) = voices_with(&["me"]);
        let mut reg =
            SpeakerRegistry::open(voices, tmp.path().join("emb"), "test-model".into(), 0.55)
                .unwrap();
        reg.load_reference_audio(&FixedEmbedder::new(vec![1.0, 0.0, 0.0]))
            .unwrap();

        let m = reg.match_speaker(&[1.0, 0.0, 0.0]).expect("matched");
        assert_eq!(m.id, "me");
        assert!(m.score > 0.99);
    }

    #[test]
    fn returns_none_below_the_threshold() {
        let (tmp, voices) = voices_with(&["me"]);
        let mut reg =
            SpeakerRegistry::open(voices, tmp.path().join("emb"), "test-model".into(), 0.55)
                .unwrap();
        reg.load_reference_audio(&FixedEmbedder::new(vec![1.0, 0.0, 0.0]))
            .unwrap();
        assert!(reg.match_speaker(&[0.0, 1.0, 0.0]).is_none());
    }

    #[test]
    fn the_directory_name_is_the_speaker_id() {
        let (tmp, voices) = voices_with(&["blithe-otter-42"]);
        let mut reg =
            SpeakerRegistry::open(voices, tmp.path().join("emb"), "test-model".into(), 0.55)
                .unwrap();
        reg.load_reference_audio(&FixedEmbedder::new(vec![1.0, 0.0, 0.0]))
            .unwrap();
        assert_eq!(
            reg.match_speaker(&[1.0, 0.0, 0.0]).unwrap().id,
            "blithe-otter-42"
        );
    }

    /// The directory name is the *display* name; the id must be stable
    /// across a rename, or `voices/blithe-otter-42/` -> `voices/tanaka-san/`
    /// silently forks one person into two unrelated ids and every past
    /// transcript stops resolving.
    #[test]
    fn an_id_marker_file_overrides_the_directory_name_as_the_speaker_id() {
        let (tmp, voices) = voices_with(&["tanaka-san"]);
        std::fs::write(voices.join("tanaka-san").join("id"), "blithe-otter-42\n").unwrap();
        let mut reg = SpeakerRegistry::open(
            voices.clone(),
            tmp.path().join("emb"),
            "test-model".into(),
            0.55,
        )
        .unwrap();
        reg.load_reference_audio(&FixedEmbedder::new(vec![1.0, 0.0, 0.0]))
            .unwrap();
        assert_eq!(
            reg.match_speaker(&[1.0, 0.0, 0.0]).unwrap().id,
            "blithe-otter-42",
            "the marker is the id; the directory name is only the display name"
        );
        assert_eq!(
            SpeakerNames::scan(&voices).display_name("blithe-otter-42"),
            "tanaka-san",
            "display name resolves from the current directory name"
        );
    }

    /// A speaker directory the user created by hand (`me/`, `agent/`) has
    /// no marker, and must keep working with its directory name as its id.
    #[test]
    fn a_directory_without_a_marker_keeps_using_its_name_as_the_id() {
        let (tmp, voices) = voices_with(&["me"]);
        let mut reg = SpeakerRegistry::open(
            voices.clone(),
            tmp.path().join("emb"),
            "test-model".into(),
            0.55,
        )
        .unwrap();
        reg.load_reference_audio(&FixedEmbedder::new(vec![1.0, 0.0, 0.0]))
            .unwrap();
        assert_eq!(reg.match_speaker(&[1.0, 0.0, 0.0]).unwrap().id, "me");
        assert_eq!(SpeakerNames::scan(&voices).display_name("me"), "me");
    }

    /// A merged speaker directory lists several ids; the first is the
    /// canonical one used for new attributions, the rest are aliases that
    /// must still resolve to the same display name.
    #[test]
    fn every_id_in_a_marker_resolves_to_the_directory_name() {
        let (tmp, voices) = voices_with(&["me"]);
        std::fs::write(voices.join("me").join("id"), "me\nblithe-otter-42\n").unwrap();
        let mut reg = SpeakerRegistry::open(
            voices.clone(),
            tmp.path().join("emb"),
            "test-model".into(),
            0.55,
        )
        .unwrap();
        reg.load_reference_audio(&FixedEmbedder::new(vec![1.0, 0.0, 0.0]))
            .unwrap();
        assert_eq!(
            reg.match_speaker(&[1.0, 0.0, 0.0]).unwrap().id,
            "me",
            "the first marker line stays canonical"
        );
        let names = SpeakerNames::scan(&voices);
        assert_eq!(names.display_name("blithe-otter-42"), "me");
        let mut ids = names.ids_for("me");
        ids.sort();
        assert_eq!(
            ids,
            vec!["blithe-otter-42".to_string(), "me".to_string()],
            "both the canonical id and the merged alias resolve to this speaker"
        );
    }

    /// Reference vectors of different lengths cannot be averaged. Taking
    /// the first vector's length and truncating the rest produced a
    /// centroid skewed towards whichever file happened to be read first.
    #[test]
    fn a_dimension_mismatch_disables_the_speaker_instead_of_skewing_its_centroid() {
        let (tmp, voices) = voices_with(&["mixed"]);
        // A second reference file with *different* content, so it hashes to
        // its own cache key and the speaker has two vectors to average.
        write_wav_of(&voices.join("mixed").join("second.wav"), 1234);
        // Pre-seed one cached embedding at a different dimension than the
        // embedder produces, which is what a half-swapped model looks like.
        let emb_dir = tmp.path().join("emb");
        std::fs::create_dir_all(&emb_dir).unwrap();
        let bytes = std::fs::read(voices.join("mixed").join("second.wav")).unwrap();
        std::fs::write(
            emb_dir.join(format!(
                "{}.{}.emb",
                crate::image_cache::sha256_hex(&bytes),
                "test-model"
            )),
            encode_embedding(&[1.0, 0.0]),
        )
        .unwrap();

        let mut reg = SpeakerRegistry::open(voices, emb_dir, "test-model".into(), 0.55).unwrap();
        reg.load_reference_audio(&FixedEmbedder::new(vec![1.0, 0.0, 0.0]))
            .expect("one bad speaker must not fail the whole load");
        assert!(
            reg.match_speaker(&[1.0, 0.0, 0.0]).is_none(),
            "a speaker whose references disagree on dimension is disabled, not averaged"
        );
    }

    #[test]
    fn embeddings_are_cached_by_reference_sha_and_model_id() {
        let (tmp, voices) = voices_with(&["me"]);
        let emb_dir = tmp.path().join("emb");
        let mut reg =
            SpeakerRegistry::open(voices.clone(), emb_dir.clone(), "model-a".into(), 0.55).unwrap();
        reg.load_reference_audio(&FixedEmbedder::new(vec![1.0, 0.0, 0.0]))
            .unwrap();
        let after_first = std::fs::read_dir(&emb_dir).unwrap().count();
        assert_eq!(after_first, 1);

        // Same files, different model id: a second cache entry, because the
        // vector is model-dependent and the workspace stores no vectors.
        let mut reg_b =
            SpeakerRegistry::open(voices, emb_dir.clone(), "model-b".into(), 0.55).unwrap();
        reg_b
            .load_reference_audio(&FixedEmbedder::new(vec![0.0, 1.0, 0.0]))
            .unwrap();
        assert_eq!(std::fs::read_dir(&emb_dir).unwrap().count(), 2);
        assert_eq!(reg_b.match_speaker(&[0.0, 1.0, 0.0]).unwrap().id, "me");
    }

    #[test]
    fn an_unreadable_reference_file_disables_that_speaker_without_failing_the_load() {
        let (tmp, voices) = voices_with(&["me"]);
        let broken = voices.join("broken");
        std::fs::create_dir_all(&broken).unwrap();
        std::fs::write(broken.join("sample.wav"), b"not a wav at all").unwrap();

        let mut reg =
            SpeakerRegistry::open(voices, tmp.path().join("emb"), "test-model".into(), 0.55)
                .unwrap();
        reg.load_reference_audio(&FixedEmbedder::new(vec![1.0, 0.0, 0.0]))
            .expect("load succeeds despite the broken speaker");
        assert_eq!(reg.match_speaker(&[1.0, 0.0, 0.0]).unwrap().id, "me");
    }
}
