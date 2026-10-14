//! Workspace-external cache for raw ambient audio segments.
//!
//! Same shape and same reasoning as [`crate::image_cache`]: the filename
//! is the sha256 hex with no extension, identical bytes from different
//! devices share one file, and write failures degrade rather than panic.
//!
//! Unlike the image cache this one **does** evict: a day of ambient
//! recording is 200–450 MB, so unbounded growth is not a theoretical
//! concern. Transcripts are kept; only the audio is swept.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use tracing::warn;

use crate::image_cache::sha256_hex;

pub struct AudioCache {
    dir: PathBuf,
}

impl AudioCache {
    pub fn open(dir: PathBuf) -> Result<Arc<Self>> {
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("creating ambient audio cache dir {dir:?}"))?;
        Ok(Arc::new(Self { dir }))
    }

    /// Platform-standard default, suitable as a config default.
    pub fn default_dir() -> Option<PathBuf> {
        dirs::cache_dir().map(|d| d.join("sapphire-agent").join("ambient").join("audio"))
    }

    /// Store `bytes` and return their sha256 hex address. Idempotent: an
    /// existing file with the same hash is left untouched.
    pub fn put(&self, bytes: &[u8]) -> Result<String> {
        let sha = sha256_hex(bytes);
        let path = self.dir.join(&sha);
        if path.exists() {
            return Ok(sha);
        }
        std::fs::write(&path, bytes)
            .with_context(|| format!("writing ambient audio blob {path:?}"))?;
        Ok(sha)
    }

    /// Raw bytes for `sha256`, or `None` on any miss.
    ///
    /// No caller yet: nothing in this plan reads a cached blob back. Kept
    /// deliberately, not speculatively — the seven-day retention default
    /// is justified as "enough to re-listen or re-run STT", and a
    /// retention window with no way to read what it retains would make
    /// that justification meaningless. The natural caller is a future
    /// playback or re-transcribe tool, paired with `put` the way a get
    /// belongs with a put.
    #[allow(dead_code)]
    pub fn get(&self, sha256: &str) -> Option<Vec<u8>> {
        std::fs::read(self.dir.join(sha256)).ok()
    }

    /// Delete blobs whose mtime is older than `max_age`. Returns how many
    /// were removed. A blob that cannot be read or removed is warned about
    /// and skipped — a sweep must never abort the process.
    pub fn sweep(&self, max_age: Duration) -> Result<usize> {
        let cutoff = SystemTime::now()
            .checked_sub(max_age)
            .unwrap_or(SystemTime::UNIX_EPOCH);
        let mut removed = 0;
        for entry in std::fs::read_dir(&self.dir)
            .with_context(|| format!("reading ambient audio cache dir {:?}", self.dir))?
        {
            let entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    warn!("ambient sweep: unreadable dir entry: {e}");
                    continue;
                }
            };
            let modified = match entry.metadata().and_then(|m| m.modified()) {
                Ok(m) => m,
                Err(e) => {
                    warn!("ambient sweep: no mtime for {:?}: {e}", entry.path());
                    continue;
                }
            };
            if modified < cutoff {
                match std::fs::remove_file(entry.path()) {
                    Ok(()) => removed += 1,
                    Err(e) => warn!("ambient sweep: cannot remove {:?}: {e}", entry.path()),
                }
            }
        }
        Ok(removed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn put_is_content_addressed_and_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = AudioCache::open(tmp.path().to_path_buf()).unwrap();
        let sha = cache.put(b"pcm-bytes").unwrap();
        assert_eq!(sha.len(), 64, "sha256 hex");
        assert_eq!(cache.get(&sha).as_deref(), Some(&b"pcm-bytes"[..]));
        // Same bytes, same address, no duplicate file.
        assert_eq!(cache.put(b"pcm-bytes").unwrap(), sha);
        assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 1);
    }

    #[test]
    fn get_misses_return_none_rather_than_erroring() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = AudioCache::open(tmp.path().to_path_buf()).unwrap();
        assert!(cache.get(&"0".repeat(64)).is_none());
    }

    #[test]
    fn sweep_removes_blobs_older_than_the_limit_and_keeps_the_rest() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = AudioCache::open(tmp.path().to_path_buf()).unwrap();
        let fresh = cache.put(b"fresh").unwrap();
        let stale = cache.put(b"stale").unwrap();

        // Backdate one blob by ten days.
        let ten_days_ago =
            std::time::SystemTime::now() - std::time::Duration::from_secs(10 * 86400);
        filetime::set_file_mtime(
            tmp.path().join(&stale),
            filetime::FileTime::from_system_time(ten_days_ago),
        )
        .unwrap();

        let removed = cache
            .sweep(std::time::Duration::from_secs(7 * 86400))
            .unwrap();
        assert_eq!(removed, 1);
        assert!(cache.get(&stale).is_none(), "stale blob swept");
        assert!(cache.get(&fresh).is_some(), "fresh blob kept");
    }
}
