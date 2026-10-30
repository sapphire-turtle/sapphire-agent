//! Tool payloads — both halves of a call — kept outside the workspace
//! and addressed by hash.
//!
//! A coding session reads a lot of files, and writes a few. Persisting
//! either half into the session log would put that content into
//! `<workspace>/sessions`, which the retrieve indexer walks line by
//! line: the workspace would grow by the size of everything the agent
//! ever looked at, and every search would compete with a second copy of
//! the files it already indexes. But a session cannot be restored
//! without the calls: the Anthropic API rejects a `tool_use` with no
//! matching `tool_result`.
//!
//! So the payloads live here and the session log keeps a hash. Same
//! shape as `image_cache`, and for the same reason.
//!
//! **Results and inputs are stored the same way, and for the same
//! reason.** Results moved out first (#194) because they are usually
//! the larger half; inputs followed (#212) once it was clear the
//! argument applies to them unchanged — a `file_write` call carries the
//! file's contents just as a `file_read` result does, and the size
//! threshold that used to elide only the biggest inputs was the wrong
//! instrument for a requirement about what belongs in an index.
//!
//! Losing this cache is survivable, though the two halves degrade
//! differently. A missing result is replaced with a placeholder and the
//! model can simply call the tool again. A missing input leaves the
//! model knowing it made a call but not with what — recoverable, since
//! the API does not validate a replayed `input` against the tool's
//! schema, but genuinely thinner. Either way the history stays valid,
//! which is the property that matters.

use anyhow::Result;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::Arc;
use tracing::warn;

pub struct ToolPayloadCache {
    dir: PathBuf,
}

impl ToolPayloadCache {
    pub fn open(dir: PathBuf) -> Result<Arc<Self>> {
        std::fs::create_dir_all(&dir)?;
        Ok(Arc::new(Self { dir }))
    }

    /// `~/.cache/sapphire-agent/tool-payloads`, beside the image cache.
    pub fn default_dir() -> Option<PathBuf> {
        dirs::cache_dir().map(|d| d.join("sapphire-agent").join("tool-payloads"))
    }

    fn path_for(&self, sha256: &str) -> PathBuf {
        self.dir.join(sha256)
    }

    /// Store `content` and return its hash. Content-addressed, so
    /// storing the same payload twice writes one file.
    pub fn put(&self, content: &str) -> Result<String> {
        let sha = sha256_hex(content.as_bytes());
        let path = self.path_for(&sha);
        if !path.exists() {
            std::fs::write(&path, content)?;
        }
        Ok(sha)
    }

    /// `None` for a hash that is not stored, or whose file cannot be
    /// read as text. Both are misses rather than errors: the caller
    /// substitutes a placeholder, and a lost payload must never make a
    /// session unreadable.
    pub fn get(&self, sha256: &str) -> Option<String> {
        match std::fs::read(self.path_for(sha256)) {
            Ok(bytes) => match String::from_utf8(bytes) {
                Ok(text) => Some(text),
                Err(_) => {
                    warn!("tool-payload cache: {sha256} is not valid UTF-8; treating as absent");
                    None
                }
            },
            Err(_) => None,
        }
    }

    /// A tool call's `input`, stored as its serialized JSON.
    ///
    /// Serialising here rather than at each call site keeps one answer
    /// to "what exactly gets hashed" — two stores write inputs, and a
    /// hash computed over differently-formatted JSON would silently
    /// stop deduplicating.
    pub fn put_input(&self, input: &Value) -> Result<String> {
        self.put(&serde_json::to_string(input)?)
    }

    /// The stored `input`, or `None` if the hash is absent or its file
    /// no longer parses as JSON.
    ///
    /// Unparseable is a miss rather than an error for the same reason
    /// non-UTF-8 is: a corrupt cache entry must cost the payload, never
    /// the session.
    pub fn get_input(&self, sha256: &str) -> Option<Value> {
        let text = self.get(sha256)?;
        match serde_json::from_str(&text) {
            Ok(value) => Some(value),
            Err(e) => {
                warn!("tool-payload cache: {sha256} is not valid JSON ({e}); treating as absent");
                None
            }
        }
    }
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut s = String::with_capacity(64);
    for b in digest.iter() {
        let _ = write!(&mut s, "{b:02x}");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_result_round_trips_by_its_hash() {
        let dir = tempfile::tempdir().unwrap();
        let cache = ToolPayloadCache::open(dir.path().to_path_buf()).unwrap();

        let sha = cache.put("the file contents").unwrap();
        assert_eq!(cache.get(&sha).as_deref(), Some("the file contents"));
    }

    /// Content-addressed: the same result stored twice is one file, and
    /// the caller gets the same handle back.
    #[test]
    fn identical_results_share_one_entry() {
        let dir = tempfile::tempdir().unwrap();
        let cache = ToolPayloadCache::open(dir.path().to_path_buf()).unwrap();

        let a = cache.put("same").unwrap();
        let b = cache.put("same").unwrap();
        assert_eq!(a, b);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    /// A miss is not an error. The caller substitutes a placeholder —
    /// losing a tool result must never make a session unreadable.
    #[test]
    fn an_absent_hash_is_none_rather_than_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let cache = ToolPayloadCache::open(dir.path().to_path_buf()).unwrap();
        assert_eq!(
            cache.get("0000000000000000000000000000000000000000000000000000000000000000"),
            None
        );
    }

    /// Non-UTF8 on disk is corruption, not a panic.
    #[test]
    fn unreadable_bytes_are_a_miss() {
        let dir = tempfile::tempdir().unwrap();
        let cache = ToolPayloadCache::open(dir.path().to_path_buf()).unwrap();
        let sha = "1111111111111111111111111111111111111111111111111111111111111111";
        std::fs::write(dir.path().join(sha), [0xff, 0xfe]).unwrap();
        assert_eq!(cache.get(sha), None);
    }
}
