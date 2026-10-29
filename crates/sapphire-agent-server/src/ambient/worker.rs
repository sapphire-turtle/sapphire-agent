//! The ambient processing pipeline.
//!
//! Re-gate, transcribe, attribute, store. **Nothing here starts an LLM
//! turn** — that is the whole distinction between `ambient` and `voice`,
//! and the [`Disposition::RecordAndConverse`] arm is deliberately left
//! unreachable until S4.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use chrono::Local;
use tokio::sync::mpsc;
use tracing::{info, warn};

use super::audio::{SpeakerEmbedder, SpeechGate};
use super::cache::AudioCache;
use super::ingest::Segment;
use super::router::{DeviceState, Disposition, route};
use super::speaker::candidates::CandidateStore;
use super::speaker::registry::SpeakerRegistry;
use super::transcript::{TranscriptRecord, TranscriptStore};
use crate::session::local_date_for_timestamp;
use crate::voice::SttProvider;

pub struct Worker {
    pub gate: Box<dyn SpeechGate>,
    pub embedder: Box<dyn SpeakerEmbedder>,
    pub stt: Arc<dyn SttProvider>,
    pub registry: SpeakerRegistry,
    /// Shared, not owned: a later task exposes promotion as an agent
    /// tool, and the tool and the worker must see the same store. Two
    /// instances over one directory would diverge in memory.
    pub candidates: Arc<Mutex<CandidateStore>>,
    pub cache: Arc<AudioCache>,
    pub transcripts: TranscriptStore,
    pub voices_dir: PathBuf,
    pub min_embed_ms: u32,
    pub promote_after_seconds: u32,
    pub promote_after_days: u32,
    pub day_boundary_hour: u8,
    pub language: Option<String>,
}

impl Worker {
    /// Process one segment. Returns the stored record, or `None` when the
    /// segment held no speech.
    pub async fn process(&mut self, seg: Segment) -> Result<Option<TranscriptRecord>> {
        // Every device is pinned to Idle until S4 gives the state machine
        // something to change it with.
        match route(&seg, DeviceState::Idle) {
            Disposition::RecordOnly => {}
            Disposition::RecordAndConverse => {
                warn!("ambient: conversation disposition is not implemented; recording only");
            }
        }

        let Some(gated) = self.gate.gate(&seg.pcm) else {
            info!("ambient: segment {} held no speech; dropped", seg.segment);
            return Ok(None);
        };

        let audio_sha = self.cache.put(&pcm_to_bytes(&gated.pcm))?;
        let text = self
            .stt
            .transcribe(&gated.pcm, self.language.as_deref())
            .await?;

        let day =
            local_date_for_timestamp(seg.started_at.with_timezone(&Local), self.day_boundary_hour);

        let (speaker, speaker_score) = if gated.speech_ms < self.min_embed_ms {
            // Embeddings from very short utterances are unreliable, and
            // trusting them is the main driver of speaker-id inflation.
            (None, None)
        } else {
            match self.embedder.embed(&gated.pcm) {
                Ok(embedding) => {
                    self.attribute(&embedding, &gated.pcm, day, gated.speech_ms, &text)?
                }
                Err(e) => {
                    warn!("ambient: embedding failed for {}: {e}", seg.segment);
                    (None, None)
                }
            }
        };

        let record = TranscriptRecord {
            segment: seg.segment,
            device: seg.device,
            started_at: seg.started_at,
            speech_ms: gated.speech_ms,
            speaker,
            speaker_score,
            text,
            audio: audio_sha,
        };
        self.transcripts.append(&record)?;
        Ok(Some(record))
    }

    /// Match, or enrol on a miss so the same voice matches next time.
    fn attribute(
        &mut self,
        embedding: &[f32],
        pcm: &[i16],
        day: chrono::NaiveDate,
        speech_ms: u32,
        text: &str,
    ) -> Result<(Option<String>, Option<f32>)> {
        if let Some(m) = self.registry.match_speaker(embedding) {
            // A match may be a candidate rather than a registered speaker;
            // observing keeps its statistics and centroid current. The
            // existence check and the observe must share one lock
            // acquisition: this store is also held by the promotion tool
            // (a later task), and releasing the lock between "is this a
            // candidate" and "observe it" would let a promotion land in
            // the gap, so `observe` would find the id already gone.
            let updated = {
                let mut candidates = self.candidates.lock().expect("candidate store poisoned");
                if candidates.get(&m.id).is_some() {
                    match candidates.observe(&m.id, embedding, day, speech_ms, text) {
                        Ok(()) => candidates.get(&m.id).map(|c| c.centroid.clone()),
                        Err(e) => {
                            // The transcript is already committed to being
                            // written (audio is cached, STT has run); losing
                            // the attribution update is recoverable in a way
                            // that losing the transcript is not, so this
                            // degrades rather than propagating with `?`.
                            warn!("ambient: could not update candidate {}: {e}", m.id);
                            None
                        }
                    }
                } else {
                    None
                }
            };
            if let Some(centroid) = updated {
                // `observe` moved the running mean on disk and in the
                // candidate store. The matcher holds its own copy, so
                // without this it would keep attributing against the
                // first-sighting vector for the life of the process — and
                // this is a daemon, so that may be weeks.
                self.registry.add_runtime(m.id.clone(), centroid);
                self.maybe_promote(&m.id)?;
            }
            return Ok((Some(m.id), Some(m.score)));
        }

        let id = {
            let mut candidates = self.candidates.lock().expect("candidate store poisoned");
            candidates.enrol(embedding.to_vec(), pcm, day, speech_ms, text)?
        };
        self.registry.add_runtime(id.clone(), embedding.to_vec());
        info!("ambient: enrolled a new voice as {id}");
        self.maybe_promote(&id)?;
        Ok((Some(id), None))
    }

    fn maybe_promote(&mut self, id: &str) -> Result<()> {
        let mut candidates = self.candidates.lock().expect("candidate store poisoned");
        if !candidates.is_promotable(id, self.promote_after_seconds, self.promote_after_days) {
            return Ok(());
        }
        match candidates.promote(id, None, &self.voices_dir) {
            // The rename really is safe: `promote` writes the grain-id into
            // the directory's `id` marker, so the directory name is only a
            // display name from here on and every transcript already
            // written keeps resolving to it.
            Ok(name) => info!(
                "ambient: promoted candidate {id} to voices/{name}; rename that directory to \
                 finish registering — the id is recorded inside it, so past transcripts follow"
            ),
            Err(e) => warn!("ambient: could not promote {id}: {e}"),
        }
        Ok(())
    }
}

/// Drain the admission queue forever. One segment at a time on purpose:
/// enrolment mutates shared speaker state, and a day of spooled audio
/// arriving at once is a throughput problem, not a latency one.
pub async fn run(mut worker: Worker, mut rx: mpsc::Receiver<Segment>) {
    while let Some(seg) = rx.recv().await {
        let id = seg.segment.clone();
        if let Err(e) = worker.process(seg).await {
            // One bad segment must never stop the pipeline; the audio stays
            // in the cache either way.
            warn!("ambient: segment {id} failed: {e}");
        }
    }
    info!("ambient: admission queue closed; worker stopping");
}

fn pcm_to_bytes(pcm: &[i16]) -> Vec<u8> {
    pcm.iter().flat_map(|s| s.to_le_bytes()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ambient::audio::{FixedEmbedder, PassthroughGate, SilentGate};
    use crate::ambient::speaker::registry::SpeakerRegistry;
    use chrono::Utc;

    fn seg(pcm_len: usize, live: bool) -> Segment {
        Segment {
            segment: format!("seg-{pcm_len}-{live}"),
            device: "pendant".into(),
            started_at: Utc::now(),
            live,
            pcm: vec![0; pcm_len],
        }
    }

    struct Harness {
        _tmp: tempfile::TempDir,
        worker: Worker,
    }

    fn harness(
        gate: Box<dyn crate::ambient::audio::SpeechGate>,
        embedder: Box<dyn crate::ambient::audio::SpeakerEmbedder>,
        registered: &[(&str, Vec<f32>)],
    ) -> Harness {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let cache = crate::ambient::cache::AudioCache::open(root.join("audio")).unwrap();
        let transcripts =
            crate::ambient::transcript::TranscriptStore::open(root.join("transcripts"), 4).unwrap();
        let mut registry = SpeakerRegistry::open(
            root.join("voices"),
            root.join("emb"),
            "test-model".into(),
            0.55,
        )
        .unwrap();
        for (id, v) in registered {
            registry.add_runtime(id.to_string(), v.clone());
        }
        let candidates = Arc::new(Mutex::new(
            crate::ambient::speaker::candidates::CandidateStore::open(
                root.join("candidates"),
                "test-model".into(),
            )
            .unwrap(),
        ));
        let stt = std::sync::Arc::new(crate::voice::MockStt::new(
            "mock".into(),
            "transcribed text".into(),
        ));
        let worker = Worker {
            gate,
            embedder,
            stt,
            registry,
            candidates,
            cache,
            transcripts,
            voices_dir: root.join("voices"),
            min_embed_ms: 1500,
            promote_after_seconds: 60,
            promote_after_days: 2,
            day_boundary_hour: 4,
            language: None,
        };
        Harness { _tmp: tmp, worker }
    }

    #[tokio::test]
    async fn a_silence_only_segment_produces_no_transcript() {
        let mut h = harness(
            Box::new(SilentGate),
            Box::new(FixedEmbedder::new(vec![1.0, 0.0])),
            &[],
        );
        let out = h.worker.process(seg(16_000, false)).await.unwrap();
        assert!(out.is_none(), "re-gate found no speech");
    }

    #[tokio::test]
    async fn a_registered_speaker_is_named_on_the_transcript() {
        let mut h = harness(
            Box::new(PassthroughGate::new()),
            Box::new(FixedEmbedder::new(vec![1.0, 0.0])),
            &[("me", vec![1.0, 0.0])],
        );
        let rec = h.worker.process(seg(32_000, true)).await.unwrap().unwrap();
        assert_eq!(rec.speaker.as_deref(), Some("me"));
        assert!(rec.speaker_score.unwrap() > 0.99);
        assert_eq!(rec.text, "transcribed text");
        assert_eq!(rec.speech_ms, 2000);
        assert_eq!(rec.device, "pendant");
    }

    #[tokio::test]
    async fn a_short_segment_gets_a_transcript_but_no_speaker() {
        let mut h = harness(
            Box::new(PassthroughGate::new()),
            Box::new(FixedEmbedder::new(vec![1.0, 0.0])),
            &[("me", vec![1.0, 0.0])],
        );
        // 16000 samples = 1000 ms, below min_embed_ms of 1500.
        let rec = h.worker.process(seg(16_000, true)).await.unwrap().unwrap();
        assert_eq!(rec.speaker, None, "too short to attribute reliably");
        assert_eq!(rec.speaker_score, None);
        assert_eq!(rec.text, "transcribed text");
    }

    #[tokio::test]
    async fn a_segment_exactly_at_min_embed_ms_gets_a_speaker() {
        let mut h = harness(
            Box::new(PassthroughGate::new()),
            Box::new(FixedEmbedder::new(vec![1.0, 0.0])),
            &[("me", vec![1.0, 0.0])],
        );
        // 24000 samples = 1500 ms, exactly min_embed_ms: the comparison is
        // `<`, so this boundary must still be attributed, not excluded.
        let rec = h.worker.process(seg(24_000, true)).await.unwrap().unwrap();
        assert_eq!(rec.speech_ms, 1500);
        assert_eq!(
            rec.speaker.as_deref(),
            Some("me"),
            "at the boundary, not below it"
        );
    }

    #[tokio::test]
    async fn an_unmatched_voice_is_enrolled_and_matches_on_its_next_segment() {
        let mut h = harness(
            Box::new(PassthroughGate::new()),
            Box::new(FixedEmbedder::new(vec![0.0, 1.0])),
            &[("me", vec![1.0, 0.0])],
        );
        let first = h.worker.process(seg(32_000, true)).await.unwrap().unwrap();
        let enrolled = first.speaker.expect("enrolled on first sight");
        assert_ne!(enrolled, "me");
        assert_eq!(h.worker.candidates.lock().unwrap().list().len(), 1);

        let mut next = seg(32_000, true);
        next.segment = "seg-second".into();
        let second = h.worker.process(next).await.unwrap().unwrap();
        assert_eq!(
            second.speaker.as_deref(),
            Some(enrolled.as_str()),
            "same voice, same id — this is what gives cross-day stability"
        );
        assert_eq!(
            h.worker.candidates.lock().unwrap().list().len(),
            1,
            "no second candidate"
        );
    }

    /// Returns a different vector per call, so a candidate's running-mean
    /// centroid actually moves between observations. `FixedEmbedder`
    /// cannot show this: with a constant vector the mean never changes.
    struct SequenceEmbedder {
        vectors: Vec<Vec<f32>>,
        next: Mutex<usize>,
    }

    impl SequenceEmbedder {
        fn new(vectors: Vec<Vec<f32>>) -> Self {
            Self {
                vectors,
                next: Mutex::new(0),
            }
        }
    }

    impl crate::ambient::audio::SpeakerEmbedder for SequenceEmbedder {
        fn embed(&self, _pcm: &[i16]) -> Result<Vec<f32>> {
            let mut n = self.next.lock().unwrap();
            let i = (*n).min(self.vectors.len() - 1);
            *n += 1;
            Ok(self.vectors[i].clone())
        }
    }

    /// `observe` updates the candidate store, on disk and in memory. It
    /// must also update the *matcher*, or the process keeps attributing
    /// against the first-sighting vector for its whole lifetime — which,
    /// for a daemon, can be weeks.
    #[tokio::test]
    async fn an_observation_updates_the_matcher_not_just_the_candidate_store() {
        let mut h = harness(
            Box::new(PassthroughGate::new()),
            Box::new(SequenceEmbedder::new(vec![vec![1.0, 0.0], vec![1.0, 1.0]])),
            &[],
        );
        let first = h.worker.process(seg(32_000, true)).await.unwrap().unwrap();
        let id = first.speaker.expect("enrolled on first sight");

        let mut next = seg(32_000, true);
        next.segment = "seg-second".into();
        let second = h.worker.process(next).await.unwrap().unwrap();
        assert_eq!(second.speaker.as_deref(), Some(id.as_str()));

        // The store now holds the mean of [1,0] and [1,1], i.e. [1,0.5].
        let stored = h
            .worker
            .candidates
            .lock()
            .unwrap()
            .get(&id)
            .unwrap()
            .centroid
            .clone();
        assert_eq!(stored, vec![1.0, 0.5]);

        // The matcher must agree. Against the stale [1,0] this scores
        // ~0.894; against the updated centroid it scores 1.0.
        let m = h
            .worker
            .registry
            .match_speaker(&[1.0, 0.5])
            .expect("still matches");
        assert!(
            m.score > 0.999,
            "matcher is still using the first-sighting vector (score {})",
            m.score
        );
    }

    #[tokio::test]
    async fn the_audio_blob_is_cached_and_referenced_by_the_transcript() {
        let mut h = harness(
            Box::new(PassthroughGate::new()),
            Box::new(FixedEmbedder::new(vec![1.0, 0.0])),
            &[("me", vec![1.0, 0.0])],
        );
        let rec = h.worker.process(seg(32_000, true)).await.unwrap().unwrap();
        assert_eq!(rec.audio.len(), 64);
        assert!(h.worker.cache.get(&rec.audio).is_some());
    }

    #[tokio::test]
    async fn a_candidate_clearing_both_thresholds_is_promoted_into_the_workspace() {
        let mut h = harness(
            Box::new(PassthroughGate::new()),
            Box::new(FixedEmbedder::new(vec![0.0, 1.0])),
            &[],
        );
        // One long segment on day one, one on day two.
        let mut a = seg(16_000 * 40, true); // 40 s
        a.started_at = chrono::Utc::now() - chrono::Duration::days(1);
        a.segment = "day-one".into();
        h.worker.process(a).await.unwrap();

        let mut b = seg(16_000 * 40, true); // another 40 s
        b.segment = "day-two".into();
        h.worker.process(b).await.unwrap();

        let voices = h.worker.voices_dir.clone();
        let promoted: Vec<_> = std::fs::read_dir(&voices)
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert_eq!(
            promoted.len(),
            1,
            "80s over two days clears both thresholds"
        );
        // The clip is named after the grain-id (so merging into an
        // existing speaker never overwrites curated audio), and the
        // grain-id is also written into the directory's `id` marker so the
        // user can rename the directory freely.
        let dir = promoted[0].path();
        let name = dir.file_name().unwrap().to_string_lossy().into_owned();
        assert!(dir.join(format!("{name}.wav")).exists());
        assert_eq!(
            std::fs::read_to_string(dir.join("id"))
                .unwrap()
                .trim()
                .to_string(),
            name
        );
    }
}
