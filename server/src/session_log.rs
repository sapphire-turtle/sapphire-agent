//! The on-disk shape both session stores share: one directory per
//! session, one Markdown file per event.
//!
//! ```text
//! <sessions>/<namespace>/<kind>/<session-dir>/
//!   session.md        the session's metadata, written once at creation
//!   <uuidv7>.md       one event: YAML front matter, then its text as the body
//! ```
//!
//! One file per event is what lets two hosts append to a synced session at
//! the same moment without ever writing the same file. An event file is
//! written once and never rewritten — a title change or a close is a new
//! event, not an edit of `session.md`.
//!
//! **The parent chain is the authority on order**, not the id's timestamp
//! and not the directory listing (which has no order). Every event records
//! the event it was appended after; UUIDv7 embeds the writer's clock, so
//! two hosts with skewed clocks produce ids that sort wrongly against each
//! other. See decision 3.1 of
//! `docs/superpowers/specs/2026-09-02-session-tool-persistence-design.md`.
//!
//! The body is the event's text, verbatim: the file is `---`, the YAML,
//! `---`, a blank line, then the text. A reader strips exactly that one
//! blank line, so text that itself starts with a line break round-trips.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use chrono::{DateTime, Utc};
use sapphire_framework::frontmatter;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tracing::warn;
use uuid::Uuid;

/// The metadata file inside a session directory.
pub(crate) const META_FILE: &str = "session.md";

/// A message part list as front matter holds it: the parts in order, one of
/// which may be the body of the file.
///
/// A message that is exactly one text part — nearly every message — writes
/// no list at all: the body *is* the message. Anything else lists its parts,
/// with the first text part replaced by `body` so the readable text still
/// sits in the body rather than in a YAML string.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) enum BodyOr<P> {
    #[serde(rename = "body")]
    Body,
    #[serde(untagged)]
    Part(P),
}

/// Split `parts` into what front matter lists and the body text.
///
/// `None` for the list means "the body is the only part". `text_of` says
/// whether a part is text the body may hold.
pub(crate) fn split_body<P>(
    parts: Vec<P>,
    text_of: impl Fn(&P) -> Option<&str>,
) -> (Option<Vec<BodyOr<P>>>, String) {
    if parts.len() == 1
        && let Some(text) = text_of(&parts[0])
    {
        return (None, text.to_owned());
    }
    let mut body = String::new();
    let mut taken = false;
    let listed = parts
        .into_iter()
        .map(|p| {
            if !taken && let Some(text) = text_of(&p) {
                body = text.to_owned();
                taken = true;
                BodyOr::Body
            } else {
                BodyOr::Part(p)
            }
        })
        .collect();
    (Some(listed), body)
}

/// The inverse of [`split_body`].
pub(crate) fn join_body<P>(
    listed: Option<Vec<BodyOr<P>>>,
    body: String,
    text: impl Fn(String) -> P,
) -> Vec<P> {
    let Some(listed) = listed else {
        return vec![text(body)];
    };
    let mut body = Some(body);
    listed
        .into_iter()
        .map(|p| match p {
            // A second `body` marker has nothing left to stand for; an
            // empty text keeps the part count rather than dropping one.
            BodyOr::Body => text(body.take().unwrap_or_default()),
            BodyOr::Part(p) => p,
        })
        .collect()
}

/// One event file, read back.
#[derive(Debug, Clone)]
pub(crate) struct EntryFile<F> {
    /// The file name, which is the event's id.
    pub id: Uuid,
    pub front: F,
    pub body: String,
}

/// What every event's front matter carries, whatever else it holds.
pub(crate) trait Linked {
    fn parent(&self) -> Option<Uuid>;
    fn at(&self) -> DateTime<Utc>;
}

/// Write `contents` to `path` through a temporary file in the same
/// directory, so a sync layer or a crash never sees half a file. The
/// temporary name starts with a dot, which every reader here skips.
fn write_atomic(path: &Path, contents: &str) -> Result<()> {
    let dir = path.parent().context("an event file has a directory")?;
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .context("an event file has a name")?;
    let tmp = dir.join(format!(".{name}.{}.tmp", Uuid::now_v7()));
    fs::write(&tmp, contents).with_context(|| format!("writing {}", tmp.display()))?;
    fs::rename(&tmp, path).with_context(|| format!("renaming into {}", path.display()))?;
    Ok(())
}

fn render<F: Serialize>(front: &F, body: &str) -> Result<String> {
    let mut out = frontmatter::render(front, "")?;
    if !body.is_empty() {
        out.push('\n');
        out.push_str(body);
    }
    Ok(out)
}

fn parse<F: DeserializeOwned>(raw: &str) -> Result<(F, String)> {
    let (front, body) = frontmatter::parse::<F>(raw)?;
    let body = body
        .strip_prefix("\r\n")
        .or_else(|| body.strip_prefix('\n'))
        .unwrap_or(body);
    Ok((front, body.to_owned()))
}

/// Write `session.md`. Refuses to overwrite: metadata is written once.
pub(crate) fn write_meta<M: Serialize>(dir: &Path, meta: &M) -> Result<PathBuf> {
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = dir.join(META_FILE);
    if path.exists() {
        anyhow::bail!("{} already exists", path.display());
    }
    write_atomic(&path, &render(meta, "")?)?;
    Ok(path)
}

pub(crate) fn read_meta<M: DeserializeOwned>(dir: &Path) -> Option<M> {
    let path = dir.join(META_FILE);
    let raw = fs::read_to_string(&path).ok()?;
    match parse::<M>(&raw) {
        Ok((meta, _)) => Some(meta),
        Err(e) => {
            warn!("Unreadable session metadata {}: {e}", path.display());
            None
        }
    }
}

/// Write one event as `<dir>/<id>.md` and return its path.
pub(crate) fn write_entry<F: Serialize>(
    dir: &Path,
    id: Uuid,
    front: &F,
    body: &str,
) -> Result<PathBuf> {
    let path = dir.join(format!("{id}.md"));
    write_atomic(&path, &render(front, body)?)?;
    Ok(path)
}

/// Every event file in `dir`, in no particular order. Unreadable ones are
/// skipped with a warning; so are files whose name is not a UUID, which is
/// what lets a README or a sync tool's bookkeeping sit alongside.
pub(crate) fn read_entries<F: DeserializeOwned>(dir: &Path) -> Vec<EntryFile<F>> {
    let Ok(rd) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in rd.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name.starts_with('.') || name == META_FILE {
            continue;
        }
        let Some(stem) = name.strip_suffix(".md") else {
            continue;
        };
        let Ok(id) = stem.parse::<Uuid>() else {
            continue;
        };
        let path = entry.path();
        let raw = match fs::read_to_string(&path) {
            Ok(raw) => raw,
            Err(e) => {
                warn!("Skipping unreadable session event {}: {e}", path.display());
                continue;
            }
        };
        match parse::<F>(&raw) {
            Ok((front, body)) => out.push(EntryFile { id, front, body }),
            Err(e) => warn!("Skipping malformed session event {}: {e}", path.display()),
        }
    }
    out
}

/// Whether `dir` is a session directory.
pub(crate) fn is_session_dir(dir: &Path) -> bool {
    dir.join(META_FILE).is_file()
}

/// The events on the parent chain, in order, starting from the root.
///
/// Where two events share a parent — two writers appended to the session
/// without seeing each other's latest event — the earlier one by
/// `(at, id)` is followed and the fork is reported. Events off the chain
/// are not lost from disk; callers that need every event (the daily log)
/// take the ones this leaves out as well.
pub(crate) fn chain<'a, F: Linked>(
    label: &str,
    entries: &'a [EntryFile<F>],
) -> Vec<&'a EntryFile<F>> {
    let mut children: HashMap<Option<Uuid>, Vec<&EntryFile<F>>> = HashMap::new();
    for entry in entries {
        children
            .entry(entry.front.parent())
            .or_default()
            .push(entry);
    }
    for kids in children.values_mut() {
        kids.sort_by_key(|e| (e.front.at(), e.id));
    }
    let mut out = Vec::with_capacity(entries.len());
    let mut cursor = None;
    while let Some(next) = children.get(&cursor) {
        if next.len() > 1 {
            warn!(
                "session {label}: {} events share one parent; following the earliest. Two \
                 writers appended to this session without seeing each other.",
                next.len()
            );
        }
        let entry = next[0];
        out.push(entry);
        cursor = Some(entry.id);
        // A hand-edited or partially-synced directory could hold a cycle.
        // The chain cannot be longer than the directory.
        if out.len() > entries.len() {
            warn!("session {label}: the parent chain cycles; stopping");
            break;
        }
    }
    out
}

/// Move a freshly built session directory into place.
///
/// Conversions build into a hidden sibling first and rename it here, so a
/// reader never sees a half-converted session — and since the directory is
/// authoritative once it exists, a half-converted one would silently lose
/// the rest. Returns `Ok(false)` when `dest` already exists (another
/// process converted it first); `staging` is removed then.
pub(crate) fn publish_dir(staging: &Path, dest: &Path) -> Result<bool> {
    if dest.exists() {
        fs::remove_dir_all(staging).ok();
        return Ok(false);
    }
    match fs::rename(staging, dest) {
        Ok(()) => Ok(true),
        Err(_) if dest.exists() => {
            fs::remove_dir_all(staging).ok();
            Ok(false)
        }
        Err(e) => Err(e).with_context(|| format!("moving {} into place", dest.display())),
    }
}

/// A hidden staging directory beside `dest` for [`publish_dir`].
pub(crate) fn staging_dir_for(dest: &Path) -> Result<PathBuf> {
    let parent = dest.parent().context("a session directory has a parent")?;
    let name = dest
        .file_name()
        .and_then(|n| n.to_str())
        .context("a session directory has a name")?;
    Ok(parent.join(format!(".{name}.{}.converting", Uuid::now_v7())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    struct Front {
        parent: Option<Uuid>,
        at: DateTime<Utc>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parts: Option<Vec<BodyOr<crate::provider::ContentPart>>>,
    }

    impl Linked for Front {
        fn parent(&self) -> Option<Uuid> {
            self.parent
        }
        fn at(&self) -> DateTime<Utc> {
            self.at
        }
    }

    fn front(parent: Option<Uuid>) -> Front {
        Front {
            parent,
            at: Utc::now(),
            parts: None,
        }
    }

    #[test]
    fn a_body_round_trips_byte_for_byte() {
        let tmp = tempfile::tempdir().unwrap();
        for body in [
            "plain",
            "\nstarts with a newline",
            "ends\n\n",
            "---\nnot a fence",
            "",
        ] {
            let id = Uuid::now_v7();
            write_entry(tmp.path(), id, &front(None), body).unwrap();
            let read: Vec<EntryFile<Front>> = read_entries(tmp.path());
            let got = read.iter().find(|e| e.id == id).unwrap();
            assert_eq!(got.body, body);
        }
    }

    #[test]
    fn parts_round_trip_with_the_first_text_as_the_body() {
        use crate::provider::ContentPart;
        let parts = vec![
            ContentPart::ToolUseRef {
                id: "c1".into(),
                name: "file_read".into(),
                sha256: Some("ab".into()),
            },
            ContentPart::Text("the readable part".into()),
            ContentPart::Text("a second text".into()),
        ];
        fn text_of(p: &ContentPart) -> Option<&str> {
            match p {
                ContentPart::Text(t) => Some(t.as_str()),
                _ => None,
            }
        }
        let (listed, body) = split_body(parts.clone(), text_of);
        assert_eq!(body, "the readable part");

        let tmp = tempfile::tempdir().unwrap();
        let id = Uuid::now_v7();
        let f = Front {
            parts: listed,
            ..front(None)
        };
        write_entry(tmp.path(), id, &f, &body).unwrap();
        let raw = fs::read_to_string(tmp.path().join(format!("{id}.md"))).unwrap();
        assert!(raw.contains("- body"), "{raw}");

        let read: Vec<EntryFile<Front>> = read_entries(tmp.path());
        let back = join_body(
            read[0].front.parts.clone(),
            read[0].body.clone(),
            ContentPart::Text,
        );
        assert_eq!(back, parts);
    }

    #[test]
    fn a_lone_text_part_writes_no_list() {
        let (listed, body) = split_body(vec!["hi".to_owned()], |p| Some(p.as_str()));
        assert!(listed.is_none());
        assert_eq!(join_body(listed, body, |s| s), vec!["hi".to_owned()]);
    }

    #[test]
    fn no_parts_stay_no_parts() {
        let (listed, body) = split_body(Vec::<String>::new(), |p| Some(p.as_str()));
        assert_eq!(join_body(listed, body, |s| s), Vec::<String>::new());
    }

    #[test]
    fn the_chain_orders_by_parent_not_by_clock() {
        let tmp = tempfile::tempdir().unwrap();
        let a = Uuid::now_v7();
        let b = Uuid::now_v7();
        // `b` claims an earlier clock than `a` but was appended after it.
        write_entry(tmp.path(), a, &front(None), "a").unwrap();
        let mut fb = front(Some(a));
        fb.at = Utc::now() - chrono::Duration::hours(1);
        write_entry(tmp.path(), b, &fb, "b").unwrap();

        let entries: Vec<EntryFile<Front>> = read_entries(tmp.path());
        let order: Vec<&str> = chain("t", &entries)
            .iter()
            .map(|e| e.body.as_str())
            .collect();
        assert_eq!(order, ["a", "b"]);
    }

    #[test]
    fn a_fork_follows_the_earliest_branch() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Uuid::now_v7();
        write_entry(tmp.path(), root, &front(None), "root").unwrap();
        write_entry(tmp.path(), Uuid::now_v7(), &front(Some(root)), "first").unwrap();
        write_entry(tmp.path(), Uuid::now_v7(), &front(Some(root)), "second").unwrap();

        let entries: Vec<EntryFile<Front>> = read_entries(tmp.path());
        let walked: Vec<&str> = chain("t", &entries)
            .iter()
            .map(|e| e.body.as_str())
            .collect();
        assert_eq!(walked, ["root", "first"]);
    }

    #[test]
    fn readers_skip_metadata_temporaries_and_foreign_files() {
        let tmp = tempfile::tempdir().unwrap();
        write_meta(tmp.path(), &serde_json::json!({ "k": "v" })).unwrap();
        fs::write(tmp.path().join(".x.md.tmp"), "junk").unwrap();
        fs::write(tmp.path().join("README.md"), "notes").unwrap();
        write_entry(tmp.path(), Uuid::now_v7(), &front(None), "only").unwrap();

        let entries: Vec<EntryFile<Front>> = read_entries(tmp.path());
        assert_eq!(entries.len(), 1);
        assert!(is_session_dir(tmp.path()));
        assert!(
            write_meta(tmp.path(), &serde_json::json!({})).is_err(),
            "written once"
        );
    }
}
