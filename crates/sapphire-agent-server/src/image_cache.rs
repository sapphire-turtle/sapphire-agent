//! Workspace-external cache for inline image bytes.
//!
//! `ChatMessage` history is held in memory and persisted to JSONL.
//! Carrying raw base64 image data through either path is expensive:
//! megabyte-scale blobs balloon RAM, JSONL size, and per-turn token
//! estimation. This module stores image bytes outside the workspace,
//! keyed by SHA-256, and exposes scrub/hydrate helpers so the rest of
//! the codebase can swap between [`ContentPart::Image`] (full bytes)
//! and [`ContentPart::ImageRef`] (compact reference) without thinking
//! about file I/O.
//!
//! Design choices:
//! - **Filename is the sha256 hex**, with no extension. The MIME type
//!   travels separately on the `ImageRef` itself; the cache file is a
//!   raw byte blob keyed by content hash.
//! - **Default location** comes from the platform's standard cache dir
//!   (`dirs::cache_dir()` → `$XDG_CACHE_HOME/sapphire-agent/images/`
//!   on Linux, `~/Library/Caches/sapphire-agent/images/` on macOS).
//!   Overridable via `[image_cache] dir = "..."` in `config.toml`.
//! - **No eviction yet**: deliberately. The user's stated concern is
//!   in-memory bloat over long-running sessions, not disk bloat.
//!   LRU/size-cap eviction is a planned follow-up.
//! - **Failures don't panic**: cache write errors degrade gracefully —
//!   the message keeps its `Image` part, which then falls back to the
//!   `SessionStore::append` text-marker scrub on persist.
//!
//! The cache is **content-addressable**: identical bytes from different
//! sessions share one file on disk.

use anyhow::{Context, Result};
use base64::{Engine, engine::general_purpose::STANDARD as BASE64_STANDARD};
use sha2::{Digest, Sha256};
use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::Arc;
use tracing::warn;

use crate::provider::{ChatMessage, ContentPart};

/// Image cache rooted at a single directory. Cheap to clone via `Arc`.
pub struct ImageCache {
    dir: PathBuf,
}

impl ImageCache {
    /// Create (or open) a cache rooted at `dir`. The directory is
    /// created if it doesn't exist; missing parents are created too.
    pub fn open(dir: PathBuf) -> Result<Arc<Self>> {
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("creating image cache dir {dir:?}"))?;
        Ok(Arc::new(Self { dir }))
    }

    /// Platform-standard default location, suitable as a config default.
    /// Returns `None` when the platform exposes no cache dir (rare —
    /// effectively only headless edge cases).
    pub fn default_dir() -> Option<PathBuf> {
        dirs::cache_dir().map(|d| d.join("sapphire-agent").join("images"))
    }

    fn path_for(&self, sha256: &str) -> PathBuf {
        self.dir.join(sha256)
    }

    /// Write `bytes` to the cache under `sha256`. Content-addressable:
    /// if a file with the same hash already exists, this is a no-op
    /// (no rewrite, no error).
    pub fn put(&self, sha256: &str, bytes: &[u8]) -> Result<()> {
        let path = self.path_for(sha256);
        if path.exists() {
            return Ok(());
        }
        std::fs::write(&path, bytes)
            .with_context(|| format!("writing image cache file {path:?}"))?;
        Ok(())
    }

    /// Read raw bytes for `sha256`. Returns `None` on cache miss
    /// (file absent or unreadable for any reason).
    pub fn get(&self, sha256: &str) -> Option<Vec<u8>> {
        std::fs::read(self.path_for(sha256)).ok()
    }
}

/// Compute the SHA-256 hex digest of `bytes`.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    let digest = h.finalize();
    let mut s = String::with_capacity(64);
    for b in digest.iter() {
        let _ = write!(&mut s, "{b:02x}");
    }
    s
}

/// Convert `ContentPart::Image` parts in `history` to `ImageRef`,
/// writing the raw bytes to `cache` along the way. In-place mutation —
/// the caller's history (the canonical in-memory store) shrinks to
/// hash-only references after the call.
///
/// A no-op when `cache` is `None`. A cache write failure on a single
/// image leaves THAT part as `Image` (with a warning) so the on-disk
/// scrub fallback still produces a hash marker. Idempotent: existing
/// `ImageRef` parts are left untouched.
pub fn scrub_history_inplace(history: &mut [ChatMessage], cache: Option<&ImageCache>) {
    let Some(cache) = cache else {
        return;
    };
    for msg in history.iter_mut() {
        for part in msg.parts.iter_mut() {
            let ContentPart::Image {
                media_type,
                data_base64,
            } = part
            else {
                continue;
            };
            let bytes = match BASE64_STANDARD.decode(data_base64.as_bytes()) {
                Ok(b) => b,
                Err(e) => {
                    warn!("image_cache: skipping scrub for un-decodable base64: {e}");
                    continue;
                }
            };
            let sha = sha256_hex(&bytes);
            if let Err(e) = cache.put(&sha, &bytes) {
                warn!("image_cache: cache write failed (keeping inline bytes): {e}");
                continue;
            }
            *part = ContentPart::ImageRef {
                media_type: std::mem::take(media_type),
                sha256: sha,
            };
        }
    }
}

/// Build a provider-ready copy of `history`: `ImageRef` parts are
/// degraded to a `Text` marker (`[image: <media_type> sha256=<hex>]`)
/// so that historical images are NOT re-sent to the model every turn.
/// `Image` parts pass through unchanged — an image that arrived this
/// turn hasn't been scrubbed to `ImageRef` yet, and is the only thing
/// that should reach the model.
///
/// Rationale: re-sending every past image as input bytes every turn
/// re-bills the entire image-token cost per turn for the whole
/// session. The cache still retains the raw bytes by sha256, so a
/// dedicated `recall_image` tool can fetch a specific past image on
/// demand when the user actually asks about one — but day-to-day
/// "react to this image" interactions don't pay for the entire
/// historical gallery.
pub fn hydrate_history(history: &[ChatMessage]) -> Vec<ChatMessage> {
    history
        .iter()
        .map(|msg| ChatMessage {
            role: msg.role.clone(),
            parts: msg
                .parts
                .iter()
                .map(|p| match p {
                    ContentPart::ImageRef { media_type, sha256 } => {
                        ContentPart::Text(format!("[image: {media_type} sha256={sha256}]"))
                    }
                    other => other.clone(),
                })
                .collect(),
            input_kind: msg.input_kind.clone(),
            user_id: msg.user_id.clone(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::Role;
    use tempfile::TempDir;

    fn fake_image() -> Vec<u8> {
        b"\xff\xd8\xff\xe0fake-jpeg-bytes".to_vec()
    }

    #[test]
    fn put_and_get_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let cache = ImageCache::open(tmp.path().to_path_buf()).unwrap();
        let bytes = fake_image();
        let sha = sha256_hex(&bytes);
        cache.put(&sha, &bytes).unwrap();
        assert_eq!(cache.get(&sha).unwrap(), bytes);
    }

    #[test]
    fn put_is_idempotent_on_same_hash() {
        let tmp = TempDir::new().unwrap();
        let cache = ImageCache::open(tmp.path().to_path_buf()).unwrap();
        let bytes = fake_image();
        let sha = sha256_hex(&bytes);
        cache.put(&sha, &bytes).unwrap();
        cache.put(&sha, &bytes).unwrap();
        assert_eq!(cache.get(&sha).unwrap(), bytes);
    }

    #[test]
    fn get_returns_none_on_miss() {
        let tmp = TempDir::new().unwrap();
        let cache = ImageCache::open(tmp.path().to_path_buf()).unwrap();
        assert!(cache.get("deadbeef").is_none());
    }

    #[test]
    fn scrub_converts_image_to_ref_and_writes_cache() {
        let tmp = TempDir::new().unwrap();
        let cache = ImageCache::open(tmp.path().to_path_buf()).unwrap();
        let bytes = fake_image();
        let b64 = BASE64_STANDARD.encode(&bytes);
        let mut history = vec![ChatMessage::user_with_images(
            "describe",
            std::iter::once(("image/jpeg".to_string(), b64.clone())),
        )];

        scrub_history_inplace(&mut history, Some(&cache));

        let parts = &history[0].parts;
        assert!(
            parts
                .iter()
                .any(|p| matches!(p, ContentPart::ImageRef { sha256, .. } if *sha256 == sha256_hex(&bytes))),
            "history should contain ImageRef after scrub; got {parts:?}"
        );
        assert!(
            !parts.iter().any(|p| matches!(p, ContentPart::Image { .. })),
            "history should NOT still contain Image; got {parts:?}"
        );

        let stored = cache.get(&sha256_hex(&bytes)).expect("cache hit");
        assert_eq!(stored, bytes);
    }

    #[test]
    fn scrub_is_noop_when_cache_disabled() {
        let bytes = fake_image();
        let b64 = BASE64_STANDARD.encode(&bytes);
        let mut history = vec![ChatMessage::user_with_images(
            "describe",
            std::iter::once(("image/png".to_string(), b64)),
        )];
        scrub_history_inplace(&mut history, None);
        assert!(
            history[0]
                .parts
                .iter()
                .any(|p| matches!(p, ContentPart::Image { .. })),
            "Image should remain when cache is disabled"
        );
    }

    #[test]
    fn hydrate_degrades_imageref_to_text_marker() {
        let history = vec![ChatMessage {
            role: Role::User,
            parts: vec![ContentPart::ImageRef {
                media_type: "image/jpeg".to_string(),
                sha256: "abc123".to_string(),
            }],
            input_kind: None,
            user_id: None,
        }];
        let hydrated = hydrate_history(&history);
        match &hydrated[0].parts[0] {
            ContentPart::Text(s) => {
                assert!(s.contains("image/jpeg"));
                assert!(s.contains("sha256=abc123"));
            }
            other => panic!("expected Text marker, got {other:?}"),
        }
    }

    #[test]
    fn hydrate_leaves_image_parts_alone() {
        let bytes = fake_image();
        let b64 = BASE64_STANDARD.encode(&bytes);
        let history = vec![ChatMessage::user_with_images(
            "now",
            std::iter::once(("image/png".to_string(), b64.clone())),
        )];
        let hydrated = hydrate_history(&history);
        match &hydrated[0].parts[0] {
            ContentPart::Image {
                media_type,
                data_base64,
            } => {
                assert_eq!(media_type, "image/png");
                assert_eq!(data_base64, &b64);
            }
            other => panic!("expected unchanged Image, got {other:?}"),
        }
    }
}
