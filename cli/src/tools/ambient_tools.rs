//! Tools that let the agent read ambient transcripts and name speakers.
//!
//! This is the S1/S2 boundary: the daily summarisation in S2 is an LLM turn
//! that calls `transcript_read` and writes to the journal over MCP.
//!
//! `speaker_promote` takes an optional name so registering a speaker is a
//! sentence in chat -- "that was Tanaka-san" -- rather than a file
//! operation. Transcripts store the speaker id, not a display name, so a
//! promotion (or a later rename of the resulting `voices/<name>/`
//! directory) applies retroactively to everything already recorded.
//!
//! That last property is real rather than aspirational because the id and
//! the directory name are separate things: promotion writes the candidate's
//! grain-id into an `id` marker inside the directory, and
//! [`crate::ambient::speaker::registry::SpeakerNames`] resolves ids back to
//! whatever the directory is called *now*. `transcript_read` does that scan
//! on every call, so a rename takes effect without restarting the daemon.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::{Value, json};

use crate::ambient::speaker::candidates::CandidateStore;
use crate::ambient::speaker::registry::SpeakerNames;
use crate::ambient::transcript::TranscriptStore;
use crate::provider::ToolSpec;
use crate::tools::{Tool, ToolKind};

pub struct AmbientToolState {
    pub transcripts: TranscriptStore,
    /// Shared, not owned: the ambient worker (Task 10) holds the same
    /// `Arc<Mutex<CandidateStore>>`. Two instances over one directory
    /// would diverge in memory -- promoting through this tool must
    /// actually remove the candidate the worker is matching against, or
    /// the worker keeps re-matching that voice and re-promoting it.
    pub candidates: Arc<Mutex<CandidateStore>>,
    pub voices_dir: PathBuf,
}

fn parse_time(v: &Value, field: &str) -> Result<DateTime<Utc>> {
    let raw = v[field]
        .as_str()
        .with_context(|| format!("missing '{field}' (RFC 3339)"))?;
    Ok(DateTime::parse_from_rfc3339(raw)
        .with_context(|| format!("'{field}' must be RFC 3339, got {raw:?}"))?
        .with_timezone(&Utc))
}

// -- transcript_read ---------------------------------------------------------

pub struct TranscriptReadTool {
    state: Arc<Mutex<AmbientToolState>>,
    spec: ToolSpec,
}

impl TranscriptReadTool {
    pub fn new(state: Arc<Mutex<AmbientToolState>>) -> Self {
        Self {
            state,
            spec: ToolSpec {
                name: "transcript_read".into(),
                description: "Read ambient (always-on microphone) transcripts for a time \
                              window, optionally filtered to one speaker. Speakers are \
                              shown by their current name; names that look like random \
                              words are auto-enrolled voices nobody has named yet."
                    .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "from": { "type": "string", "description": "Start of the window, RFC 3339." },
                        "to": { "type": "string", "description": "End of the window, RFC 3339." },
                        "speaker": { "type": "string", "description": "Optional speaker to filter to: either a name from voices/ or a raw id." }
                    },
                    "required": ["from", "to"]
                }),
            },
        }
    }
}

#[async_trait]
impl Tool for TranscriptReadTool {
    fn kind(&self) -> ToolKind {
        ToolKind::Read
    }

    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn execute(&self, input: &Value) -> Result<String> {
        let from = parse_time(input, "from")?;
        let to = parse_time(input, "to")?;
        let speaker = input["speaker"].as_str();
        // Scanned per call, not cached at startup: renaming a `voices/`
        // directory is the documented way to name a speaker, and it must
        // take effect on the next read rather than the next restart.
        let (names, records) = {
            let st = self.state.lock().expect("ambient tool state poisoned");
            let names = SpeakerNames::scan(&st.voices_dir);
            // A filter may name a person or give a raw id; one person can
            // own several ids once a candidate has been merged in.
            let ids = speaker.map(|s| names.ids_for(s));
            let records = st.transcripts.read(from, to, ids.as_deref())?;
            (names, records)
        };
        if records.is_empty() {
            return Ok("No ambient transcript in that window.".into());
        }
        let mut out = String::new();
        for r in &records {
            let who = match r.speaker.as_deref() {
                Some(id) => names.display_name(id),
                None => "unattributed".to_string(),
            };
            out.push_str(&format!(
                "[{}] {}: {}\n",
                r.started_at.to_rfc3339(),
                who,
                r.text
            ));
        }
        Ok(out)
    }
}

// -- speaker_candidates ------------------------------------------------------

pub struct SpeakerCandidatesTool {
    state: Arc<Mutex<AmbientToolState>>,
    spec: ToolSpec,
}

impl SpeakerCandidatesTool {
    pub fn new(state: Arc<Mutex<AmbientToolState>>) -> Self {
        Self {
            state,
            spec: ToolSpec {
                name: "speaker_candidates".into(),
                description: "List auto-enrolled voices that have not been named yet, with \
                              how much they have spoken and what they said. Use with \
                              speaker_promote to give one a name."
                    .into(),
                input_schema: json!({ "type": "object", "properties": {} }),
            },
        }
    }
}

#[async_trait]
impl Tool for SpeakerCandidatesTool {
    fn kind(&self) -> ToolKind {
        ToolKind::Search
    }

    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn execute(&self, _input: &Value) -> Result<String> {
        let out = {
            let st = self.state.lock().expect("ambient tool state poisoned");
            let candidates = st.candidates.lock().expect("candidate store poisoned");
            let list = candidates.list();
            if list.is_empty() {
                return Ok("No unnamed speakers.".into());
            }
            let mut out = String::new();
            for c in list {
                out.push_str(&format!(
                    "{} -- {}s across {} day(s), first heard {}\n",
                    c.id,
                    c.stats.speech_ms / 1000,
                    c.stats.days_seen.len(),
                    c.stats.first_seen.to_rfc3339()
                ));
                for s in &c.stats.samples {
                    out.push_str(&format!("    \"{s}\"\n"));
                }
            }
            out
        };
        Ok(out)
    }
}

// -- speaker_promote ---------------------------------------------------------

pub struct SpeakerPromoteTool {
    state: Arc<Mutex<AmbientToolState>>,
    spec: ToolSpec,
}

impl SpeakerPromoteTool {
    pub fn new(state: Arc<Mutex<AmbientToolState>>) -> Self {
        Self {
            state,
            spec: ToolSpec {
                name: "speaker_promote".into(),
                description: "Register an auto-enrolled voice in the workspace, optionally \
                              naming it at the same time -- e.g. after the user says 'that \
                              was Tanaka-san'. Past transcripts pick the name up \
                              retroactively, because they store the speaker id and the name \
                              is resolved when they are read; renaming the directory later \
                              works the same way. Naming an existing speaker merges this \
                              voice into them and adds to their reference audio rather than \
                              replacing it. This is a manual escape hatch: it does not check \
                              whether the voice has cleared the automatic promotion \
                              thresholds."
                    .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "id": { "type": "string", "description": "Candidate id from speaker_candidates." },
                        "name": { "type": "string", "description": "Optional display name, e.g. a person's name." }
                    },
                    "required": ["id"]
                }),
            },
        }
    }
}

#[async_trait]
impl Tool for SpeakerPromoteTool {
    fn kind(&self) -> ToolKind {
        ToolKind::Edit
    }

    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn execute(&self, input: &Value) -> Result<String> {
        let id = input["id"].as_str().context("missing 'id'")?.to_string();
        let name = input["name"].as_str().map(str::to_string);
        let dir = {
            let st = self.state.lock().expect("ambient tool state poisoned");
            let voices_dir = st.voices_dir.clone();
            let mut candidates = st.candidates.lock().expect("candidate store poisoned");
            // Deliberately no `is_promotable` gate here: that threshold
            // check belongs to the worker's automatic path. This tool is
            // the manual override for naming someone who has not (yet)
            // cleared it.
            candidates.promote(&id, name.as_deref(), &voices_dir)?
        };
        Ok(format!(
            "Registered {id} as voices/{dir}. Rename that directory any time; the id is \
             recorded inside it, so transcripts follow automatically."
        ))
    }
}

pub fn ambient_tools(state: Arc<Mutex<AmbientToolState>>) -> Vec<Box<dyn Tool>> {
    vec![
        Box::new(TranscriptReadTool::new(Arc::clone(&state))),
        Box::new(SpeakerCandidatesTool::new(Arc::clone(&state))),
        Box::new(SpeakerPromoteTool::new(state)),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use serde_json::json;

    fn state() -> (tempfile::TempDir, Arc<Mutex<AmbientToolState>>) {
        let tmp = tempfile::tempdir().unwrap();
        let transcripts =
            crate::ambient::transcript::TranscriptStore::open(tmp.path().join("t"), 4).unwrap();
        let candidates = crate::ambient::speaker::candidates::CandidateStore::open(
            tmp.path().join("c"),
            "test-model".into(),
        )
        .unwrap();
        let st = AmbientToolState {
            transcripts,
            candidates: Arc::new(Mutex::new(candidates)),
            voices_dir: tmp.path().join("voices"),
        };
        (tmp, Arc::new(Mutex::new(st)))
    }

    #[tokio::test]
    async fn transcript_read_returns_records_in_the_requested_window() {
        let (_tmp, st) = state();
        {
            let s = st.lock().unwrap();
            for (h, speaker, text) in [(9, "me", "morning"), (14, "tanaka-san", "afternoon")] {
                s.transcripts
                    .append(&crate::ambient::transcript::TranscriptRecord {
                        segment: format!("seg-{h}"),
                        device: "pendant".into(),
                        started_at: Utc.with_ymd_and_hms(2026, 8, 26, h, 0, 0).unwrap(),
                        speech_ms: 3000,
                        speaker: Some(speaker.into()),
                        speaker_score: Some(0.9),
                        text: text.into(),
                        audio: "a".repeat(64),
                    })
                    .unwrap();
            }
        }
        let tool = TranscriptReadTool::new(Arc::clone(&st));
        let out = tool
            .execute(&json!({
                "from": "2026-08-26T08:00:00Z",
                "to": "2026-08-26T12:00:00Z"
            }))
            .await
            .unwrap();
        assert!(out.contains("morning"));
        assert!(!out.contains("afternoon"), "outside the window");
    }

    #[tokio::test]
    async fn transcript_read_can_filter_by_speaker() {
        let (_tmp, st) = state();
        {
            let s = st.lock().unwrap();
            for (h, speaker, text) in [(9, "me", "mine"), (10, "tanaka-san", "theirs")] {
                s.transcripts
                    .append(&crate::ambient::transcript::TranscriptRecord {
                        segment: format!("seg-{h}"),
                        device: "pendant".into(),
                        started_at: Utc.with_ymd_and_hms(2026, 8, 26, h, 0, 0).unwrap(),
                        speech_ms: 3000,
                        speaker: Some(speaker.into()),
                        speaker_score: Some(0.9),
                        text: text.into(),
                        audio: "a".repeat(64),
                    })
                    .unwrap();
            }
        }
        let tool = TranscriptReadTool::new(Arc::clone(&st));
        let out = tool
            .execute(&json!({
                "from": "2026-08-26T00:00:00Z",
                "to": "2026-08-26T23:00:00Z",
                "speaker": "tanaka-san"
            }))
            .await
            .unwrap();
        assert!(out.contains("theirs"));
        assert!(!out.contains("mine"));
    }

    #[tokio::test]
    async fn speaker_candidates_lists_what_is_awaiting_a_name() {
        let (_tmp, st) = state();
        let id = {
            let s = st.lock().unwrap();
            let mut candidates = s.candidates.lock().unwrap();
            candidates
                .enrol(
                    vec![1.0, 0.0],
                    &vec![0i16; 16_000],
                    chrono::NaiveDate::from_ymd_opt(2026, 8, 26).unwrap(),
                    30_000,
                    "something they said",
                )
                .unwrap()
        };
        let tool = SpeakerCandidatesTool::new(Arc::clone(&st));
        let out = tool.execute(&json!({})).await.unwrap();
        assert!(out.contains(&id));
        assert!(
            out.contains("something they said"),
            "samples aid recognition"
        );
        assert!(out.contains("30"), "cumulative seconds shown");
    }

    #[tokio::test]
    async fn speaker_promote_names_a_candidate_in_one_call() {
        let (_tmp, st) = state();
        let id = {
            let s = st.lock().unwrap();
            let mut candidates = s.candidates.lock().unwrap();
            candidates
                .enrol(
                    vec![1.0, 0.0],
                    &vec![0i16; 16_000],
                    chrono::NaiveDate::from_ymd_opt(2026, 8, 26).unwrap(),
                    30_000,
                    "hi",
                )
                .unwrap()
        };
        let tool = SpeakerPromoteTool::new(Arc::clone(&st));
        let out = tool
            .execute(&json!({ "id": id, "name": "tanaka-san" }))
            .await
            .unwrap();
        assert!(out.contains("tanaka-san"));

        let s = st.lock().unwrap();
        assert!(
            s.voices_dir
                .join("tanaka-san")
                .join(format!("{id}.wav"))
                .exists()
        );
        assert!(s.candidates.lock().unwrap().list().is_empty());
    }

    /// The spec's "rename transparency" test case, and the promise three
    /// user-facing strings make: renaming `voices/blithe-otter-42/` to
    /// `voices/tanaka-san/` must make every past transcript read back
    /// under the new name, with no rewrite pass over the transcripts.
    ///
    /// This only works if the id is stored independently of the directory
    /// name. When the directory name *is* the id, renaming the directory
    /// changes the id, and everything recorded before the rename becomes
    /// unreachable under the new name.
    #[tokio::test]
    async fn renaming_a_promoted_speaker_directory_renames_it_in_past_transcripts() {
        let (_tmp, st) = state();
        let id = {
            let s = st.lock().unwrap();
            let mut candidates = s.candidates.lock().unwrap();
            candidates
                .enrol(
                    vec![1.0, 0.0],
                    &vec![0i16; 16_000],
                    chrono::NaiveDate::from_ymd_opt(2026, 8, 26).unwrap(),
                    30_000,
                    "hi",
                )
                .unwrap()
        };

        // Promoted with the auto-generated name, exactly as the worker's
        // automatic path does it.
        let promote = SpeakerPromoteTool::new(Arc::clone(&st));
        promote.execute(&json!({ "id": id })).await.unwrap();

        // A transcript recorded *before* the rename.
        let voices_dir = {
            let s = st.lock().unwrap();
            s.transcripts
                .append(&crate::ambient::transcript::TranscriptRecord {
                    segment: "seg-pre-rename".into(),
                    device: "pendant".into(),
                    started_at: Utc.with_ymd_and_hms(2026, 8, 26, 9, 0, 0).unwrap(),
                    speech_ms: 3000,
                    speaker: Some(id.clone()),
                    speaker_score: Some(0.9),
                    text: "before the rename".into(),
                    audio: "a".repeat(64),
                })
                .unwrap();
            s.voices_dir.clone()
        };

        std::fs::rename(voices_dir.join(&id), voices_dir.join("tanaka-san")).unwrap();

        let read = TranscriptReadTool::new(Arc::clone(&st));
        let out = read
            .execute(&json!({
                "from": "2026-08-26T00:00:00Z",
                "to": "2026-08-26T23:00:00Z"
            }))
            .await
            .unwrap();
        assert!(
            out.contains("tanaka-san"),
            "a transcript written before the rename must read back under the new name, got: {out}"
        );
        assert!(
            !out.contains(&id),
            "the raw id must not leak into the rendered transcript, got: {out}"
        );

        // And the filter takes the new name, resolving it back to the id
        // the transcript actually stores.
        let filtered = read
            .execute(&json!({
                "from": "2026-08-26T00:00:00Z",
                "to": "2026-08-26T23:00:00Z",
                "speaker": "tanaka-san"
            }))
            .await
            .unwrap();
        assert!(
            filtered.contains("before the rename"),
            "filtering by the new name must find pre-rename records, got: {filtered}"
        );
    }

    #[tokio::test]
    async fn speaker_promote_rejects_an_unknown_id() {
        let (_tmp, st) = state();
        let tool = SpeakerPromoteTool::new(Arc::clone(&st));
        assert!(tool.execute(&json!({ "id": "nope" })).await.is_err());
    }

    /// Not just "returns an error" -- the message must name what was
    /// actually wrong, so the agent can retry sensibly rather than just
    /// seeing an opaque failure.
    #[tokio::test]
    async fn speaker_promote_surfaces_a_useful_error_for_an_unsafe_name() {
        let (_tmp, st) = state();
        let id = {
            let s = st.lock().unwrap();
            let mut candidates = s.candidates.lock().unwrap();
            candidates
                .enrol(
                    vec![1.0, 0.0],
                    &vec![0i16; 16_000],
                    chrono::NaiveDate::from_ymd_opt(2026, 8, 26).unwrap(),
                    30_000,
                    "hi",
                )
                .unwrap()
        };
        let tool = SpeakerPromoteTool::new(Arc::clone(&st));
        let err = tool
            .execute(&json!({ "id": id, "name": "../../etc/passwd" }))
            .await
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("escape") || msg.contains("path segment"),
            "error should explain the name was rejected, got: {msg}"
        );

        // The candidate must still be there to retry with a safer name.
        let s = st.lock().unwrap();
        assert_eq!(s.candidates.lock().unwrap().list().len(), 1);
    }
}
