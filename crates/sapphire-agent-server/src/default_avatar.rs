//! Installer for the default avatar pack: safe extraction of the release
//! archive into `<workspace>/avatar/`.

#![cfg_attr(not(test), allow(dead_code))]

use std::fs::{self, File};
use std::io::{self, Read};
use std::path::Path;

use anyhow::{Result, anyhow, bail};
use sapphire_agent_core::AVATAR_MAX_FILE;

use crate::avatar_store::validate_path;

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
}
