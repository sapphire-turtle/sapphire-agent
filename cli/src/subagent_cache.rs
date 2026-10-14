//! Subagent child conversations, kept outside the workspace.
//!
//! A spec-driven-development workflow wants to resume a specific
//! implementer subagent across fix rounds rather than re-spawning it
//! one-shot each time. Resuming means keeping the child's message
//! history somewhere between calls — this is that store.
//!
//! It is a cache, not part of the session store, for the same reason
//! `digest_cache` isn't: `<workspace>/sessions` is in the retrieve
//! index, and a subagent's full internal transcript is the single most
//! effective way to skew a search over it — tool calls, intermediate
//! reasoning, and false starts that were never meant to be retrieved
//! context. The subagents design also promises that a subagent's
//! conversation reaches neither the parent's history nor the store.
//! A workspace-external cache keeps that promise while making resume
//! possible: losing the whole directory costs the ability to resume
//! in-flight children and nothing else.
//!
//! Follows `src/digest_cache.rs`'s conventions throughout (the
//! filename guard, the temp-file-and-rename write, "unparseable is
//! absent, not fatal", the shape of `prune_before`) rather than
//! inventing a second one.

use anyhow::{Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use tracing::warn;
use uuid::Uuid;

/// One resumable subagent child conversation.
#[derive(Serialize, Deserialize)]
pub struct StoredChild {
    pub agent: String,
    pub history: Vec<crate::provider::ChatMessage>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

pub struct SubagentCache {
    dir: PathBuf,
    /// Cap on the serialized `history` size, in bytes. `put` refuses
    /// (rather than truncates) an entry over this cap — see `put`'s
    /// doc for why truncation is not an option here.
    max_bytes: usize,
}

impl SubagentCache {
    pub fn open(dir: PathBuf, max_bytes: usize) -> Result<Arc<Self>> {
        std::fs::create_dir_all(&dir)?;
        Ok(Arc::new(Self { dir, max_bytes }))
    }

    /// `~/.cache/sapphire-agent/subagents`, beside the digest, image,
    /// and tool-result caches.
    pub fn default_dir() -> Option<PathBuf> {
        dirs::cache_dir().map(|d| d.join("sapphire-agent").join("subagents"))
    }

    /// Reserved DOS device names. Matched case-insensitively against the
    /// whole handle, because Windows treats `CON`, `CON.txt`, `con`, etc.
    /// as the same reserved device regardless of case or extension, for
    /// any path that isn't `\\?\`-prefixed — an allow-listed charset
    /// alone doesn't exclude these, since every character in `"CON"` is
    /// an ordinary ASCII letter. Unlike the open-ended "Windows filename
    /// quirks" this guard would otherwise have to keep growing to cover,
    /// this is the complete, closed, decades-stable set Win32 documents —
    /// there is nothing left to discover here, which is what makes an
    /// explicit list safe.
    const RESERVED_WINDOWS_NAMES: &[&str] = &[
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
        "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];

    /// Subagent handles reach here from tool-call arguments, so they are
    /// not trusted to be filenames.
    ///
    /// Allow-listed by charset rather than deny-listed by pattern: a
    /// handle is expected to be a short generated id, so requiring ASCII
    /// alphanumerics, `-`, and `_` costs nothing and closes every
    /// open-ended unsafe case in one pass — traversal (`..`, `/`, `\`),
    /// absolute and drive-relative paths, and the trailing dots/spaces
    /// Windows silently strips (making `"foo."` and `"foo"` collide).
    /// That charset does *not* exclude reserved DOS device names on its
    /// own (`"CON"` is three ordinary letters), so those get their own
    /// check against the fixed, complete list above — a closed
    /// enumeration, not the kind of open-ended deny-list that keeps
    /// needing another entry.
    fn path_for(&self, handle: &str) -> Result<PathBuf> {
        let is_reserved = Self::RESERVED_WINDOWS_NAMES
            .iter()
            .any(|name| name.eq_ignore_ascii_case(handle));
        if handle.is_empty()
            || is_reserved
            || !handle
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            bail!("subagent handle '{handle}' is not usable as a cache filename");
        }
        Ok(self.dir.join(format!("{handle}.json")))
    }

    /// Store `child` under `handle`. `Ok(false)` when the serialized
    /// history exceeds `max_bytes` — the caller returns the answer and
    /// tells the model the child is not resumable, and nothing is
    /// written. This never truncates: dropping the oldest messages to
    /// fit could leave a `tool_use` whose matching `tool_result` was cut,
    /// and the API rejects such a history outright, making the session
    /// unloadable rather than merely shorter.
    ///
    /// Writes to a sibling temp file and renames it over the target
    /// rather than writing in place, for the same reason as
    /// `DigestCache::put_at`: a direct `fs::write` opens with truncate,
    /// so a `prune_before` (or `get`) racing this call could observe the
    /// file between the truncate and the write — empty or partial,
    /// either way unparseable. A rename within one directory is atomic
    /// on both Unix and Windows, so no reader ever sees anything but the
    /// old content or the new content, never a half-written one.
    pub fn put(&self, handle: &str, child: &StoredChild) -> Result<bool> {
        let path = self.path_for(handle)?;
        let body = serde_json::to_string(child)?;
        if body.len() > self.max_bytes {
            return Ok(false);
        }
        let tmp_path = self
            .dir
            .join(format!("{handle}.json.{}.tmp", Uuid::now_v7()));
        std::fs::write(&tmp_path, &body)?;
        std::fs::rename(&tmp_path, &path)?;
        Ok(true)
    }

    pub fn get(&self, handle: &str) -> Option<StoredChild> {
        let path = self.path_for(handle).ok()?;
        let text = std::fs::read_to_string(path).ok()?;
        match serde_json::from_str(&text) {
            Ok(child) => Some(child),
            Err(e) => {
                warn!("subagent cache: {handle} is unreadable ({e}); treating as absent");
                None
            }
        }
    }

    /// Drop the entry for `handle`, if any. Not an error if it was
    /// already absent.
    ///
    /// No production caller as of Task 6's fix round 1: `subagent.rs`'s
    /// `resume` used to call this when a stored child's agent
    /// definition failed to resolve, on the theory that such an entry
    /// could never be resumed again — but `agents::load_agents_dir`
    /// only *skips* a `.md` it can't read or parse rather than failing
    /// the whole load, so that miss can be transient (a restart mid-
    /// save, a YAML typo since fixed) and the deletion was destroying
    /// conversations a later, successful load would have made resumable
    /// again. `resume` now only bails, and leaves the entry for
    /// `prune_before` to retire on its own schedule instead. Kept
    /// (rather than deleted) for the cache round-trip test below and
    /// because a real caller may still turn up — e.g. an explicit
    /// "forget this conversation" operator action — without reopening
    /// the question of *when* to call it that the deletion above got
    /// wrong.
    #[allow(dead_code)]
    pub fn remove(&self, handle: &str) {
        if let Ok(path) = self.path_for(handle) {
            let _ = std::fs::remove_file(path);
        }
    }

    /// Drop every entry not touched since before `cutoff`. Returns how
    /// many went. Called once per day-boundary tick from the same
    /// heartbeat sweep that prunes `DigestCache`.
    ///
    /// `.tmp` files (`put`'s sibling-temp-file-and-rename write) get a
    /// separate rule, judged by filesystem mtime rather than by parsing
    /// their content as a `StoredChild`. A `.tmp` left behind by a `put`
    /// that crashed *before* its rename is ordinary, complete JSON and
    /// would parse fine either way — but one that crashed *during* the
    /// write can be truncated, unparseable JSON, and the "unparseable
    /// entry is corrupt or mid-write, not provably stale, so don't
    /// delete it" reasoning below is exactly backwards for that case: it
    /// was written to protect a `put` that is still in flight *right
    /// now*, racing this same scan, not to protect a file whose writer
    /// is long gone and never coming back to finish it or overwrite it
    /// (nothing ever `put`s to the same randomly-`Uuid::now_v7()`-named
    /// `.tmp` path twice). Left alone, that reasoning makes an
    /// unparseable `.tmp` orphan permanent — it can never age past
    /// "might still be mid-write" under a rule that only ever looks at
    /// its content, not its age.
    pub fn prune_before(&self, cutoff: DateTime<Utc>) -> usize {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return 0;
        };
        let mut removed = 0;
        for entry in entries.flatten() {
            let path = entry.path();
            let is_tmp = path.extension().is_some_and(|ext| ext == "tmp");
            let stale = if is_tmp {
                entry
                    .metadata()
                    .and_then(|m| m.modified())
                    .map(|modified| DateTime::<Utc>::from(modified) < cutoff)
                    // Can't stat it — treat like any other unreadable
                    // entry: not provably stale, leave it for a later
                    // sweep.
                    .unwrap_or(false)
            } else {
                std::fs::read_to_string(&path)
                    .ok()
                    .and_then(|t| serde_json::from_str::<StoredChild>(&t).ok())
                    .map(|child| child.updated_at < cutoff)
                    // An entry that fails to parse is corrupt or mid-write
                    // (a `put` races this scan), not provably stale — do
                    // not delete it. `get()` already treats it as a miss,
                    // and the next `put` for that handle overwrites it, so
                    // leaving it costs nothing but a wasted file.
                    .unwrap_or(false)
            };
            if stale && std::fs::remove_file(&path).is_ok() {
                removed += 1;
            }
        }
        removed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::ChatMessage;

    fn msg(text: &str) -> ChatMessage {
        ChatMessage::user(text)
    }

    fn child() -> StoredChild {
        StoredChild {
            agent: "impl".into(),
            history: vec![msg("hi")],
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn a_child_round_trips() {
        let d = tempfile::tempdir().unwrap();
        let c = SubagentCache::open(d.path().into(), 1_000_000).unwrap();
        let child = StoredChild {
            agent: "impl".into(),
            history: vec![msg("hi")],
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        assert!(c.put("h1", &child).unwrap());
        assert_eq!(c.get("h1").unwrap().agent, "impl");
    }

    /// Dropping old messages to fit under the cap can leave a `tool_use`
    /// whose matching `tool_result` is gone, and such a history is
    /// rejected by the API outright — the session becomes unloadable.
    /// So an oversized history must be refused wholesale, never
    /// truncated to fit.
    #[test]
    fn an_oversized_history_is_refused_not_truncated() {
        let d = tempfile::tempdir().unwrap();
        let c = SubagentCache::open(d.path().into(), 64).unwrap();
        let big = StoredChild {
            agent: "impl".into(),
            history: vec![msg(&"x".repeat(10_000))],
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        assert!(!c.put("h2", &big).unwrap());
        assert!(c.get("h2").is_none());
    }

    /// A handle reaches here from a tool-call argument, so it is used as
    /// a filename only after passing the same guard `digest_cache` uses
    /// for session ids.
    #[test]
    fn a_handle_that_is_not_a_safe_filename_is_refused() {
        let d = tempfile::tempdir().unwrap();
        let c = SubagentCache::open(d.path().into(), 1_000_000).unwrap();
        for bad in ["..", "a/b", "", "con", "CON"] {
            assert!(c.put(bad, &child()).is_err(), "accepted {bad}");
        }
    }

    /// Pruning is driven off `updated_at`, which the caller sets on
    /// every `StoredChild` it writes — unlike `DigestCache::put`, there
    /// is no separate `put_at` here, because the timestamp a test wants
    /// to place on either side of a cutoff is already a field of the
    /// value being stored rather than a side argument.
    #[test]
    fn pruning_drops_only_entries_older_than_the_cutoff() {
        let d = tempfile::tempdir().unwrap();
        let c = SubagentCache::open(d.path().into(), 1_000_000).unwrap();

        let cutoff = Utc::now();
        let old = StoredChild {
            agent: "impl".into(),
            history: vec![msg("old")],
            created_at: cutoff - chrono::Duration::days(2),
            updated_at: cutoff - chrono::Duration::days(1),
        };
        let fresh = StoredChild {
            agent: "impl".into(),
            history: vec![msg("fresh")],
            created_at: cutoff + chrono::Duration::hours(1),
            updated_at: cutoff + chrono::Duration::hours(1),
        };
        assert!(c.put("old", &old).unwrap());
        assert!(c.put("fresh", &fresh).unwrap());

        assert_eq!(c.prune_before(cutoff), 1);
        assert!(c.get("old").is_none());
        assert!(c.get("fresh").is_some());
    }

    #[test]
    fn an_unreadable_entry_is_absent_rather_than_fatal() {
        let d = tempfile::tempdir().unwrap();
        let c = SubagentCache::open(d.path().into(), 1_000_000).unwrap();
        std::fs::write(d.path().join("h3.json"), "{ not json").unwrap();
        assert!(c.get("h3").is_none());
    }

    /// A prune racing a concurrent `put` on the same handle could see
    /// the file mid-write and fail to parse it — that must not be
    /// treated as "stale", or a legitimate in-flight write gets deleted
    /// out from under its writer.
    #[test]
    fn an_unparseable_entry_survives_a_prune() {
        let d = tempfile::tempdir().unwrap();
        let c = SubagentCache::open(d.path().into(), 1_000_000).unwrap();
        std::fs::write(d.path().join("h3.json"), "{ not json").unwrap();

        let cutoff = Utc::now() + chrono::Duration::days(1);
        assert_eq!(c.prune_before(cutoff), 0);
        assert!(d.path().join("h3.json").exists());
    }

    /// An orphaned `.tmp` file — left behind by a `put` that crashed
    /// mid-write, so its content may not even be valid JSON — must
    /// still age out once it's old enough, judged by filesystem mtime
    /// rather than by parsing it. Without the `.tmp`-specific arm, an
    /// unparseable orphan is "not provably stale" forever, since that
    /// rule only ever looks at content, never age.
    #[test]
    fn an_orphaned_tmp_file_is_pruned_by_age_even_when_unparseable() {
        let d = tempfile::tempdir().unwrap();
        let c = SubagentCache::open(d.path().into(), 1_000_000).unwrap();
        let tmp_path = d.path().join("h4.json.0198f-orphan.tmp");
        std::fs::write(&tmp_path, "{ not even valid json").unwrap();

        // Backdate it past the cutoff — a fresh write must survive a
        // prune (it could be a `put` racing this exact scan).
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        let old_time = filetime::FileTime::from_system_time(old);
        filetime::set_file_mtime(&tmp_path, old_time).unwrap();

        assert_eq!(c.prune_before(Utc::now()), 1);
        assert!(!tmp_path.exists());
    }

    /// The counterpart: a `.tmp` orphan younger than the cutoff must not
    /// be swept up just because it's unparseable — the writer that
    /// created it may still be mid-write.
    #[test]
    fn a_fresh_tmp_file_survives_a_prune() {
        let d = tempfile::tempdir().unwrap();
        let c = SubagentCache::open(d.path().into(), 1_000_000).unwrap();
        let tmp_path = d.path().join("h5.json.0198f-inflight.tmp");
        std::fs::write(&tmp_path, "{ not even valid json").unwrap();

        assert_eq!(c.prune_before(Utc::now() - chrono::Duration::days(1)), 0);
        assert!(tmp_path.exists());
    }

    #[test]
    fn remove_deletes_a_stored_child() {
        let d = tempfile::tempdir().unwrap();
        let c = SubagentCache::open(d.path().into(), 1_000_000).unwrap();
        c.put("h1", &child()).unwrap();
        assert!(c.get("h1").is_some());

        c.remove("h1");
        assert!(c.get("h1").is_none());
    }
}
