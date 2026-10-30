//! Append-only transcript store: one JSONL file per logical day.
//!
//! `speaker` holds an **id**, never a display name. Names resolve from the
//! workspace `voices/` directory at read time, so renaming
//! `voices/blithe-otter-42/` to `voices/tanaka-san/` makes every past
//! transcript read back under the new name with no rewrite pass.
//!
//! Day files use the agent's `day_boundary_hour`, not UTC midnight, so a
//! 02:00 conversation lands in the same file as the evening before it —
//! the same rule the daily logs already follow.

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;

use anyhow::{Context, Result};
use chrono::{DateTime, Local, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::session::local_date_for_timestamp;

/// One ingested segment as recorded on disk.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TranscriptRecord {
    /// The device-supplied idempotency key.
    pub segment: String,
    /// `[device.<name>]` key the bearer token resolved to.
    pub device: String,
    /// When the audio was **recorded**, not when it arrived.
    pub started_at: DateTime<Utc>,
    /// Speech duration after the VAD re-gate — not the length of the
    /// submitted segment. The same measure feeds `min_embed_ms` and a
    /// candidate's promotion total, so all three agree.
    pub speech_ms: u32,
    /// Speaker id, or `None` when the segment was too short to attribute.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speaker: Option<String>,
    /// `SpeakerEmbeddingManager` match score. Absent when `speaker` is None.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speaker_score: Option<f32>,
    pub text: String,
    /// sha256 address of the raw audio in the [`super::cache::AudioCache`].
    pub audio: String,
}

pub struct TranscriptStore {
    dir: PathBuf,
    day_boundary_hour: u8,
}

impl TranscriptStore {
    pub fn open(dir: PathBuf, day_boundary_hour: u8) -> Result<Self> {
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("creating transcript dir {dir:?}"))?;
        Ok(Self {
            dir,
            day_boundary_hour,
        })
    }

    /// The logical day `at` belongs to, honouring `day_boundary_hour`.
    pub fn day_of(&self, at: DateTime<Utc>) -> NaiveDate {
        local_date_for_timestamp(at.with_timezone(&Local), self.day_boundary_hour)
    }

    fn path_for_day(&self, day: NaiveDate) -> PathBuf {
        self.dir.join(format!("{day}.jsonl"))
    }

    /// Append one record to its day file, creating the file if needed.
    pub fn append(&self, rec: &TranscriptRecord) -> Result<()> {
        let path = self.path_for_day(self.day_of(rec.started_at));
        let line = serde_json::to_string(rec).context("serializing transcript record")?;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("opening transcript file {path:?}"))?;
        writeln!(file, "{line}").with_context(|| format!("appending to {path:?}"))?;
        Ok(())
    }

    /// Every record with `from <= started_at <= to`, optionally restricted
    /// to a set of speaker ids, sorted by `started_at`.
    ///
    /// A *set* rather than one id because one person can own several ids:
    /// a speaker directory merged from a promoted candidate lists both in
    /// its `id` marker (see
    /// [`crate::ambient::speaker::registry::SpeakerNames`]), and a filter
    /// given that person's name must return everything recorded under any
    /// of them.
    ///
    /// Unparseable lines are skipped with a warning. A half-written line
    /// from a crash must not make the whole day unreadable.
    pub fn read(
        &self,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
        speakers: Option<&[String]>,
    ) -> Result<Vec<TranscriptRecord>> {
        let mut out = Vec::new();
        // Widen by a day on each side: the boundary hour means a record's
        // logical day can differ from its calendar day.
        let mut day = self.day_of(from) - chrono::Duration::days(1);
        let last = self.day_of(to) + chrono::Duration::days(1);
        while day <= last {
            let path = self.path_for_day(day);
            match std::fs::File::open(&path) {
                Ok(file) => {
                    for line in BufReader::new(file).lines() {
                        let Ok(line) = line else { continue };
                        if line.trim().is_empty() {
                            continue;
                        }
                        match serde_json::from_str::<TranscriptRecord>(&line) {
                            Ok(rec) => out.push(rec),
                            Err(e) => warn!("skipping corrupt transcript line in {path:?}: {e}"),
                        }
                    }
                }
                // A missing day file is normal: most days in the queried
                // range simply have no transcripts. Anything else (a
                // permissions error, for instance) is worse than a corrupt
                // line — it silently drops a whole day rather than one
                // record — so it gets a warning rather than staying quiet.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => warn!("skipping unreadable transcript file {path:?}: {e}"),
            }
            day += chrono::Duration::days(1);
        }
        out.retain(|r| {
            r.started_at >= from
                && r.started_at <= to
                && speakers.is_none_or(|ids| {
                    r.speaker
                        .as_deref()
                        .is_some_and(|s| ids.iter().any(|id| id == s))
                })
        });
        out.sort_by_key(|r| r.started_at);
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn rec(started_at: DateTime<Utc>, speaker: Option<&str>, text: &str) -> TranscriptRecord {
        TranscriptRecord {
            segment: format!("seg-{}", started_at.timestamp()),
            device: "pendant".into(),
            started_at,
            speech_ms: 4200,
            speaker: speaker.map(str::to_string),
            speaker_score: speaker.map(|_| 0.87),
            text: text.into(),
            audio: "a".repeat(64),
        }
    }

    #[test]
    fn append_then_read_round_trips_a_record() {
        let tmp = tempfile::tempdir().unwrap();
        let store = TranscriptStore::open(tmp.path().to_path_buf(), 4).unwrap();
        let at = Utc.with_ymd_and_hms(2026, 8, 26, 5, 3, 11).unwrap();
        let r = rec(at, Some("me"), "hello");
        store.append(&r).unwrap();

        let got = store
            .read(
                at - chrono::Duration::hours(1),
                at + chrono::Duration::hours(1),
                None,
            )
            .unwrap();
        assert_eq!(got, vec![r]);
    }

    #[test]
    fn a_record_before_the_boundary_hour_lands_in_the_previous_day_file() {
        let tmp = tempfile::tempdir().unwrap();
        let store = TranscriptStore::open(tmp.path().to_path_buf(), 4).unwrap();
        // 02:00 local on the 27th is still the 26th when the day starts at 04:00.
        let local = chrono::Local
            .with_ymd_and_hms(2026, 8, 27, 2, 0, 0)
            .unwrap();
        store
            .append(&rec(local.with_timezone(&Utc), None, "late night"))
            .unwrap();

        let files: Vec<String> = std::fs::read_dir(tmp.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(files, vec!["2026-08-26.jsonl".to_string()]);
    }

    #[test]
    fn read_filters_by_speaker_and_by_range() {
        let tmp = tempfile::tempdir().unwrap();
        let store = TranscriptStore::open(tmp.path().to_path_buf(), 4).unwrap();
        let base = Utc.with_ymd_and_hms(2026, 8, 26, 12, 0, 0).unwrap();
        store.append(&rec(base, Some("me"), "mine")).unwrap();
        store
            .append(&rec(
                base + chrono::Duration::minutes(1),
                Some("tanaka-san"),
                "theirs",
            ))
            .unwrap();
        store
            .append(&rec(base + chrono::Duration::hours(5), Some("me"), "later"))
            .unwrap();

        let mine = store
            .read(
                base - chrono::Duration::hours(1),
                base + chrono::Duration::hours(1),
                Some(&["me".to_string()][..]),
            )
            .unwrap();
        assert_eq!(mine.len(), 1, "speaker filter and range both applied");
        assert_eq!(mine[0].text, "mine");
    }

    /// One person can own several ids once a promoted candidate is merged
    /// into an existing speaker directory, so the filter takes a set.
    #[test]
    fn read_matches_any_of_several_speaker_ids() {
        let tmp = tempfile::tempdir().unwrap();
        let store = TranscriptStore::open(tmp.path().to_path_buf(), 4).unwrap();
        let base = Utc.with_ymd_and_hms(2026, 8, 26, 12, 0, 0).unwrap();
        store.append(&rec(base, Some("me"), "before")).unwrap();
        store
            .append(&rec(
                base + chrono::Duration::minutes(1),
                Some("blithe-otter-42"),
                "after",
            ))
            .unwrap();
        store
            .append(&rec(
                base + chrono::Duration::minutes(2),
                Some("someone-else"),
                "not mine",
            ))
            .unwrap();

        let ids = ["me".to_string(), "blithe-otter-42".to_string()];
        let got = store
            .read(
                base - chrono::Duration::hours(1),
                base + chrono::Duration::hours(1),
                Some(&ids[..]),
            )
            .unwrap();
        let texts: Vec<&str> = got.iter().map(|r| r.text.as_str()).collect();
        assert_eq!(texts, vec!["before", "after"]);
    }

    #[test]
    fn read_spans_multiple_day_files_in_time_order() {
        let tmp = tempfile::tempdir().unwrap();
        let store = TranscriptStore::open(tmp.path().to_path_buf(), 4).unwrap();
        let day2 = Utc.with_ymd_and_hms(2026, 8, 27, 12, 0, 0).unwrap();
        let day1 = Utc.with_ymd_and_hms(2026, 8, 26, 12, 0, 0).unwrap();
        store.append(&rec(day2, Some("me"), "second")).unwrap();
        store.append(&rec(day1, Some("me"), "first")).unwrap();

        let all = store
            .read(
                day1 - chrono::Duration::days(1),
                day2 + chrono::Duration::days(1),
                None,
            )
            .unwrap();
        let texts: Vec<&str> = all.iter().map(|r| r.text.as_str()).collect();
        assert_eq!(texts, vec!["first", "second"], "sorted by started_at");
    }

    #[test]
    fn read_orders_records_within_a_single_day_file_by_started_at() {
        let tmp = tempfile::tempdir().unwrap();
        let store = TranscriptStore::open(tmp.path().to_path_buf(), 4).unwrap();
        let base = Utc.with_ymd_and_hms(2026, 8, 26, 12, 0, 0).unwrap();
        let earlier = base;
        let later = base + chrono::Duration::minutes(30);
        // Appended out of chronological order, into the same day file.
        store.append(&rec(later, Some("me"), "later")).unwrap();
        store.append(&rec(earlier, Some("me"), "earlier")).unwrap();

        let got = store
            .read(
                base - chrono::Duration::hours(1),
                base + chrono::Duration::hours(1),
                None,
            )
            .unwrap();
        let texts: Vec<&str> = got.iter().map(|r| r.text.as_str()).collect();
        assert_eq!(
            texts,
            vec!["earlier", "later"],
            "sorted by started_at within one file"
        );
    }

    #[test]
    fn a_corrupt_line_is_skipped_rather_than_failing_the_read() {
        let tmp = tempfile::tempdir().unwrap();
        let store = TranscriptStore::open(tmp.path().to_path_buf(), 4).unwrap();
        let at = Utc.with_ymd_and_hms(2026, 8, 26, 12, 0, 0).unwrap();
        store.append(&rec(at, Some("me"), "good")).unwrap();
        let day_file = tmp.path().join("2026-08-26.jsonl");
        let mut body = std::fs::read_to_string(&day_file).unwrap();
        body.push_str("{not json at all\n");
        std::fs::write(&day_file, body).unwrap();

        let got = store
            .read(
                at - chrono::Duration::hours(1),
                at + chrono::Duration::hours(1),
                None,
            )
            .unwrap();
        assert_eq!(got.len(), 1);
    }
}
