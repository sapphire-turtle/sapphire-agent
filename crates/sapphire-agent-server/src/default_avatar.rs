//! Installer for the default avatar pack: safe extraction of the release
//! archive into `<workspace>/avatar/`.

use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use futures_util::FutureExt as _;
use futures_util::future::{BoxFuture, Shared};
use sapphire_agent_core::{AVATAR_MAX_FILE, AvatarConfig, DefaultAvatarAnswer, InstallProgress};

use crate::avatar_store::{sha256_file, validate_path};

/// Upper bound on the summed size of all files in the archive.
pub(crate) const MAX_AVATAR_TOTAL: u64 = 100_000_000;

/// Archive directory every entry must live under.
const PREFIX: &str = "avatar/";
const CONFIG_FILE: &str = "avatar.toml";

/// Extract a tar archive of an `avatar/` tree into `dest` (which must exist
/// and be empty), placing files at `dest/<path without "avatar/">`.
fn extract(archive: impl Read, dest: &Path) -> Result<()> {
    extract_with(archive, dest, AVATAR_MAX_FILE, MAX_AVATAR_TOTAL)
}

fn extract_with(archive: impl Read, dest: &Path, max_file: u64, max_total: u64) -> Result<()> {
    let mut archive = tar::Archive::new(archive);
    let mut total: u64 = 0;
    for entry in archive.entries()? {
        let entry = entry?;
        let raw = entry.path_bytes().into_owned();
        let name = std::str::from_utf8(&raw).map_err(|_| anyhow!("entry path is not UTF-8"))?;
        let Some(rest) = name.strip_prefix(PREFIX) else {
            bail!("entry {name:?} is outside {PREFIX}");
        };
        let ty = entry.header().entry_type();
        let is_dir = ty.is_dir();
        let rest = if is_dir {
            rest.strip_suffix('/').unwrap_or(rest)
        } else {
            rest
        };
        if rest.is_empty() {
            continue; // the `avatar/` directory itself
        }
        if rest != CONFIG_FILE {
            validate_path(rest).map_err(|e| anyhow!("entry {name:?}: {e}"))?;
        }
        // `rest` is validated and `/`-separated; join it component-wise so
        // the result is right on Windows too.
        let mut target = dest.to_path_buf();
        target.extend(rest.split('/'));
        if is_dir {
            fs::create_dir_all(&target)?;
        } else if ty.is_file() || ty.is_contiguous() {
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut out = File::create(&target)?;
            let written = io::copy(&mut entry.take(max_file + 1), &mut out)?;
            if written > max_file {
                bail!("entry {name:?} exceeds {max_file} bytes");
            }
            total += written;
            if total > max_total {
                bail!("archive exceeds {max_total} bytes in total");
            }
        } else {
            bail!("entry {name:?} has unsupported type {ty:?}");
        }
    }
    Ok(())
}

/// What `install_archive` did.
#[derive(Debug, PartialEq)]
pub(crate) enum Outcome {
    Installed,
    AlreadyPresent,
}

/// Verify `archive` against `expected_sha256`, extract and check it in a
/// staging directory under `staging_root`, then place it into
/// `<workspace_root>/avatar/`. Never overwrites: an existing
/// `avatar/avatar.toml` means nothing is done, and an existing destination
/// file aborts the install. `avatar.toml` is placed last, so a failed install
/// never leaves a config pointing at missing files.
pub(crate) fn install_archive(
    workspace_root: &Path,
    archive: &Path,
    expected_sha256: &str,
    staging_root: &Path,
) -> Result<Outcome> {
    let avatar_dir = workspace_root.join("avatar");
    if avatar_dir.join(CONFIG_FILE).exists() {
        return Ok(Outcome::AlreadyPresent);
    }
    let actual = sha256_file(archive)?;
    if !actual.eq_ignore_ascii_case(expected_sha256) {
        bail!("archive sha256 is {actual}, expected {expected_sha256}");
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let staging = staging_root.join(format!("{}-{nanos}", std::process::id()));
    fs::create_dir(&staging)?;
    let result = stage_and_place(archive, &staging, &avatar_dir);
    let _ = fs::remove_dir_all(&staging);
    result.map(|()| Outcome::Installed)
}

fn stage_and_place(archive: &Path, staging: &Path, avatar_dir: &Path) -> Result<()> {
    extract(File::open(archive)?, staging)?;

    let config_text = fs::read_to_string(staging.join(CONFIG_FILE))
        .map_err(|e| anyhow!("archive has no readable {CONFIG_FILE}: {e}"))?;
    let config: AvatarConfig =
        toml::from_str(&config_text).map_err(|e| anyhow!("invalid {CONFIG_FILE}: {e}"))?;
    config
        .check()
        .map_err(|e| anyhow!("invalid {CONFIG_FILE}: {e}"))?;
    for p in config.paths() {
        validate_path(p).map_err(|e| anyhow!("{CONFIG_FILE}: {e}"))?;
        if !join_rel(staging, p).is_file() {
            bail!("{CONFIG_FILE} references {p:?}, which the archive lacks");
        }
    }

    let mut rels = Vec::new();
    collect_files(staging, "", &mut rels)?;
    rels.sort(); // deterministic placing order
    rels.retain(|r| r != CONFIG_FILE);
    rels.push(CONFIG_FILE.to_string()); // last

    let mut placed: Vec<PathBuf> = Vec::new();
    let r = rels.iter().try_for_each(|rel| {
        place_one(
            rel,
            &join_rel(staging, rel),
            &join_rel(avatar_dir, rel),
            &mut placed,
        )
    });
    if r.is_err() {
        for p in &placed {
            let _ = fs::remove_file(p);
            let mut dir = p.parent();
            while let Some(d) = dir.filter(|d| *d != avatar_dir) {
                if fs::remove_dir(d).is_err() {
                    break;
                }
                dir = d.parent();
            }
        }
    }
    r
}

/// Join a validated `/`-separated relative path component-wise.
fn join_rel(base: &Path, rel: &str) -> PathBuf {
    let mut p = base.to_path_buf();
    p.extend(rel.split('/'));
    p
}

/// List files under `dir` as `/`-separated paths relative to the root.
fn collect_files(dir: &Path, prefix: &str, out: &mut Vec<String>) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| anyhow!("non-UTF-8 file name in staging"))?;
        let rel = format!("{prefix}{name}");
        if entry.file_type()?.is_dir() {
            collect_files(&entry.path(), &format!("{rel}/"), out)?;
        } else {
            out.push(rel);
        }
    }
    Ok(())
}

/// Move `src` to `dest` without overwriting; record it in `placed`.
fn place_one(rel: &str, src: &Path, dest: &Path, placed: &mut Vec<PathBuf>) -> Result<()> {
    if dest.exists() {
        bail!("avatar/{rel} already exists; not overwriting");
    }
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    if fs::rename(src, dest).is_err() {
        // Likely a cross-device move: copy next to the destination, then
        // rename, so a half-written file never has the final name.
        let name = dest.file_name().unwrap_or_default().to_os_string();
        let mut tmp_name = std::ffi::OsString::from(".");
        tmp_name.push(&name);
        tmp_name.push(".installing");
        let tmp = dest.with_file_name(tmp_name);
        let r = fs::copy(src, &tmp).and_then(|_| fs::rename(&tmp, dest));
        if let Err(e) = r {
            let _ = fs::remove_file(&tmp);
            return Err(e.into());
        }
    }
    placed.push(dest.to_path_buf());
    Ok(())
}

/// Download cap: the archive's content cap plus tar headers and padding.
const MAX_DOWNLOAD: u64 = MAX_AVATAR_TOTAL + 8 * 1024 * 1024;

/// A published avatar pack release.
#[derive(Clone, Copy)]
pub(crate) struct AvatarRelease<'a> {
    pub id: &'a str,
    pub version: u32,
    pub url: &'a str,
    pub sha256: &'a str,
}

/// The default avatar pack installed on first run; `None` until a release
/// exists.
// FIXME(#340): the first release's URL and `.sha256`
pub(crate) const INITIAL: Option<AvatarRelease<'static>> = None;

/// Where downloaded archives and their extraction are staged.
fn staging_root() -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("sapphire-agent")
        .join("avatar-staging")
}

/// Download `url` into `dest`, calling `progress(received, total)` after each
/// chunk. Fails (removing `dest`) if the body exceeds `max` bytes.
fn fetch(
    url: &str,
    dest: &Path,
    max: u64,
    mut progress: impl FnMut(u64, Option<u64>),
) -> Result<()> {
    let result = fetch_inner(url, dest, max, &mut progress);
    if result.is_err() {
        let _ = fs::remove_file(dest);
    }
    result
}

fn fetch_inner(
    url: &str,
    dest: &Path,
    max: u64,
    progress: &mut dyn FnMut(u64, Option<u64>),
) -> Result<()> {
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(30))
        .build()?;
    let mut resp = client.get(url).send()?.error_for_status()?;
    let total = resp.content_length();
    if let Some(t) = total
        && t > max
    {
        bail!("download is {t} bytes, over the {max} byte limit");
    }
    let mut out = File::create(dest)?;
    let mut buf = vec![0u8; 64 * 1024];
    let mut received: u64 = 0;
    loop {
        let n = resp.read(&mut buf)?;
        if n == 0 {
            break;
        }
        received += n as u64;
        if received > max {
            bail!("download exceeds the {max} byte limit");
        }
        io::Write::write_all(&mut out, &buf[..n])?;
        progress(received, total);
    }
    Ok(())
}

/// Download `release` and install it into `<workspace_root>/avatar/`.
/// Does not touch the network when an avatar is already present.
///
/// Blocking (uses `reqwest::blocking`); call it from `spawn_blocking` or a
/// plain thread, never from async code.
pub(crate) fn install(
    workspace_root: &Path,
    release: &AvatarRelease<'_>,
    progress: impl FnMut(u64, Option<u64>),
) -> Result<Outcome> {
    if workspace_root.join("avatar").join(CONFIG_FILE).exists() {
        return Ok(Outcome::AlreadyPresent);
    }
    let root = staging_root();
    fs::create_dir_all(&root)?;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let tar = root.join(format!(
        "{}-v{}-{}-{nanos}.tar",
        release.id,
        release.version,
        std::process::id()
    ));
    let result = fetch(release.url, &tar, MAX_DOWNLOAD, progress)
        .and_then(|()| install_archive(workspace_root, &tar, release.sha256, &root));
    let _ = fs::remove_file(&tar);
    result
}

/// What the sync setup looks like, for deciding on an automatic install.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SyncView {
    pub enabled: bool,
    pub peers: usize,
    pub bridge_available: bool,
}

/// Whether the avatar may be downloaded without asking: sync is off, or it is
/// on and certain to have no peer that might already hold an avatar.
pub(crate) fn auto_allowed(sync: SyncView) -> bool {
    !sync.enabled || (sync.bridge_available && sync.peers == 0)
}

/// Looks up the sync setup when the install decision needs it.
#[async_trait::async_trait]
pub(crate) trait SyncProbe: Send + Sync {
    async fn view(&self) -> SyncView;
}

/// Reads the sync setup from the framework's runtime.
pub(crate) struct RuntimeProbe {
    pub runtime: Arc<sapphire_framework::server::SyncRuntime>,
    pub root: PathBuf,
}

#[async_trait::async_trait]
impl SyncProbe for RuntimeProbe {
    async fn view(&self) -> SyncView {
        let s = self.runtime.status(&self.root).await;
        SyncView {
            enabled: s.enabled,
            peers: s.peers,
            bridge_available: s.bridge_available,
        }
    }
}

/// Lets an event through at most once per interval.
struct Throttle {
    last: Option<Instant>,
}

impl Throttle {
    const EVERY: Duration = Duration::from_millis(250);

    fn new() -> Self {
        Self { last: None }
    }

    /// True when an event at `now` should be let through.
    fn due(&mut self, now: Instant) -> bool {
        if self
            .last
            .is_some_and(|t| now.duration_since(t) < Self::EVERY)
        {
            return false;
        }
        self.last = Some(now);
        true
    }
}

type Flight = Shared<BoxFuture<'static, Result<DefaultAvatarAnswer, String>>>;

/// Installs the default avatar, one download at a time.
pub(crate) struct DefaultInstaller {
    root: PathBuf,
    release: Option<AvatarRelease<'static>>,
    probe: Arc<dyn SyncProbe>,
    in_flight: Arc<Mutex<Option<Flight>>>,
}

impl DefaultInstaller {
    pub(crate) fn new(
        root: PathBuf,
        release: Option<AvatarRelease<'static>>,
        probe: Arc<dyn SyncProbe>,
    ) -> Self {
        Self {
            root,
            release,
            probe,
            in_flight: Arc::new(Mutex::new(None)),
        }
    }

    /// Decide whether to install, and do it unless another request already is.
    /// With `auto`, a sync setup that might hold an avatar elsewhere answers
    /// `NeedsConfirmation` instead. Only the request that starts the download
    /// gets `progress`; the ones that join it get just the answer.
    pub(crate) async fn run(
        &self,
        auto: bool,
        progress: Arc<dyn Fn(InstallProgress) + Send + Sync>,
    ) -> Result<DefaultAvatarAnswer, String> {
        let Some(release) = self.release.as_ref() else {
            return Ok(DefaultAvatarAnswer::Unavailable);
        };
        if self.root.join("avatar").join(CONFIG_FILE).exists() {
            return Ok(DefaultAvatarAnswer::AlreadyPresent);
        }
        if auto && !auto_allowed(self.probe.view().await) {
            return Ok(DefaultAvatarAnswer::NeedsConfirmation);
        }
        let flight = {
            let mut slot = self.in_flight.lock().unwrap_or_else(|e| e.into_inner());
            match slot.as_ref() {
                Some(f) => f.clone(),
                None => {
                    let f = self.start(release, progress);
                    *slot = Some(f.clone());
                    f
                }
            }
        };
        flight.await
    }

    /// Spawn the install. It runs to the end even if every waiter goes away,
    /// and clears the in-flight slot when it finishes.
    fn start(
        &self,
        release: &AvatarRelease<'static>,
        progress: Arc<dyn Fn(InstallProgress) + Send + Sync>,
    ) -> Flight {
        let root = self.root.clone();
        let release = *release;
        let slot = Arc::clone(&self.in_flight);
        let handle = tokio::spawn(async move {
            let joined = tokio::task::spawn_blocking(move || {
                let mut throttle = Throttle::new();
                let mut unsent = None;
                let outcome = install(&root, &release, |received, total| {
                    let p = InstallProgress { received, total };
                    if throttle.due(Instant::now()) {
                        unsent = None;
                        progress(p);
                    } else {
                        unsent = Some(p);
                    }
                });
                // The end is only known now; make sure the last step is seen.
                if let Some(p) = unsent {
                    progress(p);
                }
                outcome.map_err(|e| format!("{e:#}"))
            })
            .await;
            *slot.lock().unwrap_or_else(|e| e.into_inner()) = None;
            match joined {
                Ok(Ok(Outcome::Installed)) => Ok(DefaultAvatarAnswer::Installed),
                Ok(Ok(Outcome::AlreadyPresent)) => Ok(DefaultAvatarAnswer::AlreadyPresent),
                Ok(Err(msg)) => Err(msg),
                Err(e) => Err(format!("install task failed: {e}")),
            }
        });
        async move {
            handle
                .await
                .unwrap_or_else(|e| Err(format!("install task failed: {e}")))
        }
        .boxed()
        .shared()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) enum Kind {
        File,
        Dir,
        Symlink(&'static str),
        HardLink(&'static str),
    }

    /// Build a tar by writing header name bytes directly, bypassing
    /// `tar::Builder`'s own path checks so hostile names can be produced.
    pub(crate) fn tar_of(entries: &[(&str, Kind, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        for (name, kind, data) in entries {
            let mut h = tar::Header::new_old();
            h.as_old_mut().name[..name.len()].copy_from_slice(name.as_bytes());
            h.set_mode(0o644);
            let (ty, link, size) = match kind {
                Kind::File => (tar::EntryType::Regular, None, data.len()),
                Kind::Dir => (tar::EntryType::Directory, None, 0),
                Kind::Symlink(t) => (tar::EntryType::Symlink, Some(*t), 0),
                Kind::HardLink(t) => (tar::EntryType::Link, Some(*t), 0),
            };
            h.set_entry_type(ty);
            h.set_size(size as u64);
            if let Some(t) = link {
                h.as_old_mut().linkname[..t.len()].copy_from_slice(t.as_bytes());
            }
            h.set_cksum();
            out.extend_from_slice(h.as_bytes());
            if size > 0 {
                out.extend_from_slice(data);
                out.resize(out.len().div_ceil(512) * 512, 0);
            }
        }
        out.extend_from_slice(&[0u8; 1024]);
        out
    }

    /// `<tmp>/dest` inside a fresh parent so escapes are observable.
    fn dest() -> (tempfile::TempDir, std::path::PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path().join("dest");
        fs::create_dir(&d).unwrap();
        (tmp, d)
    }

    #[test]
    fn extract_places_files_without_prefix() {
        let (_t, d) = dest();
        let tar = tar_of(&[
            ("avatar/", Kind::Dir, b""),
            ("avatar/avatar.toml", Kind::File, b"cfg"),
            ("avatar/sprite/idle.webp", Kind::File, b"img"),
        ]);
        extract(&tar[..], &d).unwrap();
        assert_eq!(fs::read(d.join("avatar.toml")).unwrap(), b"cfg");
        assert_eq!(fs::read(d.join("sprite/idle.webp")).unwrap(), b"img");
    }

    #[test]
    fn extract_rejects_parent_component() {
        let (t, d) = dest();
        let tar = tar_of(&[("avatar/../evil", Kind::File, b"x")]);
        assert!(extract(&tar[..], &d).is_err());
        assert!(!t.path().join("evil").exists());
    }

    #[test]
    fn extract_rejects_outside_prefix() {
        let (_t, d) = dest();
        let tar = tar_of(&[("other/a.webp", Kind::File, b"x")]);
        assert!(extract(&tar[..], &d).is_err());
    }

    #[test]
    fn extract_rejects_symlink() {
        let (_t, d) = dest();
        let tar = tar_of(&[("avatar/link", Kind::Symlink("/etc/passwd"), b"")]);
        assert!(extract(&tar[..], &d).is_err());
    }

    #[test]
    fn extract_rejects_hardlink() {
        let (_t, d) = dest();
        let tar = tar_of(&[("avatar/link", Kind::HardLink("avatar/a.webp"), b"")]);
        assert!(extract(&tar[..], &d).is_err());
    }

    #[test]
    fn extract_rejects_hidden_name() {
        let (_t, d) = dest();
        let tar = tar_of(&[("avatar/.DS_Store", Kind::File, b"x")]);
        assert!(extract(&tar[..], &d).is_err());
    }

    #[test]
    fn extract_rejects_total_over_cap() {
        let (_t, d) = dest();
        let tar = tar_of(&[
            ("avatar/a.webp", Kind::File, &[0u8; 10]),
            ("avatar/b.webp", Kind::File, &[0u8; 10]),
            ("avatar/c.webp", Kind::File, &[0u8; 10]),
        ]);
        assert!(extract_with(&tar[..], &d, 10, 25).is_err());
    }

    #[test]
    fn extract_rejects_file_over_cap() {
        let (_t, d) = dest();
        let tar = tar_of(&[("avatar/a.webp", Kind::File, &[0u8; 11])]);
        assert!(extract_with(&tar[..], &d, 10, 25).is_err());
    }

    const CFG: &[u8] =
        b"[sprite]\ndefault_pose = \"idle\"\n[sprite.poses.idle]\nbase = \"sprite/idle.webp\"\n";

    /// Write `entries` as a tar in `dir`; returns its path and sha256.
    fn archive_in(dir: &Path, entries: &[(&str, Kind, &[u8])]) -> (std::path::PathBuf, String) {
        let p = dir.join("pack.tar");
        fs::write(&p, tar_of(entries)).unwrap();
        let h = crate::avatar_store::sha256_file(&p).unwrap();
        (p, h)
    }

    fn good_entries() -> Vec<(&'static str, Kind, &'static [u8])> {
        vec![
            ("avatar/avatar.toml", Kind::File, CFG),
            ("avatar/sprite/", Kind::Dir, b""),
            ("avatar/sprite/idle.webp", Kind::File, b"img"),
            ("avatar/profile.md", Kind::File, b"hi"),
        ]
    }

    /// (tempdir, workspace, staging_root)
    fn env() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
        let t = tempfile::tempdir().unwrap();
        let ws = t.path().join("ws");
        let st = t.path().join("staging");
        fs::create_dir(&ws).unwrap();
        fs::create_dir(&st).unwrap();
        (t, ws, st)
    }

    #[test]
    fn install_archive_installs() {
        let (t, ws, st) = env();
        let (a, h) = archive_in(t.path(), &good_entries());
        assert_eq!(
            install_archive(&ws, &a, &h, &st).unwrap(),
            Outcome::Installed
        );
        assert!(ws.join("avatar/avatar.toml").is_file());
        assert_eq!(
            fs::read(ws.join("avatar/sprite/idle.webp")).unwrap(),
            b"img"
        );
        assert!(ws.join("avatar/profile.md").is_file());
    }

    #[test]
    fn install_archive_skips_when_present() {
        let (t, ws, st) = env();
        fs::create_dir(ws.join("avatar")).unwrap();
        fs::write(ws.join("avatar/avatar.toml"), "x").unwrap();
        let (a, h) = archive_in(t.path(), &good_entries());
        assert_eq!(
            install_archive(&ws, &a, &h, &st).unwrap(),
            Outcome::AlreadyPresent
        );
        assert_eq!(
            fs::read_to_string(ws.join("avatar/avatar.toml")).unwrap(),
            "x"
        );
    }

    #[test]
    fn install_archive_rejects_bad_hash() {
        let (t, ws, st) = env();
        let (a, _) = archive_in(t.path(), &good_entries());
        assert!(install_archive(&ws, &a, &"0".repeat(64), &st).is_err());
        assert!(!ws.join("avatar").exists());
    }

    #[test]
    fn install_archive_rejects_missing_referenced_file() {
        let (t, ws, st) = env();
        let (a, h) = archive_in(t.path(), &[("avatar/avatar.toml", Kind::File, CFG)]);
        assert!(install_archive(&ws, &a, &h, &st).is_err());
        assert!(!ws.join("avatar/avatar.toml").exists());
    }

    #[test]
    fn install_archive_rejects_escaping_reference() {
        let (t, ws, st) = env();
        // `../x` from staging resolves to a real file beside it.
        fs::write(st.join("x"), b"outside").unwrap();
        let cfg = b"[sprite]\ndefault_pose = \"idle\"\n[sprite.poses.idle]\nbase = \"../x\"\n";
        let (a, h) = archive_in(t.path(), &[("avatar/avatar.toml", Kind::File, cfg)]);
        assert!(install_archive(&ws, &a, &h, &st).is_err());
        assert!(!ws.join("avatar/avatar.toml").exists());
    }

    #[test]
    fn install_archive_rejects_invalid_config() {
        let (t, ws, st) = env();
        let cfg = b"[sprite]\ndefault_pose = \"nope\"\n[sprite.poses.idle]\nbase = \"sprite/idle.webp\"\n";
        let (a, h) = archive_in(
            t.path(),
            &[
                ("avatar/avatar.toml", Kind::File, cfg),
                ("avatar/sprite/idle.webp", Kind::File, b"img"),
            ],
        );
        assert!(install_archive(&ws, &a, &h, &st).is_err());
        assert!(!ws.join("avatar/avatar.toml").exists());
    }

    #[test]
    fn install_refuses_to_overwrite_existing_file() {
        let (t, ws, st) = env();
        fs::create_dir_all(ws.join("avatar/sprite")).unwrap();
        fs::write(ws.join("avatar/sprite/idle.webp"), "mine").unwrap();
        let (a, h) = archive_in(t.path(), &good_entries());
        assert!(install_archive(&ws, &a, &h, &st).is_err());
        assert_eq!(
            fs::read(ws.join("avatar/sprite/idle.webp")).unwrap(),
            b"mine"
        );
        assert!(!ws.join("avatar/avatar.toml").exists());
        // `profile.md` sorts before `sprite/idle.webp`, so it was placed
        // first and must have been rolled back.
        assert!(!ws.join("avatar/profile.md").exists());
    }

    #[test]
    fn install_into_existing_empty_dir() {
        let (t, ws, st) = env();
        fs::create_dir(ws.join("avatar")).unwrap();
        let (a, h) = archive_in(t.path(), &good_entries());
        assert_eq!(
            install_archive(&ws, &a, &h, &st).unwrap(),
            Outcome::Installed
        );
    }

    #[test]
    fn staging_dir_is_removed() {
        let (t, ws, st) = env();
        let (a, h) = archive_in(t.path(), &good_entries());
        install_archive(&ws, &a, &h, &st).unwrap();
        let ws2 = t.path().join("ws2");
        fs::create_dir(&ws2).unwrap();
        assert!(install_archive(&ws2, &a, &"0".repeat(64), &st).is_err());
        assert_eq!(fs::read_dir(&st).unwrap().count(), 0);
        // Failures after extraction (missing referenced file) clean up too.
        let (bad, bh) = archive_in(t.path(), &[("avatar/avatar.toml", Kind::File, CFG)]);
        assert!(install_archive(&ws2, &bad, &bh, &st).is_err());
        assert_eq!(fs::read_dir(&st).unwrap().count(), 0);
    }

    /// Answer one HTTP request with 200 and `body`; returns the URL.
    fn serve_once(body: Vec<u8>, content_length: Option<usize>) -> String {
        use std::io::Write;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let Ok((mut s, _)) = listener.accept() else {
                return;
            };
            let mut buf = [0u8; 4096];
            let mut got = Vec::new();
            while !got.windows(4).any(|w| w == b"\r\n\r\n") {
                match s.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => got.extend_from_slice(&buf[..n]),
                }
            }
            let mut head = String::from("HTTP/1.1 200 OK\r\nConnection: close\r\n");
            if let Some(n) = content_length {
                head.push_str(&format!("Content-Length: {n}\r\n"));
            }
            head.push_str("\r\n");
            let _ = s.write_all(head.as_bytes());
            let _ = s.write_all(&body);
        });
        format!("http://127.0.0.1:{port}/a.tar")
    }

    #[test]
    fn fetch_writes_body_and_reports_progress() {
        let (t, _, _) = env();
        let body: Vec<u8> = (0..204_800u32).map(|i| (i % 251) as u8).collect();
        let url = serve_once(body.clone(), Some(body.len()));
        let dest = t.path().join("out.tar");
        let mut last = None;
        fetch(&url, &dest, 1_000_000, |r, tot| last = Some((r, tot))).unwrap();
        assert_eq!(fs::read(&dest).unwrap(), body);
        assert_eq!(last, Some((204_800, Some(204_800))));
    }

    #[test]
    fn fetch_aborts_past_cap() {
        let (t, _, _) = env();
        let url = serve_once(vec![7u8; 5000], None);
        let dest = t.path().join("out.tar");
        assert!(fetch(&url, &dest, 1000, |_, _| {}).is_err());
        assert!(!dest.exists());
    }

    #[test]
    fn fetch_rejects_declared_size_over_cap() {
        let (t, _, _) = env();
        let url = serve_once(vec![7u8; 5000], Some(5000));
        let dest = t.path().join("out.tar");
        let mut calls = 0;
        assert!(fetch(&url, &dest, 1000, |_, _| calls += 1).is_err());
        assert_eq!(calls, 0);
        assert!(!dest.exists());
    }

    #[test]
    fn install_end_to_end() {
        let (t, ws, _) = env();
        let (a, h) = archive_in(t.path(), &good_entries());
        let body = fs::read(&a).unwrap();
        let url = serve_once(body.clone(), Some(body.len()));
        let release = AvatarRelease {
            id: "test-e2e",
            version: 1,
            url: &url,
            sha256: &h,
        };
        assert_eq!(
            install(&ws, &release, |_, _| {}).unwrap(),
            Outcome::Installed
        );
        assert!(ws.join("avatar/avatar.toml").is_file());
        assert!(ws.join("avatar/sprite/idle.webp").is_file());
    }

    #[test]
    fn install_skips_without_network_when_present() {
        let (_t, ws, _) = env();
        fs::create_dir_all(ws.join("avatar")).unwrap();
        fs::write(ws.join("avatar/avatar.toml"), CFG).unwrap();
        let release = AvatarRelease {
            id: "x",
            version: 1,
            url: "http://127.0.0.1:9/",
            sha256: &"0".repeat(64),
        };
        assert_eq!(
            install(&ws, &release, |_, _| {}).unwrap(),
            Outcome::AlreadyPresent
        );
    }

    /// Serve `body` to every request, counting them in `hits`.
    pub(crate) fn serve_counting(
        body: Vec<u8>,
        hits: Arc<std::sync::atomic::AtomicUsize>,
    ) -> String {
        use std::io::Write;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            while let Ok((mut s, _)) = listener.accept() {
                hits.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let mut buf = [0u8; 4096];
                let mut got = Vec::new();
                while !got.windows(4).any(|w| w == b"\r\n\r\n") {
                    match s.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => got.extend_from_slice(&buf[..n]),
                    }
                }
                let head = format!(
                    "HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Length: {}\r\n\r\n",
                    body.len()
                );
                let _ = s.write_all(head.as_bytes());
                let _ = s.write_all(&body);
                // Slow enough that concurrent runs overlap.
                std::thread::sleep(Duration::from_millis(100));
            }
        });
        format!("http://127.0.0.1:{port}/a.tar")
    }

    pub(crate) struct FixedProbe(pub Option<SyncView>);

    #[async_trait::async_trait]
    impl SyncProbe for FixedProbe {
        async fn view(&self) -> SyncView {
            self.0.expect("the probe must not be asked")
        }
    }

    pub(crate) fn view(enabled: bool, peers: usize, bridge_available: bool) -> SyncView {
        SyncView {
            enabled,
            peers,
            bridge_available,
        }
    }

    fn no_progress() -> Arc<dyn Fn(InstallProgress) + Send + Sync> {
        Arc::new(|_| {})
    }

    /// A release for `url` whose hash matches `sha256`; leaked for `'static`.
    pub(crate) fn release_for(url: String, sha256: &str) -> AvatarRelease<'static> {
        AvatarRelease {
            id: "test-installer",
            version: 1,
            url: Box::leak(url.into_boxed_str()),
            sha256: Box::leak(sha256.to_string().into_boxed_str()),
        }
    }

    /// A served good pack: (tempdir, workspace, release, hits, tar length).
    pub(crate) fn served() -> (
        tempfile::TempDir,
        PathBuf,
        AvatarRelease<'static>,
        Arc<std::sync::atomic::AtomicUsize>,
        usize,
    ) {
        let (t, ws, _) = env();
        let (a, h) = archive_in(t.path(), &good_entries());
        let body = fs::read(&a).unwrap();
        let len = body.len();
        let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let url = serve_counting(body, hits.clone());
        (t, ws, release_for(url, &h), hits, len)
    }

    fn installer(
        ws: &Path,
        release: Option<AvatarRelease<'static>>,
        v: Option<SyncView>,
    ) -> DefaultInstaller {
        DefaultInstaller::new(ws.to_path_buf(), release, Arc::new(FixedProbe(v)))
    }

    #[test]
    fn auto_allowed_follows_the_sync_rule() {
        assert!(auto_allowed(view(false, 3, false)));
        assert!(auto_allowed(view(true, 0, true)));
        assert!(!auto_allowed(view(true, 1, true)));
        assert!(!auto_allowed(view(true, 0, false)));
    }

    #[tokio::test]
    async fn run_without_a_release_is_unavailable() {
        let (_t, ws, _) = env();
        let i = installer(&ws, None, None);
        assert_eq!(
            i.run(false, no_progress()).await,
            Ok(DefaultAvatarAnswer::Unavailable)
        );
    }

    #[tokio::test]
    async fn run_with_an_avatar_is_already_present_without_probing() {
        let (_t, ws, release, hits, _) = served();
        fs::create_dir_all(ws.join("avatar")).unwrap();
        fs::write(ws.join("avatar/avatar.toml"), CFG).unwrap();
        let i = installer(&ws, Some(release), None);
        assert_eq!(
            i.run(true, no_progress()).await,
            Ok(DefaultAvatarAnswer::AlreadyPresent)
        );
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn run_auto_with_peers_needs_confirmation_and_fetches_nothing() {
        let (_t, ws, release, hits, _) = served();
        let i = installer(&ws, Some(release), Some(view(true, 1, true)));
        assert_eq!(
            i.run(true, no_progress()).await,
            Ok(DefaultAvatarAnswer::NeedsConfirmation)
        );
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert!(!ws.join("avatar").exists());
    }

    #[tokio::test]
    async fn run_not_auto_ignores_the_sync_rule() {
        let (_t, ws, release, _, _) = served();
        let i = installer(&ws, Some(release), Some(view(true, 1, true)));
        assert_eq!(
            i.run(false, no_progress()).await,
            Ok(DefaultAvatarAnswer::Installed)
        );
        assert!(ws.join("avatar/avatar.toml").is_file());
    }

    #[tokio::test]
    async fn concurrent_runs_download_once_and_share_the_answer() {
        let (_t, ws, release, hits, _) = served();
        let i = installer(&ws, Some(release), Some(view(false, 0, true)));
        let (a, b) = tokio::join!(i.run(false, no_progress()), i.run(false, no_progress()));
        assert_eq!(a, Ok(DefaultAvatarAnswer::Installed));
        assert_eq!(b, Ok(DefaultAvatarAnswer::Installed));
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_failed_run_reports_install_s_error_and_leaves_no_avatar() {
        let (_t, ws, release, _, _) = served();
        let bad = release_for(release.url.to_string(), &"0".repeat(64));
        let i = installer(&ws, Some(bad), Some(view(false, 0, true)));
        let err = i.run(false, no_progress()).await.unwrap_err();
        assert!(err.contains("sha256"), "{err}");
        assert!(!ws.join("avatar/avatar.toml").exists());
    }

    #[tokio::test]
    async fn progress_reports_the_total_and_ends_at_the_full_size() {
        let (_t, ws, release, _, len) = served();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = seen.clone();
        let i = installer(&ws, Some(release), Some(view(false, 0, true)));
        i.run(false, Arc::new(move |p| sink.lock().unwrap().push(p)))
            .await
            .unwrap();
        let last = *seen.lock().unwrap().last().unwrap();
        assert_eq!(last.received, len as u64);
        assert_eq!(last.total, Some(len as u64));
    }

    #[test]
    fn progress_is_throttled() {
        let t0 = Instant::now();
        let mut th = Throttle::new();
        assert!(th.due(t0));
        assert!(!th.due(t0 + Duration::from_millis(100)));
        assert!(th.due(t0 + Duration::from_millis(260)));
    }
}
