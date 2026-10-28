//! File layer for the agent workspace's `avatar/` folder.
//!
//! The desktop mate's VRM model and VRMA gestures live in the workspace so
//! they sync like the rest of it, and `avatar.toml` names which file plays
//! which part. Clients reach this over IPC (`sapphire-agent.avatar.*`); this
//! module is only the file I/O behind those methods.
//!
//! Design choices:
//! - **Uploads are staged outside `avatar/`** and replace the final file only
//!   on `write_commit`, after the length and sha256 match. A half-written
//!   model must never appear where the workspace sync can see it.
//! - **Idle uploads are dropped after 10 minutes** without a write. The
//!   server gets no notice when a peer disconnects, so a timer (swept on the
//!   next `write_begin`) is the only way to reclaim an abandoned upload.
//! - **Paths are `/`-separated strings, validated textually** (no `\`, no
//!   `:`, no `.`/`..`), so the same rules hold on Windows and no path can
//!   leave `avatar/`.

// Task 5 wires the store into the IPC methods; until then only tests use it.
#![allow(dead_code)]

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use sapphire_agent_core::{
    AVATAR_CHUNK, AVATAR_MAX_FILE, AvatarConfig, AvatarFile, AvatarManifest,
};
use sha2::{Digest, Sha256};

const CONFIG_FILE: &str = "avatar.toml";
/// An upload with no write for this long is discarded.
const UPLOAD_IDLE: Duration = Duration::from_secs(10 * 60);

#[derive(Debug)]
pub(crate) enum AvatarError {
    /// The caller's mistake; maps to INVALID_PARAMS.
    Invalid(String),
    Io(std::io::Error),
}

impl From<std::io::Error> for AvatarError {
    fn from(e: std::io::Error) -> Self {
        AvatarError::Io(e)
    }
}

impl std::fmt::Display for AvatarError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AvatarError::Invalid(m) => f.write_str(m),
            AvatarError::Io(e) => write!(f, "{e}"),
        }
    }
}

fn invalid<T>(msg: impl Into<String>) -> Result<T, AvatarError> {
    Err(AvatarError::Invalid(msg.into()))
}

/// Check a client-supplied path: relative to `avatar/`, `/`-separated,
/// staying inside it, and not the config file itself.
pub(crate) fn validate_path(path: &str) -> Result<(), AvatarError> {
    if path.is_empty() {
        return invalid("path is empty");
    }
    if path.starts_with('/') || path.contains('\\') || path.contains(':') {
        return invalid(format!("path {path:?} must be relative and use '/'"));
    }
    if path
        .split('/')
        .any(|c| c.is_empty() || c == "." || c == "..")
    {
        return invalid(format!("path {path:?} has an empty, '.' or '..' component"));
    }
    if path == CONFIG_FILE {
        return invalid(format!("{CONFIG_FILE} is managed by set_manifest"));
    }
    Ok(())
}

struct Upload {
    path: String,
    size: u64,
    sha256: String,
    file: PathBuf,
    written: u64,
    last: Instant,
}

pub(crate) struct AvatarStore {
    dir: PathBuf,
    staging: PathBuf,
    uploads: Mutex<HashMap<String, Upload>>,
}

fn sha256_file(path: &Path) -> std::io::Result<String> {
    let mut f = File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

impl AvatarStore {
    /// `dir` is `<workspace>/avatar`; `staging` holds files mid-upload.
    pub(crate) fn new(dir: PathBuf, staging: PathBuf) -> Self {
        Self {
            dir,
            staging,
            uploads: Mutex::new(HashMap::new()),
        }
    }

    fn read_config(&self) -> Result<AvatarConfig, AvatarError> {
        match fs::read_to_string(self.dir.join(CONFIG_FILE)) {
            Ok(s) => {
                toml::from_str(&s).map_err(|e| AvatarError::Invalid(format!("{CONFIG_FILE}: {e}")))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(AvatarConfig::default()),
            Err(e) => Err(e.into()),
        }
    }

    fn collect(&self, rel: &str, out: &mut Vec<AvatarFile>) -> Result<(), AvatarError> {
        let here = if rel.is_empty() {
            self.dir.clone()
        } else {
            self.dir.join(rel)
        };
        for entry in fs::read_dir(&here)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let child = if rel.is_empty() {
                name.clone()
            } else {
                format!("{rel}/{name}")
            };
            let ty = entry.file_type()?;
            if ty.is_dir() {
                self.collect(&child, out)?;
            } else if ty.is_file() && child != CONFIG_FILE {
                let p = entry.path();
                out.push(AvatarFile {
                    size: entry.metadata()?.len(),
                    sha256: sha256_file(&p)?,
                    path: child,
                });
            }
        }
        Ok(())
    }

    pub(crate) fn manifest(&self) -> Result<AvatarManifest, AvatarError> {
        let config = self.read_config()?;
        let mut files = Vec::new();
        if self.dir.is_dir() {
            self.collect("", &mut files)?;
        }
        files.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(AvatarManifest { config, files })
    }

    pub(crate) fn read(
        &self,
        path: &str,
        offset: u64,
        len: usize,
    ) -> Result<(Vec<u8>, bool), AvatarError> {
        validate_path(path)?;
        if len > AVATAR_CHUNK {
            return invalid(format!("len {len} exceeds {AVATAR_CHUNK}"));
        }
        let full = self.dir.join(path);
        if !full.is_file() {
            return invalid(format!("no such file: {path}"));
        }
        let mut f = File::open(&full)?;
        let total = f.metadata()?.len();
        f.seek(SeekFrom::Start(offset))?;
        let mut buf = Vec::new();
        f.take(len as u64).read_to_end(&mut buf)?;
        let eof = offset + buf.len() as u64 >= total;
        Ok((buf, eof))
    }

    /// Drop uploads idle for 10 minutes as of `now`.
    pub(crate) fn sweep(&self, now: Instant) {
        let mut ups = self.uploads.lock().unwrap();
        ups.retain(|_, u| {
            let keep = now.saturating_duration_since(u.last) < UPLOAD_IDLE;
            if !keep {
                let _ = fs::remove_file(&u.file);
            }
            keep
        });
    }

    pub(crate) fn write_begin(
        &self,
        path: &str,
        size: u64,
        sha256: &str,
    ) -> Result<String, AvatarError> {
        validate_path(path)?;
        if size > AVATAR_MAX_FILE {
            return invalid(format!("size {size} exceeds {AVATAR_MAX_FILE}"));
        }
        self.sweep(Instant::now());
        fs::create_dir_all(&self.staging)?;
        let id = uuid::Uuid::now_v7().to_string();
        let file = self.staging.join(&id);
        File::create(&file)?;
        self.uploads.lock().unwrap().insert(
            id.clone(),
            Upload {
                path: path.to_string(),
                size,
                sha256: sha256.to_ascii_lowercase(),
                file,
                written: 0,
                last: Instant::now(),
            },
        );
        Ok(id)
    }

    fn discard(ups: &mut HashMap<String, Upload>, id: &str) {
        if let Some(u) = ups.remove(id) {
            let _ = fs::remove_file(&u.file);
        }
    }

    pub(crate) fn write_chunk(
        &self,
        upload_id: &str,
        offset: u64,
        data: &[u8],
    ) -> Result<(), AvatarError> {
        let mut ups = self.uploads.lock().unwrap();
        let Some(u) = ups.get_mut(upload_id) else {
            return invalid("unknown upload_id");
        };
        if offset != u.written {
            return invalid(format!("offset {offset} != written {}", u.written));
        }
        if u.written + data.len() as u64 > u.size {
            Self::discard(&mut ups, upload_id);
            return invalid("upload exceeds its declared size");
        }
        OpenOptions::new()
            .append(true)
            .open(&u.file)?
            .write_all(data)?;
        u.written += data.len() as u64;
        u.last = Instant::now();
        Ok(())
    }

    pub(crate) fn write_commit(&self, upload_id: &str) -> Result<(), AvatarError> {
        let u = {
            let mut ups = self.uploads.lock().unwrap();
            match ups.remove(upload_id) {
                Some(u) => u,
                None => return invalid("unknown upload_id"),
            }
        };
        let checked = if u.written != u.size {
            Err(format!("upload is {} of {} bytes", u.written, u.size))
        } else {
            match sha256_file(&u.file) {
                Ok(h) if h == u.sha256 => Ok(()),
                Ok(_) => Err("sha256 does not match".to_string()),
                Err(e) => {
                    let _ = fs::remove_file(&u.file);
                    return Err(e.into());
                }
            }
        };
        if let Err(m) = checked {
            let _ = fs::remove_file(&u.file);
            return invalid(m);
        }
        let dest = self.dir.join(&u.path);
        let placed = (|| -> std::io::Result<()> {
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent)?;
            }
            if fs::rename(&u.file, &dest).is_err() {
                // staging may be on another filesystem: copy beside the
                // destination, then rename within it
                let tmp = PathBuf::from(format!("{}.uploading", dest.display()));
                let r = fs::copy(&u.file, &tmp).and_then(|_| fs::rename(&tmp, &dest));
                if r.is_err() {
                    let _ = fs::remove_file(&tmp);
                }
                r?;
                let _ = fs::remove_file(&u.file);
            }
            Ok(())
        })();
        if placed.is_err() {
            let _ = fs::remove_file(&u.file);
        }
        Ok(placed?)
    }

    pub(crate) fn set_config(&self, config: &AvatarConfig) -> Result<(), AvatarError> {
        let refs = config
            .model
            .iter()
            .chain(config.idle.iter())
            .chain(config.actions.values());
        for p in refs {
            validate_path(p)?;
            if !self.dir.join(p).is_file() {
                return invalid(format!("{p} does not exist in avatar/"));
            }
        }
        let text = toml::to_string_pretty(config)
            .map_err(|e| AvatarError::Invalid(format!("{CONFIG_FILE}: {e}")))?;
        fs::create_dir_all(&self.dir)?;
        let tmp = self.dir.join(format!("{CONFIG_FILE}.tmp"));
        fs::write(&tmp, text)?;
        if let Err(e) = fs::rename(&tmp, self.dir.join(CONFIG_FILE)) {
            let _ = fs::remove_file(&tmp);
            return Err(e.into());
        }
        Ok(())
    }

    pub(crate) fn remove(&self, path: &str) -> Result<(), AvatarError> {
        validate_path(path)?;
        let cfg = self.read_config()?;
        let used = cfg
            .model
            .iter()
            .chain(cfg.idle.iter())
            .chain(cfg.actions.values())
            .any(|p| p == path);
        if used {
            return invalid(format!("{path} is in use by {CONFIG_FILE}"));
        }
        let full = self.dir.join(path);
        if !full.is_file() {
            return invalid(format!("no such file: {path}"));
        }
        fs::remove_file(full)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn sha(b: &[u8]) -> String {
        crate::image_cache::sha256_hex(b)
    }

    struct Fx {
        _t: tempfile::TempDir,
        store: AvatarStore,
        dir: PathBuf,
        staging: PathBuf,
    }

    fn fx() -> Fx {
        let t = tempfile::tempdir().unwrap();
        let dir = t.path().join("avatar");
        let staging = t.path().join("staging");
        Fx {
            store: AvatarStore::new(dir.clone(), staging.clone()),
            _t: t,
            dir,
            staging,
        }
    }

    fn put(f: &Fx, path: &str, data: &[u8]) {
        let id = f
            .store
            .write_begin(path, data.len() as u64, &sha(data))
            .unwrap();
        f.store.write_chunk(&id, 0, data).unwrap();
        f.store.write_commit(&id).unwrap();
    }

    fn staged(f: &Fx) -> usize {
        std::fs::read_dir(&f.staging)
            .map(|d| d.count())
            .unwrap_or(0)
    }

    #[test]
    fn an_empty_workspace_has_an_empty_manifest() {
        let f = fx();
        assert_eq!(f.store.manifest().unwrap(), AvatarManifest::default());
    }

    #[test]
    fn paths_that_escape_or_name_the_config_are_refused() {
        for p in [
            "",
            "/abs",
            "../x",
            "a/../b",
            "a//b",
            "a\\b",
            "C:x",
            "avatar.toml",
            "./a",
        ] {
            assert!(
                matches!(validate_path(p), Err(AvatarError::Invalid(_))),
                "{p}"
            );
        }
        assert!(validate_path("actions/peace.vrma").is_ok());
    }

    #[test]
    fn an_upload_lands_only_on_commit() {
        let f = fx();
        let id = f
            .store
            .write_begin("model.vrm", 6, &sha(b"abcdef"))
            .unwrap();
        f.store.write_chunk(&id, 0, b"abc").unwrap();
        assert!(!f.dir.join("model.vrm").exists());
        f.store.write_chunk(&id, 3, b"def").unwrap();
        f.store.write_commit(&id).unwrap();
        assert_eq!(std::fs::read(f.dir.join("model.vrm")).unwrap(), b"abcdef");
        let m = f.store.manifest().unwrap();
        assert_eq!(
            m.files,
            vec![AvatarFile {
                path: "model.vrm".into(),
                size: 6,
                sha256: sha(b"abcdef")
            }]
        );
        assert_eq!(staged(&f), 0);
    }

    #[test]
    fn a_hash_mismatch_discards_the_upload() {
        let f = fx();
        let id = f.store.write_begin("model.vrm", 3, &sha(b"xyz")).unwrap();
        f.store.write_chunk(&id, 0, b"abc").unwrap();
        assert!(matches!(
            f.store.write_commit(&id),
            Err(AvatarError::Invalid(_))
        ));
        assert!(!f.dir.join("model.vrm").exists());
        assert_eq!(staged(&f), 0);
        assert!(f.store.write_commit(&id).is_err());
    }

    #[test]
    fn out_of_order_or_oversized_chunks_are_refused() {
        let f = fx();
        let id = f.store.write_begin("a.vrm", 4, &sha(b"abcd")).unwrap();
        assert!(matches!(
            f.store.write_chunk(&id, 2, b"cd"),
            Err(AvatarError::Invalid(_))
        ));
        f.store.write_chunk(&id, 0, b"abc").unwrap();
        assert!(matches!(
            f.store.write_chunk(&id, 3, b"de"),
            Err(AvatarError::Invalid(_))
        ));
        // the oversized chunk dropped the upload
        assert_eq!(staged(&f), 0);
        assert!(f.store.write_chunk(&id, 3, b"d").is_err());
        assert!(matches!(
            f.store
                .write_begin("big.vrm", AVATAR_MAX_FILE + 1, &sha(b"")),
            Err(AvatarError::Invalid(_))
        ));
    }

    #[test]
    fn uploads_idle_for_ten_minutes_are_swept() {
        let f = fx();
        let id = f.store.write_begin("a.vrm", 1, &sha(b"a")).unwrap();
        let now = Instant::now();
        f.store.sweep(now + Duration::from_secs(9 * 60));
        assert_eq!(staged(&f), 1);
        f.store.sweep(now + Duration::from_secs(11 * 60));
        assert_eq!(staged(&f), 0);
        assert!(f.store.write_chunk(&id, 0, b"a").is_err());
    }

    #[test]
    fn read_returns_chunks_and_eof() {
        let f = fx();
        put(&f, "actions/a.vrma", b"abcdef");
        assert_eq!(
            f.store.read("actions/a.vrma", 0, 4).unwrap(),
            (b"abcd".to_vec(), false)
        );
        assert_eq!(
            f.store.read("actions/a.vrma", 4, 4).unwrap(),
            (b"ef".to_vec(), true)
        );
        assert!(f.store.read("actions/a.vrma", 0, AVATAR_CHUNK + 1).is_err());
        assert!(f.store.read("../x", 0, 1).is_err());
        assert!(f.store.read("missing", 0, 1).is_err());
    }

    #[test]
    fn set_config_needs_existing_files_and_round_trips() {
        let f = fx();
        put(&f, "model.vrm", b"m");
        put(&f, "actions/peace.vrma", b"p");
        let bad = AvatarConfig {
            model: Some("nope.vrm".into()),
            ..Default::default()
        };
        assert!(matches!(
            f.store.set_config(&bad),
            Err(AvatarError::Invalid(_))
        ));
        let cfg = AvatarConfig {
            model: Some("model.vrm".into()),
            idle: None,
            actions: [("peace".to_string(), "actions/peace.vrma".to_string())].into(),
        };
        f.store.set_config(&cfg).unwrap();
        let m = f.store.manifest().unwrap();
        assert_eq!(m.config, cfg);
        assert!(m.files.iter().all(|x| x.path != "avatar.toml"));
        assert_eq!(m.files.len(), 2);
        std::fs::write(f.dir.join("avatar.toml"), "model = [").unwrap();
        assert!(matches!(
            f.store.manifest(),
            Err(AvatarError::Invalid(m)) if m.starts_with("avatar.toml")
        ));
    }

    #[test]
    fn a_file_in_use_cannot_be_removed() {
        let f = fx();
        put(&f, "model.vrm", b"m");
        put(&f, "spare.vrma", b"s");
        f.store
            .set_config(&AvatarConfig {
                model: Some("model.vrm".into()),
                ..Default::default()
            })
            .unwrap();
        assert!(matches!(
            f.store.remove("model.vrm"),
            Err(AvatarError::Invalid(m)) if m.contains("in use by avatar.toml")
        ));
        f.store.remove("spare.vrma").unwrap();
        assert!(!f.dir.join("spare.vrma").exists());
        assert!(f.store.remove("spare.vrma").is_err());
    }
}
