//! Auto-enrolled speakers awaiting a name.
//!
//! A day of ambient audio contains television, shop staff, train
//! announcements and passers-by. Writing every first-seen voice straight
//! into the workspace would bury the handful of people worth naming under
//! hundreds of one-off entries, so first sight enrols a **candidate** in
//! the cache, and only sustained presence promotes it into `voices/`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use tracing::warn;

use super::registry::{ID_MARKER, decode_embedding, encode_embedding, speaker_ids_in};
use crate::voice::PIPELINE_SAMPLE_RATE;

/// How many sample utterances to keep per candidate, so a human deciding
/// whether to name it has something to recognise it by.
const MAX_SAMPLES: usize = 5;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CandidateStats {
    /// Cumulative gated speech, in **milliseconds**.
    ///
    /// Milliseconds, not seconds, because the spec makes `speech_ms` one
    /// measure used in three places — the transcript field, the
    /// `min_embed_ms` comparison, and this promotion total. Truncating
    /// each observation to whole seconds broke that third use: a voice
    /// made of 1.9 s utterances accumulated 1 s per observation, so it
    /// took nearly twice as long to reach `promote_after_seconds` as the
    /// configuration says. The division now happens once, at the
    /// [`CandidateStore::is_promotable`] comparison.
    #[serde(default)]
    pub speech_ms: u64,
    /// Distinct logical days this voice was heard on.
    pub days_seen: Vec<NaiveDate>,
    pub first_seen: DateTime<Utc>,
    /// A few transcribed utterances, for recognition.
    #[serde(default)]
    pub samples: Vec<String>,
    /// Number of observations folded into the centroid, for the running
    /// mean. Persisted so a restart does not corrupt the weighting: this
    /// is deliberately distinct from `days_seen.len()`, which counts only
    /// distinct days and undercounts a voice heard several times in one
    /// afternoon.
    ///
    /// The default is **1**, not 0: a candidate always exists because at
    /// least one segment was folded into its centroid, and a 0 would make
    /// the next running-mean update `(centroid * 0 + x) / 1` — discarding
    /// the entire accumulated centroid in favour of a single observation.
    #[serde(default = "default_observations")]
    pub observations: u32,
    /// Embedding model the centroid was computed with.
    ///
    /// Registered speakers get this for free: their cache key is (file
    /// sha256 x model id), so a model swap recomputes. A candidate's
    /// centroid has no such key — it is derived from audio that is long
    /// gone — so the model id has to travel with it. Without this, swapping
    /// `[ambient].embedding_model_dir` either leaves candidates permanently
    /// unmatchable (different dimension) or, far worse, keeps matching
    /// against them and attributes speech to the wrong id when the new
    /// model happens to share a dimension — and 192 dimensions is shared
    /// across several common checkpoints.
    #[serde(default)]
    pub model_id: String,
}

fn default_observations() -> u32 {
    1
}

#[derive(Debug, Clone)]
pub struct Candidate {
    pub id: String,
    pub centroid: Vec<f32>,
    pub stats: CandidateStats,
}

pub struct CandidateStore {
    dir: PathBuf,
    /// Embedding model the *live* pipeline is using. Candidates recorded
    /// under any other model are left on disk but never loaded.
    model_id: String,
    candidates: HashMap<String, Candidate>,
}

impl CandidateStore {
    /// Open the store, loading every candidate recorded under `model_id`.
    ///
    /// Candidates from a different embedding model are **skipped, not
    /// deleted**. Skipping is enough for correctness — they are never
    /// matched against, never promoted, never counted — and deleting would
    /// be a silent, unattended `remove_dir_all` over the only surviving
    /// artefact of audio that is otherwise gone: the representative clip a
    /// user might still want to promote by hand. It would also make trying
    /// a different checkpoint and switching back a destructive operation,
    /// which it should not be. They cost a few kilobytes each; a future
    /// prune can be an explicit, asked-for action.
    pub fn open(dir: PathBuf, model_id: String) -> Result<Self> {
        std::fs::create_dir_all(&dir).with_context(|| format!("creating candidate dir {dir:?}"))?;
        let mut candidates = HashMap::new();
        let mut foreign = 0usize;
        for entry in std::fs::read_dir(&dir)?.flatten() {
            if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let id = entry.file_name().to_string_lossy().into_owned();
            match load_candidate(&entry.path(), &id) {
                Ok(c) if c.stats.model_id != model_id => {
                    foreign += 1;
                }
                Ok(c) => {
                    candidates.insert(id, c);
                }
                Err(e) => warn!("skipping unreadable candidate {id}: {e}"),
            }
        }
        if foreign > 0 {
            warn!(
                "ambient: {foreign} candidate(s) were enrolled with a different embedding model \
                 and are ignored under {model_id}; they are left on disk, not deleted"
            );
        }
        Ok(Self {
            dir,
            model_id,
            candidates,
        })
    }

    /// Register a newly seen voice. Returns the minted id.
    pub fn enrol(
        &mut self,
        embedding: Vec<f32>,
        clip: &[i16],
        day: NaiveDate,
        speech_ms: u32,
        sample_text: &str,
    ) -> Result<String> {
        let id = grain_id::GrainId::random().to_string();
        let dir = self.dir.join(&id);
        std::fs::create_dir_all(&dir)?;
        write_wav(&dir.join("clip.wav"), clip)?;
        std::fs::write(dir.join("centroid.emb"), encode_embedding(&embedding))?;

        let stats = CandidateStats {
            speech_ms: speech_ms as u64,
            days_seen: vec![day],
            first_seen: Utc::now(),
            samples: sample_vec(sample_text),
            observations: 1,
            model_id: self.model_id.clone(),
        };
        std::fs::write(dir.join("stats.json"), serde_json::to_vec_pretty(&stats)?)?;

        self.candidates.insert(
            id.clone(),
            Candidate {
                id: id.clone(),
                centroid: embedding,
                stats,
            },
        );
        Ok(id)
    }

    /// Fold another segment into an existing candidate.
    pub fn observe(
        &mut self,
        id: &str,
        embedding: &[f32],
        day: NaiveDate,
        speech_ms: u32,
        sample_text: &str,
    ) -> Result<()> {
        let Some(c) = self.candidates.get_mut(id) else {
            bail!("no such candidate: {id}");
        };
        // An explicit error rather than a truncating `take(centroid.len())`:
        // folding a shorter or longer vector into the mean quietly produces
        // a centroid that is partly one model's geometry and partly
        // another's, and the caller degrades gracefully on an error here
        // (the transcript is still written) but cannot notice a silent one.
        if embedding.len() != c.centroid.len() {
            bail!(
                "candidate {id}: embedding has {} dimensions, centroid has {}",
                embedding.len(),
                c.centroid.len()
            );
        }
        // Running mean, so a candidate's centroid tracks the voice rather
        // than being pinned to whatever the first segment sounded like.
        // `observations` is persisted (see `CandidateStats`) precisely so
        // this weight survives a restart intact.
        let n = c.stats.observations as f32;
        for (i, x) in embedding.iter().enumerate() {
            c.centroid[i] = (c.centroid[i] * n + x) / (n + 1.0);
        }
        c.stats.observations += 1;
        c.stats.speech_ms += speech_ms as u64;
        if !c.stats.days_seen.contains(&day) {
            c.stats.days_seen.push(day);
        }
        if !sample_text.trim().is_empty() && c.stats.samples.len() < MAX_SAMPLES {
            c.stats.samples.push(sample_text.to_string());
        }

        let dir = self.dir.join(id);
        std::fs::write(dir.join("centroid.emb"), encode_embedding(&c.centroid))?;
        std::fs::write(dir.join("stats.json"), serde_json::to_vec_pretty(&c.stats)?)?;
        Ok(())
    }

    pub fn list(&self) -> Vec<&Candidate> {
        let mut out: Vec<&Candidate> = self.candidates.values().collect();
        out.sort_by_key(|c| std::cmp::Reverse(c.stats.speech_ms));
        out
    }

    /// Look up a single candidate by id.
    pub fn get(&self, id: &str) -> Option<&Candidate> {
        self.candidates.get(id)
    }

    /// Both thresholds must be cleared. Cumulative time alone would promote
    /// a television left on all afternoon; distinct days alone would promote
    /// a passing greeting heard twice.
    ///
    /// The seconds/milliseconds conversion happens **here**, once, rather
    /// than per observation — see [`CandidateStats::speech_ms`].
    pub fn is_promotable(&self, id: &str, after_seconds: u32, after_days: u32) -> bool {
        self.candidates.get(id).is_some_and(|c| {
            c.stats.speech_ms >= after_seconds as u64 * 1000
                && c.stats.days_seen.len() as u32 >= after_days
        })
    }

    /// Export a candidate into `voices/<name>/` and stop tracking it.
    /// Returns the directory name used.
    ///
    /// Two properties this has to get right, both of them about not
    /// destroying things the user curated:
    ///
    /// - **The clip is named `<grain-id>.wav`, not `clip.wav`.**
    ///   `speaker_promote(id, name = "me")` is exactly what the model does
    ///   when the user says "that was me", and a fixed filename made that a
    ///   model-invoked overwrite of whatever `voices/me/clip.wav` held. The
    ///   registry averages every `*.wav` in the directory, so a unique name
    ///   turns merging into an existing speaker into correct behaviour.
    /// - **The grain-id is recorded in an [`ID_MARKER`] file**, so the
    ///   directory can be renamed afterwards without changing the id every
    ///   past transcript refers to. When the directory already existed, its
    ///   own name is written as the *first* (canonical) id, because
    ///   transcripts may already say `me`; the promoted grain-id joins as
    ///   an alias, and both resolve to the same display name.
    pub fn promote(&mut self, id: &str, name: Option<&str>, voices_dir: &Path) -> Result<String> {
        let Some(c) = self.candidates.get(id) else {
            bail!("no such candidate: {id}");
        };
        let dir_name = match name {
            Some(n) => {
                validate_speaker_name(n)?;
                n.to_string()
            }
            None => c.id.clone(),
        };

        // `voices_dir` is ours to create; canonicalise it once so the
        // eventual write always lands on a fully resolved, symlink-free
        // root rather than whatever relative form the caller passed in.
        std::fs::create_dir_all(voices_dir).with_context(|| format!("creating {voices_dir:?}"))?;
        let voices_root = voices_dir
            .canonicalize()
            .with_context(|| format!("resolving {voices_dir:?}"))?;

        // The character blocklist in `validate_speaker_name` is a guess
        // about this OS's path grammar, and it has already been wrong
        // twice on this same function: first backslash, then a bare
        // Windows drive prefix. Assert the actual invariant instead of
        // guessing again: join `dir_name` onto the un-canonicalised
        // `voices_dir`, exactly as this function would, and require the
        // join's parent to still be `voices_dir`. A component that
        // hijacks `Path::join` (a drive prefix, a UNC prefix, and so on)
        // replaces the whole path rather than appending to it, so the
        // parent comes out completely different; this catches that
        // regardless of which character caused it. It is also
        // side-effect free, since comparing `Path` values touches no
        // disk, so a rejected name never causes anything to be created
        // that would need cleaning up afterwards. Canonicalising the
        // target here instead would both require it to exist first,
        // which it does not yet, and risk creating -- and then having to
        // destroy -- whatever real location a hijacked join resolved to,
        // which for a bare drive prefix could be nowhere near
        // `voices_dir` at all.
        let target = voices_dir.join(&dir_name);
        if target.parent() != Some(voices_dir) {
            bail!("speaker name would escape the voices directory: {dir_name:?}");
        }
        let target = voices_root.join(&dir_name);

        let target_existed = target.exists();
        std::fs::create_dir_all(&target).with_context(|| format!("creating {target:?}"))?;
        // Named after the candidate, so promoting a second voice into the
        // same speaker adds a reference file instead of replacing one.
        let clip_name = format!("{id}.wav");
        if let Err(e) = std::fs::copy(self.dir.join(id).join("clip.wav"), target.join(&clip_name)) {
            // Don't leave a stray, empty directory ahead of a promotion
            // that did not actually happen -- but only if this call is
            // the one that just created it; a pre-existing target (a
            // merge) is not ours to delete.
            if !target_existed {
                std::fs::remove_dir_all(&target).ok();
            }
            return Err(e).with_context(|| format!("copying candidate clip into {target:?}"));
        }

        if let Err(e) = write_id_marker(&target, id, &dir_name, target_existed) {
            // Same rule as the copy above: undo only what this call made.
            // A directory carrying the clip but no marker would attribute
            // this voice under its *directory name* instead of the id the
            // transcripts already use, which is the exact identity fork
            // the marker exists to prevent.
            std::fs::remove_file(target.join(&clip_name)).ok();
            if !target_existed {
                std::fs::remove_dir_all(&target).ok();
            }
            return Err(e);
        }

        std::fs::remove_dir_all(self.dir.join(id)).ok();
        self.candidates.remove(id);
        Ok(dir_name)
    }
}

/// Record `grain_id` in the speaker directory's [`ID_MARKER`], preserving
/// whatever was already there.
///
/// When the directory pre-dates this promotion and carries no marker, its
/// own name goes in first and stays canonical: `voices/me/` had id `me`
/// before the merge, and every transcript already written says `me`.
/// Promoting a candidate into it must add an alias, not silently rename
/// that speaker's identity to a grain-id nothing else refers to.
fn write_id_marker(
    target: &Path,
    grain_id: &str,
    dir_name: &str,
    target_existed: bool,
) -> Result<()> {
    let mut ids = speaker_ids_in(target);
    if ids.is_empty() && target_existed {
        ids.push(dir_name.to_string());
    }
    if !ids.iter().any(|i| i == grain_id) {
        ids.push(grain_id.to_string());
    }
    let mut body = ids.join("\n");
    body.push('\n');
    let path = target.join(ID_MARKER);
    std::fs::write(&path, body).with_context(|| format!("writing speaker id marker {path:?}"))
}

/// A speaker name becomes a directory name under the workspace, so it must
/// be a single harmless path segment.
fn validate_speaker_name(name: &str) -> Result<()> {
    let trimmed = name.trim();
    if trimmed.is_empty() || trimmed == "." || trimmed == ".." {
        bail!("speaker name must not be empty or a directory reference: {name:?}");
    }
    if name.contains('/') || name.contains('\\') || name.contains(':') || name.contains("..") {
        bail!("speaker name must be a single path segment: {name:?}");
    }
    Ok(())
}

fn sample_vec(text: &str) -> Vec<String> {
    if text.trim().is_empty() {
        Vec::new()
    } else {
        vec![text.to_string()]
    }
}

fn load_candidate(dir: &Path, id: &str) -> Result<Candidate> {
    let centroid = decode_embedding(&std::fs::read(dir.join("centroid.emb"))?);
    let stats: CandidateStats = serde_json::from_slice(&std::fs::read(dir.join("stats.json"))?)?;
    Ok(Candidate {
        id: id.to_string(),
        centroid,
        stats,
    })
}

fn write_wav(path: &Path, pcm: &[i16]) -> Result<()> {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: PIPELINE_SAMPLE_RATE,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut w = hound::WavWriter::create(path, spec)?;
    for s in pcm {
        w.write_sample(*s)?;
    }
    w.finalize()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn day(d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 8, d).unwrap()
    }

    fn store() -> (tempfile::TempDir, CandidateStore) {
        let tmp = tempfile::tempdir().unwrap();
        let store =
            CandidateStore::open(tmp.path().join("candidates"), "test-model".into()).unwrap();
        (tmp, store)
    }

    #[test]
    fn enrolling_mints_an_id_and_persists_the_candidate() {
        let (tmp, mut store) = store();
        let id = store
            .enrol(vec![1.0, 0.0], &vec![0i16; 16_000], day(26), 1_000, "hello")
            .unwrap();
        assert!(!id.is_empty());
        assert!(
            tmp.path()
                .join("candidates")
                .join(&id)
                .join("clip.wav")
                .exists()
        );
        assert!(
            tmp.path()
                .join("candidates")
                .join(&id)
                .join("stats.json")
                .exists()
        );

        // Reload from disk: candidates survive a restart.
        let reloaded =
            CandidateStore::open(tmp.path().join("candidates"), "test-model".into()).unwrap();
        assert_eq!(reloaded.list().len(), 1);
        assert_eq!(reloaded.list()[0].id, id);
    }

    /// A candidate centroid is model-specific and has no cache key that a
    /// model swap invalidates, so the model id travels in its stats. After
    /// a swap the old candidates must not be matched against: a different
    /// dimension makes them dead weight, and — much worse — the *same*
    /// dimension under a different model attributes speech to the wrong id.
    #[test]
    fn candidates_enrolled_under_another_model_are_ignored_but_left_on_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("candidates");
        let id = {
            let mut store = CandidateStore::open(dir.clone(), "model-a".into()).unwrap();
            store
                .enrol(vec![1.0, 0.0], &vec![0i16; 16_000], day(26), 60_000, "a")
                .unwrap()
        };

        let swapped = CandidateStore::open(dir.clone(), "model-b".into()).unwrap();
        assert!(
            swapped.get(&id).is_none(),
            "a centroid from another model must not be matched against"
        );
        assert!(swapped.list().is_empty());
        assert!(
            dir.join(&id).join("clip.wav").exists(),
            "skipped, not deleted: the clip is the only surviving artefact of that audio"
        );

        // Switching back must find them again — this is why they are kept.
        let back = CandidateStore::open(dir, "model-a".into()).unwrap();
        assert!(back.get(&id).is_some());
    }

    #[test]
    fn is_not_promotable_at_fifty_nine_seconds() {
        let (_tmp, mut store) = store();
        let id = store
            .enrol(vec![1.0, 0.0], &vec![0i16; 16_000], day(26), 59_000, "a")
            .unwrap();
        store.observe(&id, &[1.0, 0.0], day(27), 0, "b").unwrap();
        assert!(!store.is_promotable(&id, 60, 2), "59s < 60s threshold");
    }

    #[test]
    fn is_promotable_at_exactly_sixty_seconds_across_two_days() {
        let (_tmp, mut store) = store();
        let id = store
            .enrol(vec![1.0, 0.0], &vec![0i16; 16_000], day(26), 30_000, "a")
            .unwrap();
        store
            .observe(&id, &[1.0, 0.0], day(27), 30_000, "b")
            .unwrap();
        assert!(store.is_promotable(&id, 60, 2));
    }

    #[test]
    fn is_not_promotable_on_a_single_day_however_long() {
        let (_tmp, mut store) = store();
        let id = store
            .enrol(vec![1.0, 0.0], &vec![0i16; 16_000], day(26), 600_000, "a")
            .unwrap();
        assert!(
            !store.is_promotable(&id, 60, 2),
            "one day of television must not promote"
        );
    }

    #[test]
    fn observing_the_same_day_twice_counts_it_once() {
        let (_tmp, mut store) = store();
        let id = store
            .enrol(vec![1.0, 0.0], &vec![0i16; 16_000], day(26), 40_000, "a")
            .unwrap();
        store
            .observe(&id, &[1.0, 0.0], day(26), 40_000, "b")
            .unwrap();
        assert!(!store.is_promotable(&id, 60, 2), "still only one day seen");
    }

    #[test]
    fn promotion_writes_the_clip_into_the_workspace_under_the_id() {
        let (tmp, mut store) = store();
        let voices = tmp.path().join("voices");
        let id = store
            .enrol(vec![1.0, 0.0], &vec![0i16; 16_000], day(26), 60_000, "a")
            .unwrap();
        let name = store.promote(&id, None, &voices).unwrap();
        assert_eq!(name, id, "no name given, so the grain-id is the directory");
        assert!(voices.join(&id).join(format!("{id}.wav")).exists());
        assert!(
            store.list().iter().all(|c| c.id != id),
            "promoted, so no longer a candidate"
        );
    }

    #[test]
    fn promotion_with_a_name_uses_it_as_the_directory() {
        let (tmp, mut store) = store();
        let voices = tmp.path().join("voices");
        let id = store
            .enrol(vec![1.0, 0.0], &vec![0i16; 16_000], day(26), 60_000, "a")
            .unwrap();
        let name = store.promote(&id, Some("tanaka-san"), &voices).unwrap();
        assert_eq!(name, "tanaka-san");
        assert!(voices.join("tanaka-san").join(format!("{id}.wav")).exists());
    }

    #[test]
    fn promotion_refuses_a_name_that_would_escape_the_voices_directory() {
        let (tmp, mut store) = store();
        let voices = tmp.path().join("voices");
        let id = store
            .enrol(vec![1.0, 0.0], &vec![0i16; 16_000], day(26), 60_000, "a")
            .unwrap();
        assert!(
            store
                .promote(&id, Some("../../etc/passwd"), &voices)
                .is_err()
        );
        assert!(store.promote(&id, Some("has/slash"), &voices).is_err());
    }

    /// A voice heard four times in one afternoon should weight its fourth
    /// observation as a fifth of the running mean, not half of it. That
    /// requires the observation count to survive a restart, not just the
    /// count of distinct days.
    #[test]
    fn reopening_preserves_the_observation_count_not_just_distinct_days() {
        let (tmp, mut store) = store();
        let id = store
            .enrol(vec![1.0, 0.0], &vec![0i16; 16_000], day(26), 10_000, "a")
            .unwrap();
        store
            .observe(&id, &[1.0, 0.0], day(26), 10_000, "b")
            .unwrap();
        store
            .observe(&id, &[1.0, 0.0], day(26), 10_000, "c")
            .unwrap();

        let reloaded =
            CandidateStore::open(tmp.path().join("candidates"), "test-model".into()).unwrap();
        let reloaded_candidate = reloaded.list().into_iter().find(|c| c.id == id).unwrap();
        assert_eq!(
            reloaded_candidate.stats.observations, 3,
            "three observations were folded in, even though they all land on one day"
        );
    }

    /// Windows accepts backslash as a path separator too, so a check that
    /// only rejects `/` would still let `..\..\` escape the voices
    /// directory on this OS.
    #[test]
    fn promotion_refuses_a_name_with_backslash_traversal() {
        let (tmp, mut store) = store();
        let voices = tmp.path().join("voices");
        let id = store
            .enrol(vec![1.0, 0.0], &vec![0i16; 16_000], day(26), 60_000, "a")
            .unwrap();
        assert!(
            store
                .promote(&id, Some("..\\..\\etc\\passwd"), &voices)
                .is_err()
        );
        assert!(store.promote(&id, Some("has\\backslash"), &voices).is_err());
    }

    /// On Windows, `PathBuf::join` treats a component with a drive prefix
    /// as absolute and *replaces* the base path instead of appending to
    /// it: `voices_dir.join("C:foo")` is `"C:foo"`, entirely outside
    /// `voices_dir`. Neither `/` nor `\` nor `..` catches this; only a
    /// dedicated rejection (and the join-parent containment check) does.
    #[test]
    fn promotion_refuses_a_drive_qualified_name() {
        let (tmp, mut store) = store();
        let voices = tmp.path().join("voices");
        let id = store
            .enrol(vec![1.0, 0.0], &vec![0i16; 16_000], day(26), 60_000, "a")
            .unwrap();
        assert!(store.promote(&id, Some("C:foo"), &voices).is_err());
        assert!(store.promote(&id, Some("C:"), &voices).is_err());
    }

    /// `voices_dir.join(".")` is `voices_dir` itself, so a bare "." would
    /// collapse every candidate promoted under that name into the voices
    /// root rather than a subdirectory of it. The join-parent containment
    /// check happens to reject this too -- Rust's `Path` normalises away
    /// a non-leading "." component, so `target.parent()` strips
    /// `voices_dir`'s own last component instead of the phantom ".", and
    /// comes out one level too high -- but that is an accident of how
    /// `Path::parent` is implemented, not a stated guarantee. A
    /// `starts_with`-style containment check would *not* catch it, since
    /// `voices_dir` trivially starts with itself. Keep this explicit
    /// rejection regardless, so the behaviour does not depend on which
    /// comparison the containment check happens to use.
    #[test]
    fn promotion_refuses_a_bare_dot_name() {
        let (tmp, mut store) = store();
        let voices = tmp.path().join("voices");
        let id = store
            .enrol(vec![1.0, 0.0], &vec![0i16; 16_000], day(26), 60_000, "a")
            .unwrap();
        assert!(store.promote(&id, Some("."), &voices).is_err());
        assert!(store.promote(&id, Some(".."), &voices).is_err());
    }

    /// `speech_ms` is documented as the same measure everywhere: the
    /// transcript field, the `min_embed_ms` comparison and the promotion
    /// total. Truncating each observation to whole seconds broke that —
    /// two 1.9 s utterances counted as 2 s of speech, not 3.8 s, so a
    /// voice made of short utterances could never reach the threshold.
    #[test]
    fn sub_second_observations_accumulate_rather_than_truncating() {
        let (_tmp, mut store) = store();
        let id = store
            .enrol(vec![1.0, 0.0], &vec![0i16; 16_000], day(26), 1_900, "a")
            .unwrap();
        store
            .observe(&id, &[1.0, 0.0], day(27), 1_900, "b")
            .unwrap();
        assert!(
            store.is_promotable(&id, 3, 2),
            "1.9s + 1.9s is 3.8s of speech, not 2s"
        );
    }

    /// `observations` defaulting to 0 makes the very next running-mean
    /// update `(centroid * 0 + x) / 1` — the whole accumulated centroid is
    /// discarded and replaced by one observation.
    #[test]
    fn a_stats_file_with_no_observation_count_still_weights_the_existing_centroid() {
        let (tmp, mut store) = store();
        let id = store
            .enrol(vec![1.0, 0.0], &vec![0i16; 16_000], day(26), 10_000, "a")
            .unwrap();
        // Simulate a stats file written before `observations` existed.
        let stats_path = tmp.path().join("candidates").join(&id).join("stats.json");
        let mut v: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&stats_path).unwrap()).unwrap();
        v.as_object_mut().unwrap().remove("observations");
        std::fs::write(&stats_path, serde_json::to_vec_pretty(&v).unwrap()).unwrap();

        let mut reloaded =
            CandidateStore::open(tmp.path().join("candidates"), "test-model".into()).unwrap();
        reloaded
            .observe(&id, &[0.0, 1.0], day(26), 10_000, "b")
            .unwrap();
        let c = reloaded.get(&id).unwrap();
        assert_eq!(
            c.centroid,
            vec![0.5, 0.5],
            "an absent count must mean one prior observation, not zero"
        );
    }

    /// `speaker_promote(id, name="me")` is exactly what the model does
    /// when the user says "that was me". A fixed `clip.wav` filename made
    /// that a destructive, model-invoked overwrite of curated reference
    /// audio in the user's workspace. Naming the copy after the grain-id
    /// turns merging into an existing speaker into correct behaviour: the
    /// registry averages every `*.wav` in the directory.
    #[test]
    fn promotion_into_an_existing_speaker_keeps_its_curated_audio() {
        let (tmp, mut store) = store();
        let voices = tmp.path().join("voices");
        let curated = voices.join("me");
        std::fs::create_dir_all(&curated).unwrap();
        write_wav(&curated.join("clip.wav"), &vec![1234i16; 16_000]).unwrap();
        let before = std::fs::read(curated.join("clip.wav")).unwrap();

        let id = store
            .enrol(vec![1.0, 0.0], &vec![0i16; 16_000], day(26), 60_000, "a")
            .unwrap();
        store.promote(&id, Some("me"), &voices).unwrap();

        // `assert!` rather than `assert_eq!` on purpose: a failure here
        // would otherwise dump both 32 kB WAV bodies into the test log.
        assert!(
            std::fs::read(curated.join("clip.wav")).unwrap() == before,
            "curated reference audio must survive a promotion into the same speaker"
        );
        assert!(
            curated.join(format!("{id}.wav")).exists(),
            "the promoted clip lands under its own grain-id"
        );
    }

    /// The workspace directory name is the *display* name; the id must be
    /// stable across a rename, so promotion records it in a marker file.
    #[test]
    fn promotion_writes_an_id_marker_holding_the_grain_id() {
        let (tmp, mut store) = store();
        let voices = tmp.path().join("voices");
        let id = store
            .enrol(vec![1.0, 0.0], &vec![0i16; 16_000], day(26), 60_000, "a")
            .unwrap();
        store.promote(&id, Some("tanaka-san"), &voices).unwrap();
        let marker = std::fs::read_to_string(voices.join("tanaka-san").join("id")).unwrap();
        assert_eq!(marker.lines().next(), Some(id.as_str()));
    }

    /// Merging into a hand-made speaker directory must not silently change
    /// that speaker's id: `voices/me/` had no marker, so its id was "me",
    /// and every transcript already written says "me".
    #[test]
    fn merging_into_a_hand_made_speaker_keeps_its_directory_name_as_the_canonical_id() {
        let (tmp, mut store) = store();
        let voices = tmp.path().join("voices");
        let curated = voices.join("me");
        std::fs::create_dir_all(&curated).unwrap();
        write_wav(&curated.join("clip.wav"), &vec![7i16; 16_000]).unwrap();

        let id = store
            .enrol(vec![1.0, 0.0], &vec![0i16; 16_000], day(26), 60_000, "a")
            .unwrap();
        store.promote(&id, Some("me"), &voices).unwrap();

        let marker = std::fs::read_to_string(curated.join("id")).unwrap();
        let ids: Vec<&str> = marker.lines().collect();
        assert_eq!(
            ids.first(),
            Some(&"me"),
            "the pre-existing id stays canonical"
        );
        assert!(
            ids.contains(&id.as_str()),
            "the merged grain-id is an alias"
        );
    }

    /// `promote` creates the target directory before copying the clip into
    /// it. If the copy fails, that directory must not survive — a stray,
    /// empty `voices/<name>/` would be a partial workspace write ahead of
    /// a promotion that never actually happened.
    #[test]
    fn promotion_removes_a_freshly_created_directory_when_the_copy_fails() {
        let (tmp, mut store) = store();
        let voices = tmp.path().join("voices");
        let id = store
            .enrol(vec![1.0, 0.0], &vec![0i16; 16_000], day(26), 60_000, "a")
            .unwrap();
        // Knock out the candidate's own clip so the copy inside `promote`
        // fails partway through.
        std::fs::remove_file(store.dir.join(&id).join("clip.wav")).unwrap();

        assert!(store.promote(&id, Some("ghost"), &voices).is_err());
        assert!(
            !voices.join("ghost").exists(),
            "a failed promotion must not leave a stray directory behind"
        );
    }
}
