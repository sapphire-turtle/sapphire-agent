//! Temporary, conversation-scoped storage for unmodified channel attachments.
//!
//! The caller runs this synchronous API on a blocking worker. A store directory
//! is owned by one serve process; the mutex serializes all operations in it.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow, bail};
use sha2::{Digest, Sha256};

use crate::channel::MAX_DOWNLOAD_BYTES;

pub const RETENTION_MS: u64 = 7 * 24 * 60 * 60 * 1000;
pub const STORE_CAP_BYTES: u64 = 1024 * 1024 * 1024;
const UNAVAILABLE: &str = "attachment unavailable in this conversation or expired";

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AttachmentOwner {
    pub platform: String,
    pub room_id: String,
    pub thread_id: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct StoredAttachment {
    pub version: u32,
    pub id: String,
    pub owner: AttachmentOwner,
    pub message_id: String,
    pub filename: String,
    pub media_type: String,
    pub bytes: u64,
    pub received_at_ms: u64,
    pub expires_at_ms: u64,
}

pub struct AttachmentStore {
    dir: PathBuf,
    gate: Mutex<()>,
    cap: u64,
}

impl AttachmentStore {
    pub fn open(dir: PathBuf) -> Result<Arc<Self>> {
        Self::open_inner(dir, STORE_CAP_BYTES)
    }

    #[cfg(test)]
    fn open_with_cap(dir: PathBuf, cap: u64) -> Result<Arc<Self>> {
        Self::open_inner(dir, cap)
    }

    fn open_inner(dir: PathBuf, cap: u64) -> Result<Arc<Self>> {
        fs::create_dir_all(&dir).context("create attachment store directory")?;
        Ok(Arc::new(Self {
            dir,
            gate: Mutex::new(()),
            cap,
        }))
    }

    pub fn default_dir(root: &Path) -> Result<PathBuf> {
        let root = root.canonicalize().context("canonicalize workspace root")?;
        let mut hash = Sha256::new();
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            hash.update(root.as_os_str().as_bytes());
        }
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStrExt;
            for unit in root.as_os_str().encode_wide() {
                hash.update(unit.to_le_bytes());
            }
        }
        let key: String = hash.finalize().iter().map(|b| format!("{b:02x}")).collect();
        Ok(dirs::cache_dir()
            .context("find standard cache directory")?
            .join("sapphire-agent")
            .join("attachments")
            .join(key))
    }

    pub fn insert(
        &self,
        owner: AttachmentOwner,
        message_id: String,
        filename: String,
        media_type: String,
        bytes: &[u8],
        now_ms: u64,
    ) -> Result<StoredAttachment> {
        let _guard = self
            .gate
            .lock()
            .map_err(|_| anyhow!("attachment store lock poisoned"))?;
        self.sweep_locked(now_ms)?;
        if bytes.len() > MAX_DOWNLOAD_BYTES {
            bail!("attachment exceeds 32 MiB limit");
        }
        let expires_at_ms = now_ms
            .checked_add(RETENTION_MS)
            .context("attachment expiry overflow")?;
        let id = uuid::Uuid::now_v7().to_string();
        let meta = StoredAttachment {
            version: 1,
            id: id.clone(),
            owner,
            message_id,
            filename,
            media_type,
            bytes: bytes.len() as u64,
            received_at_ms: now_ms,
            expires_at_ms,
        };
        let json = serde_json::to_vec(&meta)?;
        let total = self
            .regular_file_bytes()?
            .checked_add(bytes.len() as u64)
            .and_then(|n| n.checked_add(json.len() as u64))
            .context("attachment store size overflow")?;
        if total > self.cap {
            bail!("attachment store capacity exceeded");
        }

        let bin = self.path(&id, "bin");
        let json_path = self.path(&id, "json");
        let bin_tmp = self.path(&id, "bin.tmp");
        let json_tmp = self.path(&id, "json.tmp");
        // Never clean up or overwrite an existing entry, even on an ID collision.
        for path in [&bin, &json_path, &bin_tmp, &json_tmp] {
            match fs::symlink_metadata(path) {
                Ok(_) => bail!("attachment ID collision"),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
        let mut created = Vec::new();
        let result = (|| -> Result<()> {
            write_new(&bin_tmp, bytes, &mut created)?;
            write_new(&json_tmp, &json, &mut created)?;
            fs::rename(&bin_tmp, &bin)?;
            created.push(bin.clone());
            fs::rename(&json_tmp, &json_path)?;
            created.push(json_path.clone());
            Ok(())
        })();
        if let Err(err) = result {
            for path in created {
                if let Err(e) = fs::remove_file(&path)
                    && e.kind() != std::io::ErrorKind::NotFound
                {
                    tracing::warn!("failed to clean incomplete attachment file: {e}");
                }
            }
            return Err(err.context("persist attachment"));
        }
        Ok(meta)
    }

    pub fn get(
        &self,
        owner: &AttachmentOwner,
        id: &str,
        now_ms: u64,
    ) -> Result<(StoredAttachment, Vec<u8>)> {
        let _guard = self
            .gate
            .lock()
            .map_err(|_| anyhow!("attachment store lock poisoned"))?;
        // Do not add identifying context to failures: callers must not be able
        // to distinguish another conversation's ID from an expired/missing ID.
        self.get_locked(owner, id, now_ms)
            .ok_or_else(|| anyhow!(UNAVAILABLE))
    }

    fn get_locked(
        &self,
        owner: &AttachmentOwner,
        id: &str,
        now_ms: u64,
    ) -> Option<(StoredAttachment, Vec<u8>)> {
        if !canonical_id(id) {
            return None;
        }
        let json = self.path(id, "json");
        if !regular_file(&json).ok()? {
            return None;
        }
        let meta: StoredAttachment =
            serde_json::from_reader(BufReader::new(File::open(json).ok()?)).ok()?;
        if meta.version != 1
            || meta.id != id
            || &meta.owner != owner
            || now_ms >= meta.expires_at_ms
            || meta.bytes > MAX_DOWNLOAD_BYTES as u64
        {
            return None;
        }
        let bin = self.path(id, "bin");
        let stat = fs::symlink_metadata(&bin).ok()?;
        if !stat.file_type().is_file() || stat.len() != meta.bytes {
            return None;
        }
        let mut bytes = Vec::new();
        File::open(bin)
            .ok()?
            .take(MAX_DOWNLOAD_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .ok()?;
        if bytes.len() as u64 != meta.bytes {
            return None;
        }
        Some((meta, bytes))
    }

    pub fn sweep(&self, now_ms: u64) -> Result<()> {
        let _guard = self
            .gate
            .lock()
            .map_err(|_| anyhow!("attachment store lock poisoned"))?;
        self.sweep_locked(now_ms)
    }

    fn path(&self, id: &str, suffix: &str) -> PathBuf {
        self.dir.join(format!("{id}.{suffix}"))
    }

    fn regular_file_bytes(&self) -> Result<u64> {
        let mut total = 0u64;
        for entry in fs::read_dir(&self.dir)? {
            let entry = entry?;
            let stat = fs::symlink_metadata(entry.path())?;
            if stat.file_type().is_file() {
                total = total
                    .checked_add(stat.len())
                    .context("attachment store size overflow")?;
            }
        }
        Ok(total)
    }

    fn sweep_locked(&self, now_ms: u64) -> Result<()> {
        // Only classify canonical IDs with our own suffixes. Unknown files,
        // directories and symlinks are neither followed nor deleted.
        let mut entries: HashMap<String, GeneratedFiles> = HashMap::new();
        for entry in fs::read_dir(&self.dir)? {
            let entry = entry?;
            if !fs::symlink_metadata(entry.path())?.file_type().is_file() {
                continue;
            }
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            let Some((id, suffix)) = generated_name(name) else {
                continue;
            };
            let files = entries.entry(id.to_owned()).or_default();
            match suffix {
                "bin" => files.bin = true,
                "json" => files.json = true,
                "bin.tmp" | "json.tmp" => {
                    tracing::warn!("removing incomplete attachment file");
                    remove_generated(&entry.path())?;
                }
                _ => unreachable!(),
            }
        }
        for (id, files) in entries {
            let json = self.path(&id, "json");
            let bin = self.path(&id, "bin");
            let mut keep = false;
            if files.json {
                // Inspect version before deserializing: a future schema need not
                // have any of the fields required by version 1.
                let value: std::result::Result<serde_json::Value, _> =
                    serde_json::from_reader(BufReader::new(File::open(&json)?));
                if let Ok(value) = value {
                    if value
                        .get("version")
                        .and_then(|v| v.as_u64())
                        .is_some_and(|v| v > 1)
                    {
                        keep = true;
                    } else if let Ok(meta) = serde_json::from_value::<StoredAttachment>(value) {
                        keep = meta.version == 1
                            && meta.id == id
                            && now_ms < meta.expires_at_ms
                            && meta.bytes <= MAX_DOWNLOAD_BYTES as u64
                            && files.bin
                            && fs::symlink_metadata(&bin)?.len() == meta.bytes;
                    }
                }
            }
            if !keep {
                tracing::warn!("removing expired, orphaned or corrupt attachment entry");
                if files.json {
                    remove_generated(&json)?;
                }
                if files.bin {
                    remove_generated(&bin)?;
                }
            }
        }
        Ok(())
    }
}

#[derive(Default)]
struct GeneratedFiles {
    bin: bool,
    json: bool,
}

fn canonical_id(id: &str) -> bool {
    uuid::Uuid::parse_str(id).is_ok_and(|uuid| uuid.to_string() == id)
}

fn generated_name(name: &str) -> Option<(&str, &str)> {
    for suffix in ["bin.tmp", "json.tmp", "bin", "json"] {
        if let Some(id) = name.strip_suffix(suffix).and_then(|s| s.strip_suffix('.'))
            && canonical_id(id)
        {
            return Some((id, suffix));
        }
    }
    None
}

fn regular_file(path: &Path) -> std::io::Result<bool> {
    Ok(fs::symlink_metadata(path)?.file_type().is_file())
}

fn write_new(path: &Path, data: &[u8], created: &mut Vec<PathBuf>) -> Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    created.push(path.to_owned());
    file.write_all(data)?;
    drop(file);
    Ok(())
}

fn remove_generated(path: &Path) -> Result<()> {
    // Recheck file type rather than follow a link that appeared after listing.
    match fs::symlink_metadata(path) {
        Ok(stat) if stat.file_type().is_file() => fs::remove_file(path)?,
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::{Arc, Barrier};

    fn owner() -> AttachmentOwner {
        AttachmentOwner {
            platform: "discord".into(),
            room_id: "7".into(),
            thread_id: None,
        }
    }
    fn insert(store: &AttachmentStore, bytes: &[u8], now: u64) -> StoredAttachment {
        store
            .insert(
                owner(),
                "m".into(),
                "x.pdf".into(),
                "application/pdf".into(),
                bytes,
                now,
            )
            .unwrap()
    }
    fn path(dir: &std::path::Path, id: &str, suffix: &str) -> std::path::PathBuf {
        dir.join(format!("{id}.{suffix}"))
    }

    #[test]
    fn original_survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let store = AttachmentStore::open(dir.path().to_owned()).unwrap();
        let meta = insert(&store, b"original", 1000);
        assert_eq!(
            uuid::Uuid::parse_str(&meta.id).unwrap().get_version_num(),
            7
        );
        assert_eq!(meta.version, 1);
        assert_eq!(meta.bytes, 8);
        assert_eq!(meta.received_at_ms, 1000);
        assert_eq!(meta.expires_at_ms, 1000 + RETENTION_MS);
        drop(store);
        let reopened = AttachmentStore::open(dir.path().to_owned()).unwrap();
        let (again, bytes) = reopened.get(&owner(), &meta.id, 1001).unwrap();
        assert_eq!(bytes, b"original");
        assert_eq!(again.filename, "x.pdf");
        assert_eq!(again.media_type, "application/pdf");
        assert_eq!(again.message_id, "m");
    }
    #[test]
    fn expiry_is_inclusive_and_reads_do_not_extend_it() {
        let dir = tempfile::tempdir().unwrap();
        let store = AttachmentStore::open(dir.path().to_owned()).unwrap();
        let meta = insert(&store, b"original", 1000);
        assert!(
            store
                .get(&owner(), &meta.id, 1000 + RETENTION_MS - 1)
                .is_ok()
        );
        assert!(store.get(&owner(), &meta.id, 1000 + RETENTION_MS).is_err());
        store.sweep(1000 + RETENTION_MS).unwrap();
        assert!(!path(dir.path(), &meta.id, "bin").exists());
        assert!(!path(dir.path(), &meta.id, "json").exists());
    }
    #[test]
    fn owner_is_platform_room_and_thread() {
        let dir = tempfile::tempdir().unwrap();
        let store = AttachmentStore::open(dir.path().to_owned()).unwrap();
        let meta = insert(&store, b"a", 0);
        for other in [
            AttachmentOwner {
                platform: "matrix".into(),
                ..owner()
            },
            AttachmentOwner {
                room_id: "8".into(),
                ..owner()
            },
            AttachmentOwner {
                thread_id: Some("t".into()),
                ..owner()
            },
        ] {
            assert!(store.get(&other, &meta.id, 0).is_err());
        }
        let threaded = AttachmentOwner {
            thread_id: Some("t".into()),
            ..owner()
        };
        let m = store
            .insert(
                threaded.clone(),
                "m".into(),
                "x".into(),
                "x".into(),
                b"b",
                0,
            )
            .unwrap();
        assert!(store.get(&threaded, &m.id, 0).is_ok());
        assert!(store.get(&owner(), &m.id, 0).is_err());
    }
    #[test]
    fn rejects_noncanonical_uuid_and_traversal() {
        let dir = tempfile::tempdir().unwrap();
        let store = AttachmentStore::open(dir.path().to_owned()).unwrap();
        let m = insert(&store, b"x", 0);
        for id in [
            m.id.to_uppercase(),
            m.id.replace('-', ""),
            format!("../{}", m.id),
            format!("{{{}}}", m.id),
        ] {
            assert!(store.get(&owner(), &id, 0).is_err(), "{id}");
        }
    }
    #[test]
    fn unavailable_errors_are_identical() {
        let dir = tempfile::tempdir().unwrap();
        let store = AttachmentStore::open(dir.path().to_owned()).unwrap();
        let m = insert(&store, b"x", 0);
        let other = AttachmentOwner {
            room_id: "other".into(),
            ..owner()
        };
        let absent = uuid::Uuid::now_v7().to_string();
        for error in [
            store.get(&other, &m.id, 0).unwrap_err(),
            store.get(&owner(), &absent, 0).unwrap_err(),
            store.get(&owner(), &m.id, RETENTION_MS).unwrap_err(),
        ] {
            assert_eq!(
                error.to_string(),
                "attachment unavailable in this conversation or expired"
            );
        }
    }
    #[test]
    fn capacity_refuses_new_without_evicting_live() {
        let dir = tempfile::tempdir().unwrap();
        let store = AttachmentStore::open_with_cap(dir.path().to_owned(), 500).unwrap();
        let m = insert(&store, &[1; 100], 0);
        assert!(
            store
                .insert(owner(), "m".into(), "x".into(), "x".into(), &[2; 100], 0)
                .is_err()
        );
        assert_eq!(store.get(&owner(), &m.id, 0).unwrap().1, vec![1; 100]);
        store.sweep(RETENTION_MS).unwrap();
        assert!(
            store
                .insert(
                    owner(),
                    "m".into(),
                    "x".into(),
                    "x".into(),
                    &[2; 100],
                    RETENTION_MS
                )
                .is_ok()
        );
    }
    #[test]
    fn concurrent_inserts_cannot_exceed_cap() {
        let dir = tempfile::tempdir().unwrap();
        let store = AttachmentStore::open_with_cap(dir.path().to_owned(), 700).unwrap();
        let barrier = Arc::new(Barrier::new(8));
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let store = Arc::clone(&store);
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    store
                        .insert(owner(), "m".into(), "x".into(), "x".into(), &[1; 100], 0)
                        .is_ok()
                })
            })
            .collect();
        let successes = handles
            .into_iter()
            .map(|h| h.join().unwrap())
            .filter(|ok| *ok)
            .count();
        assert!(successes > 0 && successes < 8);
        let total: u64 = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().metadata().unwrap().len())
            .sum();
        assert!(total <= 700);
    }
    #[test]
    fn sweep_only_touches_generated_files() {
        let dir = tempfile::tempdir().unwrap();
        let store = AttachmentStore::open_with_cap(dir.path().to_owned(), 500).unwrap();
        let unrelated = dir.path().join("unrelated.bin");
        fs::write(&unrelated, vec![0; 500]).unwrap();
        let id = uuid::Uuid::now_v7().to_string();
        let subdir = path(dir.path(), &id, "json");
        fs::create_dir(&subdir).unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&unrelated, path(dir.path(), &id, "bin")).unwrap();
        }
        store.sweep(0).unwrap();
        assert!(unrelated.is_file());
        assert!(subdir.is_dir());
        #[cfg(unix)]
        assert!(
            fs::symlink_metadata(path(dir.path(), &id, "bin"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(
            store
                .insert(owner(), "m".into(), "x".into(), "x".into(), b"x", 0)
                .is_err()
        );
    }
    #[test]
    fn orphan_and_corrupt_entries_are_cleaned() {
        let dir = tempfile::tempdir().unwrap();
        let store = AttachmentStore::open(dir.path().to_owned()).unwrap();
        let orphan = uuid::Uuid::now_v7().to_string();
        let corrupt = uuid::Uuid::now_v7().to_string();
        let missing = uuid::Uuid::now_v7().to_string();
        let m = insert(&store, b"valid", 0);
        fs::write(path(dir.path(), &orphan, "bin"), b"orphan").unwrap();
        fs::write(path(dir.path(), &orphan, "bin.tmp"), b"tmp").unwrap();
        fs::write(path(dir.path(), &corrupt, "bin"), b"bad").unwrap();
        fs::write(path(dir.path(), &corrupt, "json"), b"not json").unwrap();
        let mut data = serde_json::to_value(&m).unwrap();
        data["id"] = missing.clone().into();
        fs::write(
            path(dir.path(), &missing, "json"),
            serde_json::to_vec(&data).unwrap(),
        )
        .unwrap();
        store.sweep(0).unwrap();
        for (id, suffix) in [
            (&orphan, "bin"),
            (&orphan, "bin.tmp"),
            (&corrupt, "bin"),
            (&corrupt, "json"),
            (&missing, "json"),
        ] {
            assert!(!path(dir.path(), id, suffix).exists());
        }
        assert_eq!(store.get(&owner(), &m.id, 0).unwrap().1, b"valid");
    }
    #[test]
    fn future_version_is_preserved_and_counted() {
        let dir = tempfile::tempdir().unwrap();
        let store = AttachmentStore::open_with_cap(dir.path().to_owned(), 600).unwrap();
        let id = uuid::Uuid::now_v7().to_string();
        fs::write(path(dir.path(), &id, "json"), b"{\"version\":2}").unwrap();
        fs::write(path(dir.path(), &id, "bin"), vec![1; 500]).unwrap();
        store.sweep(u64::MAX).unwrap();
        assert!(path(dir.path(), &id, "json").exists());
        assert!(path(dir.path(), &id, "bin").exists());
        assert!(store.get(&owner(), &id, 0).is_err());
        assert!(
            store
                .insert(owner(), "m".into(), "x".into(), "x".into(), b"hello", 0)
                .is_err()
        );
    }
    #[test]
    fn rejects_oversized_original_and_size_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let store = AttachmentStore::open(dir.path().to_owned()).unwrap();
        let too_large = vec![0; MAX_DOWNLOAD_BYTES + 1];
        assert!(
            store
                .insert(owner(), "m".into(), "x".into(), "x".into(), &too_large, 0)
                .is_err()
        );
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
        let max = &too_large[..MAX_DOWNLOAD_BYTES];
        let m = insert(&store, max, 0);
        assert_eq!(store.get(&owner(), &m.id, 0).unwrap().1, max);
        fs::write(path(dir.path(), &m.id, "bin"), b"tampered").unwrap();
        assert!(store.get(&owner(), &m.id, 0).is_err());
        store.sweep(0).unwrap();
        assert!(!path(dir.path(), &m.id, "bin").exists());
        assert!(!path(dir.path(), &m.id, "json").exists());
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_not_read_or_removed() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let store = AttachmentStore::open(dir.path().to_owned()).unwrap();
        let m = insert(&store, b"original", 0);
        let bin = path(dir.path(), &m.id, "bin");
        let target = outside.path().join("target");
        fs::write(&target, b"original").unwrap();
        fs::remove_file(&bin).unwrap();
        symlink(&target, &bin).unwrap();
        assert!(store.get(&owner(), &m.id, 0).is_err());
        store.sweep(0).unwrap();
        assert!(fs::symlink_metadata(&bin).unwrap().file_type().is_symlink());
        assert_eq!(fs::read(target).unwrap(), b"original");

        let m2 = insert(&store, b"other", 0);
        let json = path(dir.path(), &m2.id, "json");
        let target = outside.path().join("meta");
        fs::rename(&json, &target).unwrap();
        symlink(&target, &json).unwrap();
        assert!(store.get(&owner(), &m2.id, 0).is_err());
        store.sweep(0).unwrap();
        assert!(
            fs::symlink_metadata(&json)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(target.is_file());
    }

    #[cfg(unix)]
    #[test]
    fn default_dir_hashes_canonical_lossless_workspace_bytes() {
        use std::os::unix::ffi::OsStringExt;
        let dir = tempfile::tempdir().unwrap();
        let root = dir
            .path()
            .join(std::ffi::OsString::from_vec(b"invalid-\xff".to_vec()));
        fs::create_dir(&root).unwrap();
        let actual = AttachmentStore::default_dir(&root.join(".")).unwrap();
        let canonical = root.canonicalize().unwrap();
        use std::os::unix::ffi::OsStrExt;
        let hash = Sha256::digest(canonical.as_os_str().as_bytes());
        let key: String = hash.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            actual,
            dirs::cache_dir()
                .unwrap()
                .join("sapphire-agent/attachments")
                .join(key)
        );
        let other = dir.path().join("other");
        fs::create_dir(&other).unwrap();
        assert_ne!(actual, AttachmentStore::default_dir(&other).unwrap());
    }

    #[test]
    fn expiry_overflow_cannot_wrap() {
        let dir = tempfile::tempdir().unwrap();
        let store = AttachmentStore::open(dir.path().to_owned()).unwrap();
        assert!(
            store
                .insert(owner(), "m".into(), "x".into(), "x".into(), b"x", u64::MAX)
                .is_err()
        );
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn persisted_metadata_contains_no_source_secrets() {
        // Task 4 connects the source fixture to the receive pipeline. This test
        // fixes the store's serialized schema, which has no source fields.
        let dir = tempfile::tempdir().unwrap();
        let store = AttachmentStore::open(dir.path().to_owned()).unwrap();
        insert(&store, b"original", 0);
        for entry in fs::read_dir(dir.path()).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_some_and(|ext| ext == "json") {
                let json = fs::read_to_string(path).unwrap();
                for secret in [
                    "https://cdn.example/secret",
                    "signature=sentinel",
                    "matrix-key-sentinel",
                ] {
                    assert!(!json.contains(secret));
                }
                let value: serde_json::Value = serde_json::from_str(&json).unwrap();
                let keys = value
                    .as_object()
                    .unwrap()
                    .keys()
                    .map(String::as_str)
                    .collect::<std::collections::BTreeSet<_>>();
                assert_eq!(
                    keys,
                    [
                        "version",
                        "id",
                        "owner",
                        "message_id",
                        "filename",
                        "media_type",
                        "bytes",
                        "received_at_ms",
                        "expires_at_ms"
                    ]
                    .into_iter()
                    .collect()
                );
            }
        }
    }
}
