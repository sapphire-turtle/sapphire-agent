//! Installer for the default avatar pack: safe extraction of the release
//! archive into `<workspace>/avatar/`.

#![cfg_attr(not(test), allow(dead_code))]

use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow, bail};
use sapphire_agent_core::{AVATAR_MAX_FILE, AvatarConfig};

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
    fs::create_dir_all(&staging)?;
    let result = stage_and_place(archive, &staging, &avatar_dir);
    let _ = fs::remove_dir_all(&staging);
    result.map(|()| Outcome::Installed)
}

fn stage_and_place(archive: &Path, staging: &Path, avatar_dir: &Path) -> Result<()> {
    extract(File::open(archive)?, staging)?;

    let config_text = fs::read_to_string(staging.join(CONFIG_FILE))
        .map_err(|e| anyhow!("archive has no readable {CONFIG_FILE}: {e}"))?;
    let config: AvatarConfig = toml::from_str(&config_text)?;
    config
        .check()
        .map_err(|e| anyhow!("invalid {CONFIG_FILE}: {e}"))?;
    for p in config.paths() {
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
fn place_one(src: &Path, dest: &Path, placed: &mut Vec<PathBuf>) -> Result<()> {
    if dest.exists() {
        bail!("{} already exists; not overwriting", dest.display());
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

#[cfg(test)]
mod tests {
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
}
