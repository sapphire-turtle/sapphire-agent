//! Session persistence: one directory per session, one Markdown file per
//! event.
//!
//! ```text
//! <workspace>/sessions/<namespace>/<kind>/<session_id>/
//!   session.md       metadata (room, channel, created_at, …), written once
//!   <uuidv7>.md      one event — a message, a compaction summary, a title, a close
//! ```
//!
//! The format itself — front matter, the body, the parent chain that
//! orders events — is `session_log`'s, shared with the ACP store. What is
//! here is what this store's events are and how its callers query them.
//!
//! Sessions written before this format are single `.jsonl` files. They are
//! read as they are, and converted the first time something is written to
//! them, so only sessions that are actually continued get converted. Once
//! the directory exists it is authoritative and the `.jsonl` beside it is
//! ignored; it is never deleted. TODO(#284): remove the `.jsonl` reader and
//! the conversion once a bulk conversion has shipped.

use crate::provider::{ChatMessage, ContentPart, Role, UserInputKind};
use crate::session_log::{self, BodyOr, EntryFile, Linked};
use base64::{Engine, engine::general_purpose::STANDARD as BASE64_STANDARD};
use chrono::{DateTime, Duration, Local, NaiveDate, TimeZone, Timelike, Utc};
use sapphire_framework::workspace::WorkspaceState;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tracing::warn;
use uuid::Uuid;

pub type ConversationKey = (String, Option<String>);

// ---------------------------------------------------------------------------
// Stored types
// ---------------------------------------------------------------------------

/// Metadata written as the first line of each session file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMeta {
    pub session_id: String,
    pub room_id: String,
    pub thread_id: Option<String>,
    pub channel: String,
    pub created_at: DateTime<Utc>,
    /// Human-readable alias (grain-id, 7 chars). Only set for API sessions.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub public_id: Option<String>,
    /// Memory namespace this session writes/reads under, captured at
    /// session creation so cross-session digest builders can route digests
    /// to the correct namespace even for sessions where the namespace is
    /// not derivable from `room_id` (e.g. API/voice sessions pinning a
    /// non-default room_profile). `None` for legacy files predating this
    /// field; consumers fall back to room-id derivation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    /// Originating device id for device-default sessions (kind =
    /// `"device-default"`). Combined with `room_profile` below, this
    /// pair is the routing key — `find_or_create_for_device` returns
    /// the most-recent file matching both. Absent on every other kind.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub device_id: Option<String>,
    /// Resolved room_profile name for device-default sessions. Pinned
    /// at creation time (from the bearer token's `DeviceAuth` resolution:
    /// token -> device -> room profile) so rotation and
    /// `find_or_create_for_device` don't accidentally hand a session to a
    /// different profile after `[room_profile]` reshuffles. Absent on
    /// every other kind.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub room_profile: Option<String>,
    /// Short auto-generated title, populated from a later `session_title` line.
    #[serde(skip)]
    pub title: Option<String>,
}

/// A single stored message: `ChatMessage` + wall-clock timestamp.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredMessage {
    /// Stable identity for this line, generated at write time.
    ///
    /// The compaction checkpoint (`SummaryLine::covers_through`) points
    /// at one of these. A timestamp would be the obvious cursor and is a
    /// worse one: coarse system clocks repeat a value across two rapid
    /// appends, and an NTP step backwards makes timestamps non-monotonic,
    /// either of which silently drops messages from a replay.
    ///
    /// In the directory format this is the event file's name, and order
    /// comes from the `parent` each event records (see `session_log`). It
    /// is an identity, not a sort key.
    ///
    /// `None` only on lines of a legacy `.jsonl` written before this field
    /// existed; readers fall back to file order there, and the conversion
    /// gives each one an id (decision 3.1 of the design doc).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub id: Option<Uuid>,
    pub timestamp: DateTime<Utc>,
    pub role: Role,
    pub parts: Vec<ContentPart>,
    /// Input modality for user-role messages. `None` for legacy
    /// pre-existing JSONL (read via `serde(default)`), for assistant
    /// replies, and for synthetic user lines that don't have a
    /// meaningful modality (e.g. heartbeat-injected pushes).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub input_kind: Option<UserInputKind>,
    /// Authenticated user id, when the inbound transport has mapped
    /// the message to a known identity. Always `None` today;
    /// reserved for API-key / channel-ID → user_id mapping work.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub user_id: Option<String>,
}

impl StoredMessage {
    #[cfg(test)]
    pub fn from_chat(msg: &ChatMessage) -> Self {
        Self {
            id: Some(Uuid::now_v7()),
            timestamp: Utc::now(),
            role: msg.role.clone(),
            parts: msg.parts.clone(),
            input_kind: msg.input_kind.clone(),
            user_id: msg.user_id.clone(),
        }
    }

    pub fn into_chat_message(self) -> ChatMessage {
        ChatMessage {
            role: self.role,
            parts: self.parts,
            input_kind: self.input_kind,
            user_id: self.user_id,
        }
    }
}

// Legacy `.jsonl` line wrappers, read-only (TODO(#284)) ---------------------

#[derive(Serialize, Deserialize)]
struct MetaLine {
    meta: SessionMeta,
}

// Event files --------------------------------------------------------------

/// The front matter of one event file. The id is the file name; the body is
/// the message text, the summary, or nothing.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Front {
    Message {
        parent: Option<Uuid>,
        at: DateTime<Utc>,
        role: Role,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        input_kind: Option<UserInputKind>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        user_id: Option<String>,
        /// Absent when the body is the whole message.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parts: Option<Vec<BodyOr<ContentPart>>>,
    },
    Summary {
        parent: Option<Uuid>,
        at: DateTime<Utc>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        covers_through: Option<Uuid>,
    },
    Title {
        parent: Option<Uuid>,
        at: DateTime<Utc>,
        title: String,
    },
    Closed {
        parent: Option<Uuid>,
        at: DateTime<Utc>,
    },
}

impl Linked for Front {
    fn parent(&self) -> Option<Uuid> {
        match self {
            Front::Message { parent, .. }
            | Front::Summary { parent, .. }
            | Front::Title { parent, .. }
            | Front::Closed { parent, .. } => *parent,
        }
    }
    fn at(&self) -> DateTime<Utc> {
        match self {
            Front::Message { at, .. }
            | Front::Summary { at, .. }
            | Front::Title { at, .. }
            | Front::Closed { at, .. } => *at,
        }
    }
}

fn text_of(part: &ContentPart) -> Option<&str> {
    match part {
        ContentPart::Text(t) => Some(t.as_str()),
        _ => None,
    }
}

/// One event, whichever format it was read from, in session order.
enum Event {
    Message(StoredMessage),
    Summary(SummaryLine),
    Title(String),
    Closed,
}

impl Event {
    fn from_entry(entry: &EntryFile<Front>) -> Event {
        match &entry.front {
            Front::Message {
                at,
                role,
                input_kind,
                user_id,
                parts,
                ..
            } => Event::Message(StoredMessage {
                id: Some(entry.id),
                timestamp: *at,
                role: role.clone(),
                parts: session_log::join_body(parts.clone(), entry.body.clone(), ContentPart::Text),
                input_kind: input_kind.clone(),
                user_id: user_id.clone(),
            }),
            Front::Summary {
                at, covers_through, ..
            } => Event::Summary(SummaryLine {
                summary_at: *at,
                summary: entry.body.clone(),
                up_to_timestamp: None,
                covers_through: *covers_through,
            }),
            Front::Title { title, .. } => Event::Title(title.clone()),
            Front::Closed { .. } => Event::Closed,
        }
    }
}

/// Where a session lives on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Location {
    /// A session directory.
    Dir(PathBuf),
    /// A legacy single-file session. TODO(#284).
    Jsonl(PathBuf),
}

impl Location {
    pub(crate) fn path(&self) -> &Path {
        match self {
            Location::Dir(p) | Location::Jsonl(p) => p,
        }
    }

    /// The name a session is looked up by: the directory's name, or the
    /// `.jsonl` file's stem.
    fn name(&self) -> Option<&str> {
        match self {
            Location::Dir(p) => p.file_name()?.to_str(),
            Location::Jsonl(p) => p.file_stem()?.to_str(),
        }
    }
}

/// Compacted recap of a session, appended whenever a compaction fires.
/// `covers_through` makes the latest `SummaryLine` the restore cursor: a
/// reload replays this summary as a stub, followed only by the messages it
/// did not absorb, rather than the whole raw history.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SummaryLine {
    pub summary_at: DateTime<Utc>,
    pub summary: String,
    /// Unused: always written as `None` and read nowhere. `covers_through`
    /// took over its stated job as the restore cursor. Kept (rather than
    /// removed) because removing a field is a format change; don't
    /// implement against this one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub up_to_timestamp: Option<DateTime<Utc>>,
    /// The last message this summary absorbed. A restore replays the
    /// messages *after* it, prefixed with the summary as a stub — which
    /// reproduces the in-memory state at the moment of compaction rather
    /// than replaying a whole session and paying for the compaction all
    /// over again on the first turn back.
    ///
    /// `None` on lines written before this field existed. Those are read
    /// as "this summary stands in for everything before it in file
    /// order", which is what a shutdown summary — almost certainly what
    /// such a line is — actually meant.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub covers_through: Option<Uuid>,
}

// ---------------------------------------------------------------------------
// SessionStore
// ---------------------------------------------------------------------------

/// How a [`SessionStore`] names its session directories.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileNaming {
    /// `{session_id}`. Every store but the autonomous one.
    Plain,
    /// `{date}-{session_id}`, `date` being the agent-day (per
    /// `boundary_hour`) the session was created in.
    Dated { boundary_hour: u8 },
}

pub struct SessionStore {
    /// Base sessions directory (e.g. `<workspace>/sessions`). Session
    /// directories live under `<base_dir>/<namespace>/<kind>/<session_id>/`
    /// — the namespace split is mechanical (matches `memory/<namespace>/`)
    /// so retrieve indexing can scope itself by directory and never
    /// accidentally mix NSFW sessions with default-namespace ones.
    pub base_dir: PathBuf,
    /// Second-level subdirectory: `"channel"` for Matrix/Discord,
    /// `"cross-device"` for user-selectable RPC sessions,
    /// `"device-default"` for per-`(device_id, room_profile)` sessions.
    /// Lets the Agent and ServeState
    /// keep separate `SessionStore` instances while sharing one base dir.
    pub kind: &'static str,
    /// How this store names its session files.
    ///
    /// `Plain` for every store but the autonomous one. A `Dated` store
    /// prefixes the agent-day the file was created in, which is what
    /// makes an overnight run of autonomous sessions readable in a
    /// directory listing — the session's own metadata is still one line
    /// away, this is for the human doing the looking.
    naming: FileNaming,
    /// Optional sapphire-framework workspace state. When set, file
    /// modifications notify the workspace so the index/cache stay in
    /// sync.
    ws_state: Option<Arc<WorkspaceState>>,
    /// `session_id → location` cache. Populated lazily by
    /// `resolve_path` (filesystem scan) and eagerly by `create_session` /
    /// `ensure_session`. Avoids re-scanning per `append` call.
    path_cache: Mutex<HashMap<String, Location>>,
    /// `session_id` → the id of the last event written, so an append
    /// knows its parent without re-reading the session. Held across the
    /// whole read-parent / write / record-tip step, so two appends to one
    /// session in this process never fabricate a fork — the same reason
    /// as `AcpSessionStore::append_line`.
    tips: Mutex<HashMap<String, Uuid>>,
    /// Workspace-external, content-addressed store for both halves of a
    /// tool call — the `input` and the result.
    ///
    /// `None` when the platform cache directory could not be opened at
    /// startup. Degrades rather than failing: a payload written without
    /// a cache is recorded as a `ToolUseRef` / `ToolResultRef` with no
    /// hash, which reads back as the corresponding stand-in. Losing the
    /// content is survivable; losing the pairing is not.
    tool_payloads: Option<Arc<crate::tool_payload_cache::ToolPayloadCache>>,
}

impl SessionStore {
    #[allow(dead_code)]
    pub fn new(
        base_dir: PathBuf,
        kind: &'static str,
        tool_payloads: Option<Arc<crate::tool_payload_cache::ToolPayloadCache>>,
    ) -> Self {
        Self {
            base_dir,
            kind,
            naming: FileNaming::Plain,
            ws_state: None,
            path_cache: Mutex::new(HashMap::new()),
            tips: Mutex::new(HashMap::new()),
            tool_payloads,
        }
    }

    pub fn with_workspace(
        base_dir: PathBuf,
        kind: &'static str,
        ws_state: Arc<WorkspaceState>,
        tool_payloads: Option<Arc<crate::tool_payload_cache::ToolPayloadCache>>,
    ) -> Self {
        Self {
            base_dir,
            kind,
            naming: FileNaming::Plain,
            ws_state: Some(ws_state),
            path_cache: Mutex::new(HashMap::new()),
            tips: Mutex::new(HashMap::new()),
            tool_payloads,
        }
    }

    /// Name this store's files by agent-day. See [`FileNaming`].
    ///
    /// A builder rather than a constructor argument because exactly one
    /// store wants it and every other call site — four in `main.rs`,
    /// eleven in tests — would otherwise have to name the `Plain`
    /// default explicitly.
    pub fn with_dated_files(mut self, boundary_hour: u8) -> Self {
        self.naming = FileNaming::Dated { boundary_hour };
        self
    }

    /// Compute (without filesystem checks) the directory a new session
    /// should live at. Used by `create_session` / `ensure_session`. Also
    /// seeds the path cache so subsequent `append` calls hit it directly.
    fn path_for_new(&self, session_id: &str, namespace: &str) -> PathBuf {
        let dir_name = match self.naming {
            FileNaming::Plain => session_id.to_string(),
            FileNaming::Dated { boundary_hour } => {
                let date = local_date_for_timestamp(Local::now(), boundary_hour);
                format!("{date}-{session_id}")
            }
        };
        let p = self.base_dir.join(namespace).join(self.kind).join(dir_name);
        if let Ok(mut cache) = self.path_cache.lock() {
            cache.insert(session_id.to_string(), Location::Dir(p.clone()));
        }
        p
    }

    /// Where an existing session lives — its directory, or its legacy
    /// `.jsonl` until something is written to it. For callers that show
    /// or test for the location rather than going through the store's
    /// own accessors.
    pub fn absolute_path_for(&self, session_id: &str) -> Option<PathBuf> {
        self.resolve_path(session_id)
            .map(|l| l.path().to_path_buf())
    }

    /// When a session was created, from its metadata alone.
    pub fn created_at(&self, session_id: &str) -> Option<DateTime<Utc>> {
        meta_at(&self.resolve_path(session_id)?).map(|m| m.created_at)
    }

    /// Locate an existing session by id, scanning every namespace
    /// subdirectory under `<base_dir>/<*>/<kind>/`. Returns `None` if it
    /// isn't found. Hot path for `append`-style methods, so cached.
    fn resolve_path(&self, session_id: &str) -> Option<Location> {
        if let Ok(cache) = self.path_cache.lock()
            && let Some(p) = cache.get(session_id)
        {
            return Some(p.clone());
        }
        for loc in collect_sessions(&self.base_dir, self.kind) {
            let Some(name) = loc.name() else {
                continue;
            };
            let hit = match self.naming {
                FileNaming::Plain => name == session_id,
                // A v7 UUID is 36 characters with four hyphens, so
                // `-{session_id}` with a 10-character date in front of
                // it cannot collide with another session's name.
                FileNaming::Dated { .. } => name
                    .strip_suffix(&format!("-{session_id}"))
                    .is_some_and(|date| date.len() == 10),
            };
            if hit {
                if let Ok(mut cache) = self.path_cache.lock() {
                    cache.insert(session_id.to_string(), loc.clone());
                }
                return Some(loc);
            }
        }
        None
    }

    /// Test-only: the resolver, so a test can assert which file an id
    /// maps to when two candidates exist.
    #[cfg(test)]
    pub fn resolve_path_for_test(&self, session_id: &str) -> Option<PathBuf> {
        self.absolute_path_for(session_id)
    }

    /// Notify the sapphire-framework workspace that a session file was created or modified.
    /// No-op if no WorkspaceState is attached or the path is outside the workspace.
    fn notify_updated(&self, abs_path: &Path) {
        let Some(state) = &self.ws_state else { return };
        let guard = state;
        if !abs_path.starts_with(&guard.workspace.root) {
            return;
        }
        if let Err(e) = guard.on_file_updated(abs_path) {
            warn!(
                "Failed to notify workspace of update {}: {e}",
                abs_path.display()
            );
        }
    }

    /// Notify the sapphire-framework workspace that a session file was deleted.
    #[allow(dead_code)]
    fn notify_deleted(&self, abs_path: &Path) {
        let Some(state) = &self.ws_state else { return };
        let guard = state;
        if !abs_path.starts_with(&guard.workspace.root) {
            return;
        }
        if let Err(e) = guard.on_file_deleted(abs_path) {
            warn!(
                "Failed to notify workspace of delete {}: {e}",
                abs_path.display()
            );
        }
    }

    /// Delete a session (used when an empty session is discarded).
    #[allow(dead_code)]
    pub fn delete_session(&self, session_id: &str) -> anyhow::Result<()> {
        match self.resolve_path(session_id) {
            Some(Location::Dir(dir)) => {
                let files: Vec<PathBuf> = fs::read_dir(&dir)
                    .map(|rd| rd.flatten().map(|e| e.path()).collect())
                    .unwrap_or_default();
                fs::remove_dir_all(&dir)?;
                for f in files {
                    self.notify_deleted(&f);
                }
            }
            Some(Location::Jsonl(path)) => {
                fs::remove_file(&path)?;
                self.notify_deleted(&path);
            }
            None => {}
        }
        if let Ok(mut cache) = self.path_cache.lock() {
            cache.remove(session_id);
        }
        if let Ok(mut tips) = self.tips.lock() {
            tips.remove(session_id);
        }
        Ok(())
    }

    /// Write a new session's directory and metadata.
    fn create_dir(&self, meta: &SessionMeta, namespace: &str) -> anyhow::Result<()> {
        let dir = self.path_for_new(&meta.session_id, namespace);
        let path = session_log::write_meta(&dir, meta)?;
        self.notify_updated(&path);
        Ok(())
    }

    /// Create a new session for `key`. Returns the new session_id (UUIDv7).
    pub fn create_session(
        &self,
        key: &ConversationKey,
        channel: &str,
        namespace: &str,
    ) -> anyhow::Result<String> {
        let session_id = Uuid::now_v7().to_string();
        let meta = SessionMeta {
            session_id: session_id.clone(),
            room_id: key.0.clone(),
            thread_id: key.1.clone(),
            channel: channel.to_string(),
            created_at: Utc::now(),
            public_id: None,
            namespace: Some(namespace.to_string()),
            device_id: None,
            room_profile: None,
            title: None,
        };
        self.create_dir(&meta, namespace)?;
        Ok(session_id)
    }

    /// Create one autonomous session for a task. `room_id` carries the
    /// task name — that is the reverse index the loop searches by, and
    /// it is why there is no separate state file for "which session is
    /// this task in" (spec decision 3).
    ///
    /// `channel` is `"server"`: the same value a `/rpc` session uses,
    /// because an autonomous session is not a chat and has no channel of
    /// its own.
    pub fn create_autonomous_session(&self, task: &str, namespace: &str) -> anyhow::Result<String> {
        self.create_session(&(task.to_string(), None), "server", namespace)
    }

    /// The directory to write `session_id`'s next event into, converting a
    /// legacy `.jsonl` session first. TODO(#284).
    fn writable_dir(&self, session_id: &str) -> anyhow::Result<PathBuf> {
        match self.resolve_path(session_id) {
            Some(Location::Dir(dir)) => Ok(dir),
            Some(Location::Jsonl(path)) => {
                let dir = convert_jsonl(&path)?;
                for entry in fs::read_dir(&dir)?.flatten() {
                    self.notify_updated(&entry.path());
                }
                if let Ok(mut cache) = self.path_cache.lock() {
                    cache.insert(session_id.to_string(), Location::Dir(dir.clone()));
                }
                Ok(dir)
            }
            None => Err(anyhow::anyhow!("Session not found for {session_id}")),
        }
    }

    /// Append one event: `make(parent)` builds it, after the current tip.
    fn append_event(
        &self,
        session_id: &str,
        make: impl FnOnce(Option<Uuid>) -> (Front, String),
    ) -> anyhow::Result<Uuid> {
        let mut tips = self.tips.lock().unwrap_or_else(|e| e.into_inner());
        let dir = self.writable_dir(session_id)?;
        let parent = match tips.get(session_id) {
            Some(tip) => Some(*tip),
            None => {
                let entries: Vec<EntryFile<Front>> = session_log::read_entries(&dir);
                session_log::chain(session_id, &entries)
                    .last()
                    .map(|e| e.id)
            }
        };
        let id = Uuid::now_v7();
        let (front, body) = make(parent);
        let path = session_log::write_entry(&dir, id, &front, &body)?;
        tips.insert(session_id.to_string(), id);
        drop(tips);
        self.notify_updated(&path);
        Ok(id)
    }

    /// Append a `ChatMessage` (with current timestamp) to an existing session.
    pub fn append(&self, session_id: &str, msg: &ChatMessage) -> anyhow::Result<()> {
        let scrubbed = self.scrub_for_storage(msg);
        let to_store = scrubbed.as_ref().unwrap_or(msg);
        self.append_stored(session_id, Utc::now(), to_store)
    }

    fn append_stored(
        &self,
        session_id: &str,
        at: DateTime<Utc>,
        msg: &ChatMessage,
    ) -> anyhow::Result<()> {
        let (parts, body) = session_log::split_body(msg.parts.clone(), text_of);
        self.append_event(session_id, |parent| {
            (
                Front::Message {
                    parent,
                    at,
                    role: msg.role.clone(),
                    input_kind: msg.input_kind.clone(),
                    user_id: msg.user_id.clone(),
                    parts,
                },
                body,
            )
        })?;
        Ok(())
    }

    /// Test-only: every file of a session, concatenated — for asserting
    /// what reached the disk.
    #[cfg(test)]
    pub(crate) fn raw_for_test(&self, session_id: &str) -> String {
        let path = self
            .absolute_path_for(session_id)
            .expect("the session exists");
        if path.is_file() {
            return fs::read_to_string(path).unwrap();
        }
        let mut out = String::new();
        for entry in fs::read_dir(path).unwrap().flatten() {
            out.push_str(&fs::read_to_string(entry.path()).unwrap_or_default());
        }
        out
    }

    /// Test-only: append a message as stored, at a chosen time — what the
    /// tests that backdate a session need.
    #[cfg(test)]
    pub(crate) fn append_at_for_test(
        &self,
        session_id: &str,
        at: DateTime<Utc>,
        msg: &ChatMessage,
    ) -> anyhow::Result<()> {
        self.append_stored(session_id, at, msg)
    }

    /// Append a compaction summary, recording how far it reaches.
    ///
    /// `keep_recent` is how many trailing messages the caller kept
    /// verbatim (0 for a day-boundary compaction, which replaces the
    /// whole history). The cursor is computed here rather than passed
    /// in: the caller holds an index into its in-memory history, and
    /// only the store can map that onto its own log, whose message
    /// count is at least as large — earlier compactions trimmed memory,
    /// not the log.
    pub fn append_summary(
        &self,
        session_id: &str,
        summary: &str,
        keep_recent: usize,
    ) -> anyhow::Result<()> {
        // Converted before the cursor is computed, so the cursor is an id
        // in the directory rather than a position in the legacy file.
        let dir = self.writable_dir(session_id)?;
        let covers_through = checkpoint_id(&Location::Dir(dir), keep_recent);
        self.append_event(session_id, |parent| {
            (
                Front::Summary {
                    parent,
                    at: Utc::now(),
                    covers_through,
                },
                summary.to_string(),
            )
        })?;
        Ok(())
    }

    /// Close a session by appending a `closed` event.
    /// The session becomes inactive; future messages create a new session.
    pub fn close_session(&self, session_id: &str) -> anyhow::Result<()> {
        self.append_event(session_id, |parent| {
            (
                Front::Closed {
                    parent,
                    at: Utc::now(),
                },
                String::new(),
            )
        })?;
        Ok(())
    }

    /// Load every active session from disk on startup.
    ///
    /// For each `ConversationKey`, picks the latest session that has no
    /// `closed` event. Returns:
    ///
    /// - `active`: which session is current per conversation
    /// - `histories`: that session's conversation as the model should
    ///   see it — the latest compaction summary as a stub, the messages
    ///   it did not absorb, tool results hydrated, pairing repaired
    ///
    /// Raw history used to be withheld on the grounds that Anthropic
    /// requires paired tool traffic and none was persisted. Both halves
    /// of that changed (#194), so the restart summary this replaced is
    /// gone: a summary costs a model call, throws the turn structure
    /// away, and answers a question the log now answers directly.
    pub fn load_all(
        &self,
    ) -> (
        HashMap<ConversationKey, String>,
        HashMap<ConversationKey, Vec<ChatMessage>>,
    ) {
        let mut entries: Vec<(String, ConversationKey)> = Vec::new();

        for loc in collect_sessions(&self.base_dir, self.kind) {
            let Some(loaded) = load(&loc) else {
                continue;
            };
            if loaded.is_closed {
                continue;
            }
            let session_id = loaded.meta.session_id.clone();
            // Seed the path cache so the first append after bootstrap
            // doesn't pay a scan.
            if let Ok(mut cache) = self.path_cache.lock() {
                cache.insert(session_id.clone(), loc.clone());
            }
            entries.push((
                session_id,
                (loaded.meta.room_id.clone(), loaded.meta.thread_id.clone()),
            ));
        }

        // Session ids are UUIDv7, so the newest session for a key sorts last.
        entries.sort_by(|a, b| a.0.cmp(&b.0));

        let mut active: HashMap<ConversationKey, String> = HashMap::new();
        let mut histories: HashMap<ConversationKey, Vec<ChatMessage>> = HashMap::new();
        for (session_id, key) in entries {
            match self.load_session(&session_id) {
                Some(history) if !history.is_empty() => {
                    histories.insert(key.clone(), history);
                }
                _ => {
                    histories.remove(&key);
                }
            }
            active.insert(key, session_id);
        }

        (active, histories)
    }

    /// List metadata for all sessions in this store (used by API for session listing).
    pub fn list_sessions(&self) -> Vec<SessionMeta> {
        let mut metas: Vec<SessionMeta> = collect_sessions(&self.base_dir, self.kind)
            .iter()
            .filter_map(|l| load(l).map(|loaded| loaded.meta))
            .collect();
        metas.sort_by_key(|m| m.created_at);
        metas
    }

    /// Listing rows for every session in this store, newest activity
    /// last. See [`SessionRow`].
    pub fn session_rows(&self) -> Vec<SessionRow> {
        let mut rows: Vec<SessionRow> = collect_sessions(&self.base_dir, self.kind)
            .iter()
            .filter_map(load_session_row)
            .collect();
        rows.sort_by_key(|r| r.last_at.unwrap_or(r.meta.created_at));
        rows
    }

    /// One session as the model should see it: the latest compaction
    /// summary rendered as a stub, then the messages it did not absorb,
    /// with tool results hydrated and any broken pairing repaired.
    ///
    /// This is the model's view, not the record as written — a checkpoint
    /// silently drops every covered message and adds a summary stub the
    /// caller never sent. A reader that wants the record as written (e.g.
    /// a client-facing endpoint handing a session back to the party that
    /// wrote it) should use `load_session_full` instead, which does not
    /// trim.
    pub fn load_session(&self, session_id: &str) -> Option<Vec<ChatMessage>> {
        let loaded = load(&self.resolve_path(session_id)?)?;
        let (summary, tail) = model_history(&loaded);
        let mut out = summary
            .map(crate::context_compression::compaction_stub)
            .unwrap_or_default();
        out.extend(tail.iter().cloned().map(|m| m.into_chat_message()));
        Some(crate::session_storage::repair_tool_pairing(
            self.hydrate(out),
        ))
    }

    /// Load a session's messages exactly as written, preserving wall-clock
    /// timestamps, alongside the latest `SummaryLine` if one has been
    /// written. For `handle_get_session` (`serve/mod.rs`), the
    /// client-facing "show me this session" RPC, which hands the record
    /// back to the party that wrote it and surfaces `ToolResultRef`'s cache
    /// key rather than pulling potentially-large content out of the cache
    /// into a response body.
    ///
    /// Unlike `load_session`, this does not hydrate `ToolResultRef` back
    /// into `ToolResult`, does not trim to a compaction checkpoint, and
    /// does not run `repair_tool_pairing` — it isn't assembling a history
    /// for a provider, so there's nothing to hydrate or repair for.
    pub fn load_session_full(
        &self,
        session_id: &str,
    ) -> Option<(Vec<StoredMessage>, Option<SummaryLine>)> {
        let loaded = load(&self.resolve_path(session_id)?)?;
        Some((loaded.messages, loaded.latest_summary))
    }

    /// Ensure a session exists for the given caller-supplied ID.
    /// Unlike `create_session`, this uses the provided ID rather than generating a new UUID.
    ///
    /// For RPC sessions (`channel == "rpc"`), a grain-id `public_id` is
    /// generated on creation unless `public_id_override` is supplied
    /// (used to commit a deferred public_id).
    /// Returns the `public_id` if present (new or existing).
    pub fn ensure_session(
        &self,
        session_id: &str,
        key: &ConversationKey,
        channel: &str,
        public_id_override: Option<String>,
        namespace: &str,
    ) -> anyhow::Result<Option<String>> {
        if let Some(existing) = self.resolve_path(session_id) {
            // Return existing public_id if the session already existed
            return Ok(meta_at(&existing).and_then(|meta| meta.public_id));
        }
        let public_id = if channel == "rpc" {
            Some(public_id_override.unwrap_or_else(|| grain_id::GrainId::random().to_string()))
        } else {
            None
        };
        let meta = SessionMeta {
            session_id: session_id.to_string(),
            room_id: key.0.clone(),
            thread_id: key.1.clone(),
            channel: channel.to_string(),
            created_at: Utc::now(),
            public_id: public_id.clone(),
            namespace: Some(namespace.to_string()),
            device_id: None,
            room_profile: None,
            title: None,
        };
        self.create_dir(&meta, namespace)?;
        Ok(public_id)
    }

    /// Find the most-recent device-default session for `(device_id, room_profile)`
    /// that falls within today's local-day window (per `boundary_hour`), or
    /// create a new one when none qualifies.
    ///
    /// Used by the voice `/rpc` methods and heartbeat fires to route into a
    /// device's always-on session (#122). Lazy: nothing lands on disk until a
    /// satellite that's actually used calls this. Closed sessions are skipped
    /// so an explicit boundary close still rotates the session even before
    /// the next day window kicks in.
    ///
    /// Returns the resolved session_id. The caller is expected to follow up
    /// with `append` for the message that triggered the lookup.
    pub fn find_or_create_for_device(
        &self,
        device_id: &str,
        room_profile: &str,
        namespace: &str,
        boundary_hour: u8,
    ) -> anyhow::Result<String> {
        let today = local_date_for_timestamp(Local::now(), boundary_hour);
        let (today_start, today_end) = day_window(today, boundary_hour);

        let mut best: Option<(DateTime<Utc>, String)> = None;
        for loc in collect_sessions(&self.base_dir, self.kind) {
            let Some(loaded) = load(&loc) else {
                continue;
            };
            let meta = loaded.meta;
            if loaded.is_closed {
                continue;
            }
            if meta.namespace.as_deref() != Some(namespace) {
                continue;
            }
            if meta.device_id.as_deref() != Some(device_id) {
                continue;
            }
            if meta.room_profile.as_deref() != Some(room_profile) {
                continue;
            }
            if meta.created_at < today_start || meta.created_at >= today_end {
                continue;
            }
            match &best {
                Some((ts, _)) if *ts >= meta.created_at => {}
                _ => best = Some((meta.created_at, meta.session_id.clone())),
            }
        }
        if let Some((_, id)) = best {
            // Seed the path cache so the upcoming `append` skips a rescan.
            let _ = self.resolve_path(&id);
            return Ok(id);
        }

        let session_id = Uuid::now_v7().to_string();
        let meta = SessionMeta {
            session_id: session_id.clone(),
            room_id: String::new(),
            thread_id: None,
            channel: "device-default".to_string(),
            created_at: Utc::now(),
            public_id: None,
            namespace: Some(namespace.to_string()),
            device_id: Some(device_id.to_string()),
            room_profile: Some(room_profile.to_string()),
            title: None,
        };
        self.create_dir(&meta, namespace)?;
        Ok(session_id)
    }

    /// Set a session's title (a new event; the latest one wins on read).
    pub fn set_title(&self, session_id: &str, title: &str) -> anyhow::Result<()> {
        self.append_event(session_id, |parent| {
            (
                Front::Title {
                    parent,
                    at: Utc::now(),
                    title: title.to_string(),
                },
                String::new(),
            )
        })?;
        Ok(())
    }

    /// Find a session by its human-readable `public_id` (grain-id).
    /// Returns the internal UUID `session_id` if found.
    pub fn find_by_public_id(&self, public_id: &str) -> Option<String> {
        collect_sessions(&self.base_dir, self.kind)
            .iter()
            .filter_map(meta_at)
            .find(|meta| meta.public_id.as_deref() == Some(public_id))
            .map(|meta| meta.session_id)
    }

    /// Return all sessions that contain at least one message falling within
    /// the given local-time day window.
    ///
    /// The "day" is `[date @ boundary_hour:00:00 local, (date+1) @ boundary_hour:00:00 local)`.
    ///
    /// Every message, forked branches included: this feeds the daily log,
    /// a permanent record that must not lose what a second writer said.
    pub fn sessions_for_day(
        &self,
        date: NaiveDate,
        boundary_hour: u8,
    ) -> Vec<(SessionMeta, Vec<StoredMessage>)> {
        let (day_start, day_end) = day_window(date, boundary_hour);
        let mut results = Vec::new();

        for loc in collect_sessions(&self.base_dir, self.kind) {
            // mtime pre-filter: skip sessions last modified before
            // day_start - 1 day. A directory's mtime moves whenever an
            // event file is added to it.
            if let Ok(meta_fs) = loc.path().metadata()
                && let Ok(mtime) = meta_fs.modified()
            {
                let mtime_utc: DateTime<Utc> = mtime.into();
                if mtime_utc < day_start - Duration::days(1) {
                    continue;
                }
            }

            if let Some(loaded) = load(&loc) {
                let meta = loaded.meta.clone();
                let day_messages: Vec<StoredMessage> = loaded
                    .all_messages()
                    .into_iter()
                    .filter(|m| m.timestamp >= day_start && m.timestamp < day_end)
                    .collect();

                if !day_messages.is_empty() {
                    results.push((meta, day_messages));
                }
            }
        }

        results.sort_by_key(|(meta, _)| meta.created_at);
        results
    }

    /// Like `sessions_for_day`, but only returns sessions for which
    /// `predicate(&meta)` is true. Used by daily-log generation when it
    /// runs per memory namespace: the caller supplies a predicate that
    /// keeps only rooms mapped to the namespace being generated.
    pub fn sessions_for_day_filtered<F>(
        &self,
        date: NaiveDate,
        boundary_hour: u8,
        predicate: F,
    ) -> Vec<(SessionMeta, Vec<StoredMessage>)>
    where
        F: Fn(&SessionMeta) -> bool,
    {
        self.sessions_for_day(date, boundary_hour)
            .into_iter()
            .filter(|(meta, _)| predicate(meta))
            .collect()
    }

    /// Like `all_session_dates`, but only counts sessions whose `meta`
    /// satisfies `predicate`. Used so per-namespace daily-log catch-up
    /// only enumerates dates that have at least one in-namespace session.
    pub fn all_session_dates_filtered<F>(&self, boundary_hour: u8, predicate: F) -> Vec<NaiveDate>
    where
        F: Fn(&SessionMeta) -> bool,
    {
        let mut dates = std::collections::HashSet::new();

        for loc in collect_sessions(&self.base_dir, self.kind) {
            if let Some(loaded) = load(&loc) {
                if !predicate(&loaded.meta) {
                    continue;
                }
                for msg in loaded.all_messages() {
                    let local_ts = msg.timestamp.with_timezone(&Local);
                    dates.insert(local_date_for_timestamp(local_ts, boundary_hour));
                }
            }
        }

        let mut sorted: Vec<NaiveDate> = dates.into_iter().collect();
        sorted.sort();
        sorted
    }

    /// Return all local dates for which at least one session message exists.
    /// Used by daily_log to find dates that need a log generated.
    #[allow(dead_code)]
    pub fn all_session_dates(&self, boundary_hour: u8) -> Vec<NaiveDate> {
        self.all_session_dates_filtered(boundary_hour, |_| true)
    }

    /// What a message looks like on disk.
    ///
    /// Three transformations, all storage-path-only — the in-memory
    /// history keeps the full values for the provider call:
    ///
    /// - a tool result's content goes to the cache and the line keeps a
    ///   hash, because `<workspace>/sessions` is inside the retrieve
    ///   index and a day of file reads would both bloat the workspace
    ///   and skew every search that touches it
    /// - an oversized tool *input* is elided, for the same reason and
    ///   with no cache to escape into
    /// - an image becomes a text marker carrying its hash
    ///
    /// Returns `None` when nothing needs rewriting, so the common
    /// text-only append skips the allocation.
    fn scrub_for_storage(&self, msg: &ChatMessage) -> Option<ChatMessage> {
        let needs_work = msg.parts.iter().any(|p| {
            matches!(
                p,
                ContentPart::Image { .. }
                    | ContentPart::ToolResult { .. }
                    | ContentPart::ToolUse { .. }
            )
        });
        if !needs_work {
            return None;
        }
        let parts = msg
            .parts
            .iter()
            .map(|p| match p {
                ContentPart::Image {
                    media_type,
                    data_base64,
                } => {
                    let hash = match BASE64_STANDARD.decode(data_base64) {
                        Ok(bytes) => sha256_hex(&bytes),
                        Err(_) => "invalid-base64".to_string(),
                    };
                    ContentPart::Text(format!("[image: {media_type} sha256={hash}]"))
                }
                ContentPart::ToolUse { id, name, input } => {
                    // Same treatment as a result, for the same reason: a
                    // `file_write` call carries the file's contents, and
                    // this line is about to land in the retrieve index.
                    // `name` stays inline — see `ContentPart::ToolUseRef`.
                    let sha256 = match &self.tool_payloads {
                        Some(cache) => match cache.put_input(input) {
                            Ok(sha) => Some(sha),
                            Err(e) => {
                                warn!("Failed to cache the input of tool call '{id}': {e}");
                                None
                            }
                        },
                        None => None,
                    };
                    ContentPart::ToolUseRef {
                        id: id.clone(),
                        name: name.clone(),
                        sha256,
                    }
                }
                ContentPart::ToolResult {
                    tool_use_id,
                    content,
                } => {
                    // A missing cache must not degrade any further than
                    // a lost hash does. Dropping the part would leave a
                    // `tool_use` with no matching `tool_result`, which
                    // the API rejects outright — the session would fail
                    // to load rather than load thinner.
                    let sha256 = match &self.tool_payloads {
                        Some(cache) => match cache.put(content) {
                            Ok(sha) => Some(sha),
                            Err(e) => {
                                warn!("Failed to cache tool result '{tool_use_id}': {e}");
                                None
                            }
                        },
                        None => None,
                    };
                    ContentPart::ToolResultRef {
                        tool_use_id: tool_use_id.clone(),
                        sha256,
                    }
                }
                other => other.clone(),
            })
            .collect();
        Some(ChatMessage {
            role: msg.role.clone(),
            parts,
            input_kind: msg.input_kind.clone(),
            user_id: msg.user_id.clone(),
        })
    }

    /// Turn every `ToolUseRef` and `ToolResultRef` back into the call
    /// the model needs to see. A miss is a stand-in, not an error.
    pub(crate) fn hydrate(&self, msgs: Vec<ChatMessage>) -> Vec<ChatMessage> {
        // Every pre-existing session file, and every one that has not
        // used a tool yet, holds nothing to hydrate — and this runs on
        // the startup path for each restored session. Ask once before
        // cloning the history.
        if !msgs.iter().any(|m| {
            m.parts.iter().any(|p| {
                matches!(
                    p,
                    ContentPart::ToolUseRef { .. } | ContentPart::ToolResultRef { .. }
                )
            })
        }) {
            return msgs;
        }
        msgs.into_iter()
            .map(|m| ChatMessage {
                parts: m
                    .parts
                    .iter()
                    .map(|p| match p {
                        ContentPart::ToolUseRef { id, name, sha256 } => ContentPart::ToolUse {
                            id: id.clone(),
                            name: name.clone(),
                            input: sha256
                                .as_ref()
                                .and_then(|s| self.tool_payloads.as_ref()?.get_input(s))
                                .unwrap_or_else(crate::session_storage::missing_input),
                        },
                        ContentPart::ToolResultRef {
                            tool_use_id,
                            sha256,
                        } => ContentPart::ToolResult {
                            tool_use_id: tool_use_id.clone(),
                            content: sha256
                                .as_ref()
                                .and_then(|s| self.tool_payloads.as_ref()?.get(s))
                                .unwrap_or_else(|| {
                                    crate::session_storage::MISSING_RESULT.to_string()
                                }),
                        },
                        other => other.clone(),
                    })
                    .collect(),
                ..m
            })
            .collect()
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut h = Sha256::new();
    h.update(bytes);
    let digest = h.finalize();
    let mut s = String::with_capacity(64);
    for b in digest.iter() {
        let _ = write!(&mut s, "{b:02x}");
    }
    s
}

// ---------------------------------------------------------------------------
// Namespace-scoped filesystem walking
// ---------------------------------------------------------------------------

/// Enumerate the sessions under `<base_dir>/<namespace>/<kind>/` across
/// every namespace directory: each session directory, and each legacy
/// `.jsonl` that has not been converted into one. Returns an empty Vec
/// when `base_dir` doesn't exist yet (fresh install) or has no namespace
/// subdirs. Each returned path is absolute.
pub(crate) fn collect_sessions(base_dir: &Path, kind: &str) -> Vec<Location> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(base_dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let ns_dir = entry.path();
        if !ns_dir.is_dir() {
            continue;
        }
        let kind_dir = ns_dir.join(kind);
        let Ok(kind_entries) = fs::read_dir(&kind_dir) else {
            continue;
        };
        for k_entry in kind_entries.flatten() {
            let path = k_entry.path();
            let hidden = path
                .file_name()
                .and_then(|n| n.to_str())
                .is_none_or(|n| n.starts_with('.'));
            if hidden {
                // Staging directories of an in-flight conversion.
                continue;
            }
            if path.is_dir() {
                if session_log::is_session_dir(&path) {
                    out.push(Location::Dir(path));
                }
            } else if path.extension().and_then(|e| e.to_str()) == Some("jsonl")
                && !session_log::is_session_dir(&path.with_extension(""))
            {
                // A converted `.jsonl` stays on disk, superseded by the
                // directory named after its stem. TODO(#284).
                out.push(Location::Jsonl(path));
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Day window helpers
// ---------------------------------------------------------------------------

/// Compute the UTC start and end of the local-time day window for `date`
/// with a given `boundary_hour`.
///
/// Window: `[date @ boundary_hour:00:00 local, (date+1day) @ boundary_hour:00:00 local)`
pub(crate) fn day_window(date: NaiveDate, boundary_hour: u8) -> (DateTime<Utc>, DateTime<Utc>) {
    let start_local = date
        .and_hms_opt(boundary_hour as u32, 0, 0)
        .expect("valid time");
    let end_local = (date + Duration::days(1))
        .and_hms_opt(boundary_hour as u32, 0, 0)
        .expect("valid time");

    let start_utc = Local
        .from_local_datetime(&start_local)
        .single()
        .unwrap_or_else(|| Local.from_local_datetime(&start_local).earliest().unwrap())
        .with_timezone(&Utc);

    let end_utc = Local
        .from_local_datetime(&end_local)
        .single()
        .unwrap_or_else(|| Local.from_local_datetime(&end_local).earliest().unwrap())
        .with_timezone(&Utc);

    (start_utc, end_utc)
}

/// Given a local timestamp, return the local date it belongs to for a given
/// `boundary_hour`. Timestamps before `boundary_hour` belong to the previous day.
pub fn local_date_for_timestamp(local_ts: DateTime<Local>, boundary_hour: u8) -> NaiveDate {
    let date = local_ts.date_naive();
    if local_ts.hour() < boundary_hour as u32 {
        date - Duration::days(1)
    } else {
        date
    }
}

// ---------------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------------

/// One session, parsed.
struct LoadedSession {
    meta: SessionMeta,
    /// The messages on the session's chain, in order.
    messages: Vec<StoredMessage>,
    /// Messages on a forked branch the chain does not follow. Empty unless
    /// two writers appended without seeing each other. See
    /// [`LoadedSession::all_messages`].
    off_chain: Vec<StoredMessage>,
    is_closed: bool,
    latest_summary: Option<SummaryLine>,
    /// `messages.len()` at the moment the latest summary was read.
    /// The order fallback for a summary with no `covers_through`.
    messages_before_summary: usize,
}

impl LoadedSession {
    fn from_events(meta: SessionMeta, events: impl IntoIterator<Item = Event>) -> LoadedSession {
        let mut loaded = LoadedSession {
            meta,
            messages: Vec::new(),
            off_chain: Vec::new(),
            is_closed: false,
            latest_summary: None,
            messages_before_summary: 0,
        };
        for event in events {
            match event {
                Event::Message(m) => loaded.messages.push(m),
                Event::Summary(s) => {
                    loaded.latest_summary = Some(s);
                    loaded.messages_before_summary = loaded.messages.len();
                }
                // Last title wins: a session can be retitled.
                Event::Title(t) => loaded.meta.title = Some(t),
                Event::Closed => loaded.is_closed = true,
            }
        }
        loaded
    }

    /// Every message, forked branches included, by time.
    fn all_messages(self) -> Vec<StoredMessage> {
        if self.off_chain.is_empty() {
            return self.messages;
        }
        let mut all = self.messages;
        all.extend(self.off_chain);
        all.sort_by_key(|m| (m.timestamp, m.id));
        all
    }
}

/// Parse a session. `None` when its metadata is unreadable.
fn load(loc: &Location) -> Option<LoadedSession> {
    match loc {
        Location::Dir(dir) => load_dir(dir),
        Location::Jsonl(path) => {
            let (meta, events) = read_jsonl(path)?;
            Some(LoadedSession::from_events(
                meta,
                events.into_iter().map(|(e, _)| e),
            ))
        }
    }
}

fn load_dir(dir: &Path) -> Option<LoadedSession> {
    let meta: SessionMeta = session_log::read_meta(dir)?;
    let entries: Vec<EntryFile<Front>> = session_log::read_entries(dir);
    let chain = session_log::chain(&meta.session_id, &entries);
    let on_chain: std::collections::HashSet<Uuid> = chain.iter().map(|e| e.id).collect();
    let off_chain = entries
        .iter()
        .filter(|e| !on_chain.contains(&e.id))
        .filter_map(|e| match Event::from_entry(e) {
            Event::Message(m) => Some(m),
            _ => None,
        })
        .collect();
    let mut loaded = LoadedSession::from_events(meta, chain.into_iter().map(Event::from_entry));
    loaded.off_chain = off_chain;
    Some(loaded)
}

/// A session's metadata alone.
fn meta_at(loc: &Location) -> Option<SessionMeta> {
    match loc {
        Location::Dir(dir) => session_log::read_meta(dir),
        Location::Jsonl(path) => {
            let file = fs::File::open(path).ok()?;
            let first = BufReader::new(file).lines().next()?.ok()?;
            serde_json::from_str::<MetaLine>(first.trim())
                .ok()
                .map(|l| l.meta)
        }
    }
}

/// Parse a legacy `.jsonl` session into its events, in file order.
///
/// Returns `None` if the file is unreadable or has a malformed first line.
/// TODO(#284).
#[allow(clippy::type_complexity)]
fn read_jsonl(path: &Path) -> Option<(SessionMeta, Vec<(Event, Option<DateTime<Utc>>)>)> {
    let file = fs::File::open(path).ok()?;
    let mut lines = BufReader::new(file).lines();

    let first = lines.next()?.ok()?;
    let meta_line: MetaLine = serde_json::from_str(first.trim()).ok()?;
    let meta = meta_line.meta;

    let mut events = Vec::new();
    for raw in lines.map_while(Result::ok) {
        let raw = raw.trim().to_string();
        if raw.is_empty() {
            continue;
        }

        let value: serde_json::Value = match serde_json::from_str(&raw) {
            Ok(v) => v,
            Err(e) => {
                warn!("Skipping unparseable line in {}: {e}", path.display());
                continue;
            }
        };

        if let Some(at) = value.get("closed_at") {
            let at = serde_json::from_value::<DateTime<Utc>>(at.clone()).ok();
            events.push((Event::Closed, at));
        } else if let Some(title) = value.get("session_title").and_then(|v| v.as_str()) {
            events.push((Event::Title(title.to_string()), None));
        } else if value.get("summary_at").is_some() {
            match serde_json::from_value::<SummaryLine>(value) {
                Ok(s) => {
                    let at = Some(s.summary_at);
                    events.push((Event::Summary(s), at));
                }
                Err(e) => {
                    warn!("Skipping malformed summary in {}: {e}", path.display());
                }
            }
        } else if value.get("digest_at").is_some() {
            // Intra-day digest lines, written by the builds that had a
            // periodic summariser. Nothing reads them any more; they are
            // skipped here so an old file's digest is never mistaken for
            // a message.
            continue;
        } else if value.get("timestamp").is_some() {
            match serde_json::from_value::<StoredMessage>(value) {
                Ok(stored) => {
                    let at = Some(stored.timestamp);
                    events.push((Event::Message(stored), at));
                }
                Err(e) => {
                    warn!("Skipping malformed message in {}: {e}", path.display());
                }
            }
        }
    }

    Some((meta, events))
}

/// Convert a legacy `.jsonl` session into a session directory beside it,
/// named after its stem, and return that directory. The `.jsonl` is left
/// in place. TODO(#284).
///
/// Each line becomes an event whose parent is the line before it — the
/// migration rule of decision 3.1: a file written before `parent` existed
/// cannot hold a fork, so file order is the chain. A message with no id
/// gets one, and a summary with no `covers_through` gets the last message
/// before it, which is exactly what such a line meant. Lines without a
/// time of their own (titles) take the time of the line before them.
fn convert_jsonl(path: &Path) -> anyhow::Result<PathBuf> {
    let dest = path.with_extension("");
    if session_log::is_session_dir(&dest) {
        return Ok(dest);
    }
    let (meta, events) = read_jsonl(path)
        .ok_or_else(|| anyhow::anyhow!("{} is not a readable session", path.display()))?;

    let staging = session_log::staging_dir_for(&dest)?;
    session_log::write_meta(&staging, &meta)?;
    let mut parent: Option<Uuid> = None;
    let mut last_at = meta.created_at;
    let mut last_message: Option<Uuid> = None;
    for (event, own_at) in events {
        let id = match &event {
            Event::Message(m) => m.id.unwrap_or_else(Uuid::now_v7),
            _ => Uuid::now_v7(),
        };
        let at = own_at.unwrap_or(last_at);
        let (front, body) = match event {
            Event::Message(m) => {
                last_message = Some(id);
                let (parts, body) = session_log::split_body(m.parts, text_of);
                (
                    Front::Message {
                        parent,
                        at,
                        role: m.role,
                        input_kind: m.input_kind,
                        user_id: m.user_id,
                        parts,
                    },
                    body,
                )
            }
            Event::Summary(s) => (
                Front::Summary {
                    parent,
                    at,
                    covers_through: s.covers_through.or(last_message),
                },
                s.summary,
            ),
            Event::Title(title) => (Front::Title { parent, at, title }, String::new()),
            Event::Closed => (Front::Closed { parent, at }, String::new()),
        };
        session_log::write_entry(&staging, id, &front, &body)?;
        parent = Some(id);
        last_at = last_at.max(at);
    }
    session_log::publish_dir(&staging, &dest)?;
    Ok(dest)
}

/// The messages a restore should replay, and the summary to prefix them
/// with.
///
/// Everything before the checkpoint is what the summary already says;
/// replaying it too would re-send a conversation the running process had
/// already compacted away, and pay for compacting it again on the first
/// turn back.
fn model_history(loaded: &LoadedSession) -> (Option<&str>, &[StoredMessage]) {
    let Some(summary) = &loaded.latest_summary else {
        return (None, &loaded.messages);
    };
    let start = match summary.covers_through {
        Some(id) => match loaded.messages.iter().position(|m| m.id == Some(id)) {
            Some(pos) => pos + 1,
            None => {
                warn!("session checkpoint {id} is not in the session; replaying the whole session");
                0
            }
        },
        None => loaded.messages_before_summary,
    };
    (
        Some(summary.summary.as_str()),
        loaded.messages.get(start..).unwrap_or(&[]),
    )
}

/// One session as the `session_list` tool renders it.
#[derive(Debug, Clone)]
pub struct SessionRow {
    pub meta: SessionMeta,
    /// Number of stored messages (both roles, tool traffic included).
    pub message_count: usize,
    /// Timestamp of the last stored message, `None` for an empty session.
    pub last_at: Option<DateTime<Utc>>,
    pub is_closed: bool,
}

/// One session's listing row. `None` when its metadata is unreadable.
fn load_session_row(loc: &Location) -> Option<SessionRow> {
    let loaded = load(loc)?;
    Some(SessionRow {
        message_count: loaded.messages.len() + loaded.off_chain.len(),
        last_at: loaded
            .messages
            .iter()
            .chain(&loaded.off_chain)
            .map(|m| m.timestamp)
            .max(),
        is_closed: loaded.is_closed,
        meta: loaded.meta,
    })
}

/// The id of the last message a summary keeping `keep_recent` trailing
/// messages absorbs.
///
/// `None` when the session has no messages.
fn checkpoint_id(loc: &Location, keep_recent: usize) -> Option<Uuid> {
    let ids: Vec<Option<Uuid>> = load(loc)?.messages.iter().map(|m| m.id).collect();
    if ids.is_empty() {
        return None;
    }
    // A summary that covers nothing is not reachable from
    // `maybe_compress` (it fires only with at least one message to
    // summarise, and the log holds at least as many as memory did), but
    // clamping costs nothing and keeps the index in range.
    let covered = ids.len().saturating_sub(keep_recent).max(1);
    ids[covered - 1]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every newly written message carries an id. The compaction
    /// checkpoint points at one, so a message without one cannot be a
    /// cursor.
    #[test]
    fn a_new_stored_message_gets_an_id() {
        let stored = StoredMessage::from_chat(&ChatMessage::user("hi"));
        assert!(stored.id.is_some(), "from_chat must stamp an id");
    }

    /// Distinct messages get distinct ids. The checkpoint looks its
    /// cursor up by position, so what it needs is uniqueness, not order —
    /// file order is what orders a session, here and in the future
    /// `parent` migration (decision 3.1).
    #[test]
    fn each_message_gets_its_own_id() {
        let a = StoredMessage::from_chat(&ChatMessage::user("first"));
        let b = StoredMessage::from_chat(&ChatMessage::user("second"));
        assert_ne!(a.id.unwrap(), b.id.unwrap());
    }

    /// Legacy JSONL predates the field and must still load.
    #[test]
    fn a_stored_message_without_an_id_deserializes_as_none() {
        let legacy = r#"{"timestamp":"2026-04-08T11:30:22.372570890Z","role":"user","parts":[{"Text":"hello"}]}"#;
        let msg: StoredMessage = serde_json::from_str(legacy).expect("legacy JSONL parses");
        assert!(msg.id.is_none());
    }

    #[test]
    fn scrub_returns_none_when_no_images() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = SessionStore::new(tmp.path().to_path_buf(), "channel", None);
        let msg = ChatMessage::user("plain text");
        assert!(store.scrub_for_storage(&msg).is_none());
    }

    #[test]
    fn scrub_replaces_image_with_hash_marker() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = SessionStore::new(tmp.path().to_path_buf(), "channel", None);
        let bytes = b"\xff\xd8\xff\xe0fake-jpeg".to_vec();
        let b64 = BASE64_STANDARD.encode(&bytes);
        let msg =
            ChatMessage::user_with_images("look", std::iter::once(("image/jpeg".to_string(), b64)));
        let scrubbed = store.scrub_for_storage(&msg).expect("scrub should rewrite");

        // No Image parts remain on the persisted shape.
        assert!(
            !scrubbed
                .parts
                .iter()
                .any(|p| matches!(p, ContentPart::Image { .. })),
            "scrubbed message still contains Image part"
        );

        // The marker is text and carries the expected hash.
        let expected = sha256_hex(&bytes);
        let has_marker = scrubbed
            .parts
            .iter()
            .any(|p| matches!(p, ContentPart::Text(s) if s.contains(&expected) && s.contains("image/jpeg")));
        assert!(
            has_marker,
            "missing hash marker; parts={:?}",
            scrubbed.parts
        );
    }

    #[test]
    fn scrub_invalid_base64_records_marker_without_panic() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = SessionStore::new(tmp.path().to_path_buf(), "channel", None);
        let msg = ChatMessage {
            role: Role::User,
            parts: vec![ContentPart::Image {
                media_type: "image/png".to_string(),
                data_base64: "@@@not-base64@@@".to_string(),
            }],
            input_kind: Some(UserInputKind::Text),
            user_id: None,
        };
        let scrubbed = store.scrub_for_storage(&msg).expect("scrub should rewrite");
        let has_marker = scrubbed
            .parts
            .iter()
            .any(|p| matches!(p, ContentPart::Text(s) if s.contains("invalid-base64")));
        assert!(has_marker, "expected invalid-base64 marker");
    }

    #[test]
    fn scrub_passes_imageref_through_unchanged() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = SessionStore::new(tmp.path().to_path_buf(), "channel", None);
        let msg = ChatMessage {
            role: Role::User,
            parts: vec![ContentPart::ImageRef {
                media_type: "image/jpeg".to_string(),
                sha256: "abc123".to_string(),
            }],
            input_kind: Some(UserInputKind::Text),
            user_id: None,
        };
        // ImageRef carries no raw bytes — nothing to scrub, so the
        // helper returns None and append serializes the variant as-is.
        assert!(
            store.scrub_for_storage(&msg).is_none(),
            "scrub should leave ImageRef-only messages untouched"
        );
    }

    // ── input_kind / user_id round-trip and backward compat ─────────────

    #[test]
    fn stored_message_without_input_kind_or_user_id_deserializes_as_none() {
        // Legacy JSONL written before these fields existed must still
        // load: no `input_kind`, no `user_id`.
        let legacy = r#"{"timestamp":"2026-04-08T11:30:22.372570890Z","role":"user","parts":[{"Text":"hello"}]}"#;
        let msg: StoredMessage = serde_json::from_str(legacy).expect("legacy JSONL parses");
        assert!(msg.input_kind.is_none());
        assert!(msg.user_id.is_none());
    }

    #[test]
    fn stored_message_omits_none_fields_on_serialize() {
        let msg = StoredMessage {
            id: None,
            timestamp: Utc::now(),
            role: Role::Assistant,
            parts: vec![ContentPart::Text("hi".to_string())],
            input_kind: None,
            user_id: None,
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(
            !json.contains("input_kind"),
            "None input_kind must be omitted, got: {json}"
        );
        assert!(
            !json.contains("user_id"),
            "None user_id must be omitted, got: {json}"
        );
    }

    #[test]
    fn stored_message_text_input_kind_round_trip() {
        let original = StoredMessage {
            id: None,
            timestamp: Utc::now(),
            role: Role::User,
            parts: vec![ContentPart::Text("hi".to_string())],
            input_kind: Some(UserInputKind::Text),
            user_id: Some("owner".to_string()),
        };
        let json = serde_json::to_string(&original).unwrap();
        assert!(json.contains(r#""input_kind":{"kind":"text"}"#));
        assert!(json.contains(r#""user_id":"owner""#));
        let parsed: StoredMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.input_kind, Some(UserInputKind::Text));
        assert_eq!(parsed.user_id.as_deref(), Some("owner"));
    }

    #[test]
    fn stored_message_voice_input_kind_round_trip() {
        let original = StoredMessage {
            id: None,
            timestamp: Utc::now(),
            role: Role::User,
            parts: vec![ContentPart::Text("hello there".to_string())],
            input_kind: Some(UserInputKind::Voice),
            user_id: None,
        };
        let json = serde_json::to_string(&original).unwrap();
        assert!(
            json.contains(r#""input_kind":{"kind":"voice"}"#),
            "missing voice tag: {json}"
        );
        let parsed: StoredMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.input_kind, Some(UserInputKind::Voice));
    }

    #[test]
    fn from_chat_and_into_chat_preserve_input_kind_and_user_id() {
        let chat = ChatMessage {
            role: Role::User,
            parts: vec![ContentPart::Text("hi".to_string())],
            input_kind: Some(UserInputKind::Voice),
            user_id: Some("alice".to_string()),
        };
        let stored = StoredMessage::from_chat(&chat);
        assert_eq!(stored.input_kind, Some(UserInputKind::Voice));
        assert_eq!(stored.user_id.as_deref(), Some("alice"));
        let round = stored.into_chat_message();
        assert_eq!(round.input_kind, Some(UserInputKind::Voice));
        assert_eq!(round.user_id.as_deref(), Some("alice"));
    }

    // ── device-default session routing (#122) ────────────────────────────

    fn new_device_default_store() -> (tempfile::TempDir, SessionStore) {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = SessionStore::new(tmp.path().to_path_buf(), "device-default", None);
        (tmp, store)
    }

    /// Fresh dir: returns a brand-new UUID, persists meta with the right
    /// `(device_id, room_profile, channel, namespace)` fields.
    #[test]
    fn find_or_create_for_device_creates_new_session() {
        let (_tmp, store) = new_device_default_store();
        let sid = store
            .find_or_create_for_device("device-a", "default", "default", 4)
            .expect("find_or_create");
        // Resolve from disk and verify the meta the store wrote.
        let loc = store.resolve_path(&sid).expect("path cached");
        assert!(
            matches!(loc, Location::Dir(_)),
            "new sessions are directories"
        );
        let loaded = load(&loc).expect("metadata present");
        let meta = loaded.meta;
        assert_eq!(meta.session_id, sid);
        assert_eq!(meta.channel, "device-default");
        assert_eq!(meta.device_id.as_deref(), Some("device-a"));
        assert_eq!(meta.room_profile.as_deref(), Some("default"));
        assert_eq!(meta.namespace.as_deref(), Some("default"));
        assert!(meta.public_id.is_none(), "device-default has no grain-id");
        assert!(!loaded.is_closed);
    }

    /// Second call within the same local day returns the same session_id.
    #[test]
    fn find_or_create_for_device_is_idempotent_within_day() {
        let (_tmp, store) = new_device_default_store();
        let first = store
            .find_or_create_for_device("device-a", "default", "default", 4)
            .unwrap();
        let second = store
            .find_or_create_for_device("device-a", "default", "default", 4)
            .unwrap();
        assert_eq!(first, second);
    }

    /// Different device_id ⇒ separate file.
    #[test]
    fn find_or_create_for_device_distinguishes_devices() {
        let (_tmp, store) = new_device_default_store();
        let a = store
            .find_or_create_for_device("device-a", "default", "default", 4)
            .unwrap();
        let b = store
            .find_or_create_for_device("device-b", "default", "default", 4)
            .unwrap();
        assert_ne!(a, b);
    }

    /// Different room_profile ⇒ separate file (NSFW isolation).
    #[test]
    fn find_or_create_for_device_distinguishes_room_profiles() {
        let (_tmp, store) = new_device_default_store();
        let sfw = store
            .find_or_create_for_device("device-a", "default", "default", 4)
            .unwrap();
        let nsfw = store
            .find_or_create_for_device("device-a", "private_nsfw", "user_nsfw", 4)
            .unwrap();
        assert_ne!(sfw, nsfw);
    }

    /// Closing the active session ⇒ next call rotates to a fresh UUID.
    #[test]
    fn find_or_create_for_device_skips_closed_sessions() {
        let (_tmp, store) = new_device_default_store();
        let first = store
            .find_or_create_for_device("device-a", "default", "default", 4)
            .unwrap();
        store.close_session(&first).expect("close");
        let second = store
            .find_or_create_for_device("device-a", "default", "default", 4)
            .unwrap();
        assert_ne!(
            first, second,
            "closed session must not be reused — boundary rotation depends on this"
        );
    }

    /// A meta file dated outside today's local window must not be reused —
    /// next call must create a fresh session.
    #[test]
    fn find_or_create_for_device_skips_yesterday_session() {
        let (tmp, store) = new_device_default_store();
        let boundary = 4u8;

        // Hand-craft a meta file whose `created_at` falls in yesterday's
        // local-day window so the most-recent scan rejects it.
        let yesterday_date = local_date_for_timestamp(Local::now(), boundary) - Duration::days(1);
        let (yesterday_start, _) = day_window(yesterday_date, boundary);
        let stale_id = Uuid::now_v7().to_string();
        let stale_dir = tmp.path().join("default").join("device-default");
        fs::create_dir_all(&stale_dir).unwrap();
        let stale_path = stale_dir.join(format!("{stale_id}.jsonl"));
        let stale_meta = SessionMeta {
            session_id: stale_id.clone(),
            room_id: String::new(),
            thread_id: None,
            channel: "device-default".to_string(),
            created_at: yesterday_start + Duration::hours(2),
            public_id: None,
            namespace: Some("default".to_string()),
            device_id: Some("device-a".to_string()),
            room_profile: Some("default".to_string()),
            title: None,
        };
        let line = serde_json::to_string(&MetaLine { meta: stale_meta }).unwrap();
        std::fs::write(&stale_path, format!("{line}\n")).unwrap();

        let fresh = store
            .find_or_create_for_device("device-a", "default", "default", boundary)
            .unwrap();
        assert_ne!(
            stale_id, fresh,
            "yesterday's session must not be picked up; daily rotation depends on it"
        );
    }

    // ── ツール結果の永続化 (#194) ────────────────────────────────────

    fn cached_store() -> (tempfile::TempDir, tempfile::TempDir, SessionStore, String) {
        let sessions = tempfile::TempDir::new().unwrap();
        let cache_dir = tempfile::TempDir::new().unwrap();
        let cache =
            crate::tool_payload_cache::ToolPayloadCache::open(cache_dir.path().to_path_buf())
                .unwrap();
        let store = SessionStore::new(sessions.path().to_path_buf(), "channel", Some(cache));
        let key = ("!room:example.org".to_string(), None);
        let sid = store.create_session(&key, "matrix", "default").unwrap();
        (sessions, cache_dir, store, sid)
    }

    fn tool_use_msg(id: &str) -> ChatMessage {
        ChatMessage {
            role: Role::Assistant,
            parts: vec![ContentPart::ToolUse {
                id: id.to_string(),
                name: "file_read".to_string(),
                input: serde_json::json!({ "path": "a.rs" }),
            }],
            input_kind: None,
            user_id: None,
        }
    }

    fn tool_result_msg(id: &str, content: &str) -> ChatMessage {
        ChatMessage {
            role: Role::User,
            parts: vec![ContentPart::ToolResult {
                tool_use_id: id.to_string(),
                content: content.to_string(),
            }],
            input_kind: None,
            user_id: None,
        }
    }

    /// The whole point: what the agent did survives a reload.
    #[test]
    fn a_tool_result_round_trips_through_the_cache() {
        let (_s, _c, store, sid) = cached_store();
        store.append(&sid, &tool_use_msg("c1")).unwrap();
        store
            .append(&sid, &tool_result_msg("c1", "fn main() {}"))
            .unwrap();

        let loaded = store.load_session(&sid).expect("the session loads");
        let content = loaded.iter().flat_map(|m| &m.parts).find_map(|p| match p {
            ContentPart::ToolResult {
                tool_use_id,
                content,
            } if tool_use_id == "c1" => Some(content.clone()),
            _ => None,
        });
        assert_eq!(content.as_deref(), Some("fn main() {}"));
    }

    /// The content must not be in the session files — they are inside the
    /// retrieve index, which is the whole reason for the cache.
    #[test]
    fn the_result_content_never_reaches_the_session_file() {
        let (_s, _c, store, sid) = cached_store();
        store.append(&sid, &tool_use_msg("c1")).unwrap();
        store
            .append(&sid, &tool_result_msg("c1", "SECRET-CANARY-VALUE"))
            .unwrap();

        let raw = store.raw_for_test(&sid);
        assert!(
            !raw.contains("SECRET-CANARY-VALUE"),
            "tool result content leaked into the indexed file:\n{raw}"
        );
        assert!(
            raw.contains("ToolResultRef"),
            "expected a reference:\n{raw}"
        );
    }

    /// An evicted result degrades to a placeholder. The pairing is what
    /// the API validates, so this must load rather than fail.
    #[test]
    fn an_evicted_result_becomes_a_placeholder() {
        let (_s, cache_dir, store, sid) = cached_store();
        store.append(&sid, &tool_use_msg("c1")).unwrap();
        store
            .append(&sid, &tool_result_msg("c1", "gone soon"))
            .unwrap();

        for entry in fs::read_dir(cache_dir.path()).unwrap().flatten() {
            fs::remove_file(entry.path()).unwrap();
        }

        let loaded = store.load_session(&sid).expect("the session still loads");
        let has_placeholder = loaded.iter().flat_map(|m| &m.parts).any(
            |p| matches!(p, ContentPart::ToolResult { tool_use_id, content } if tool_use_id == "c1" && content == crate::session_storage::MISSING_RESULT),
        );
        assert!(has_placeholder, "expected a placeholder: {loaded:?}");
    }

    /// No cache at write time records the pairing with no hash. Writing
    /// nothing would leave a tool_use the API rejects.
    #[test]
    fn no_cache_at_write_time_still_keeps_the_pairing() {
        let sessions = tempfile::TempDir::new().unwrap();
        let store = SessionStore::new(sessions.path().to_path_buf(), "channel", None);
        let key = ("!room:example.org".to_string(), None);
        let sid = store.create_session(&key, "matrix", "default").unwrap();

        store.append(&sid, &tool_use_msg("c1")).unwrap();
        store.append(&sid, &tool_result_msg("c1", "lost")).unwrap();

        let loaded = store.load_session(&sid).expect("the session loads");
        let answered = loaded.iter().flat_map(|m| &m.parts).any(
            |p| matches!(p, ContentPart::ToolResult { tool_use_id, .. } if tool_use_id == "c1"),
        );
        assert!(answered, "the tool_use must still be answered: {loaded:?}");
    }

    /// A tool input goes to the cache whatever its size, for the same
    /// reason a result does: a `file_write` call carries the file's
    /// contents, and this line lands in the retrieve index.
    #[test]
    fn a_tool_input_never_reaches_the_session_file() {
        let (_s, _c, store, sid) = cached_store();
        let huge = "SECRET-INPUT-CANARY".repeat(64);
        store
            .append(
                &sid,
                &ChatMessage {
                    role: Role::Assistant,
                    parts: vec![ContentPart::ToolUse {
                        id: "c1".to_string(),
                        name: "file_write".to_string(),
                        input: serde_json::json!({ "content": huge }),
                    }],
                    input_kind: None,
                    user_id: None,
                },
            )
            .unwrap();

        let raw = store.raw_for_test(&sid);
        assert!(
            !raw.contains("SECRET-INPUT-CANARY"),
            "the input leaked into the indexed file:\n{}",
            &raw[..raw.len().min(400)]
        );
        assert!(raw.contains("ToolUseRef"), "expected a reference:\n{raw}");
        assert!(
            raw.contains("file_write"),
            "the tool name must stay inline:\n{raw}"
        );

        // ...and comes back whole, byte-for-byte, which elision never did.
        let loaded = store.load_session(&sid).expect("the session loads");
        let input = loaded.iter().flat_map(|m| &m.parts).find_map(|p| match p {
            ContentPart::ToolUse { id, input, .. } if id == "c1" => Some(input.clone()),
            _ => None,
        });
        assert_eq!(
            input,
            Some(serde_json::json!({ "content": huge })),
            "the input must round-trip exactly: {loaded:?}"
        );
    }

    /// No cache at write time keeps the call and loses only its
    /// arguments. Dropping the part would orphan the result answering it.
    #[test]
    fn no_cache_at_write_time_still_keeps_the_call() {
        let sessions = tempfile::TempDir::new().unwrap();
        let store = SessionStore::new(sessions.path().to_path_buf(), "channel", None);
        let key = ("!room:example.org".to_string(), None);
        let sid = store.create_session(&key, "matrix", "default").unwrap();
        store.append(&sid, &tool_use_msg("c1")).unwrap();

        let loaded = store.load_session(&sid).expect("the session loads");
        let call = loaded.iter().flat_map(|m| &m.parts).find_map(|p| match p {
            ContentPart::ToolUse { id, name, input } if id == "c1" => {
                Some((name.clone(), input.clone()))
            }
            _ => None,
        });
        let (name, input) = call.expect("the call survives: {loaded:?}");
        assert_eq!(name, "file_read", "the tool name must survive");
        assert!(
            input.get("_unavailable").is_some(),
            "expected the stand-in input, got {input}"
        );
    }

    /// An evicted input degrades to the stand-in, keeping the call. The
    /// model then knows it asked `file_read` something without knowing
    /// what — thinner than a lost result, which it could simply re-run,
    /// which is why `name` is kept out of the cache.
    #[test]
    fn an_evicted_input_keeps_the_call_and_loses_its_arguments() {
        let (_s, cache_dir, store, sid) = cached_store();
        store.append(&sid, &tool_use_msg("c1")).unwrap();
        for entry in fs::read_dir(cache_dir.path()).unwrap().flatten() {
            fs::remove_file(entry.path()).unwrap();
        }

        let loaded = store.load_session(&sid).expect("the session still loads");
        let call = loaded.iter().flat_map(|m| &m.parts).find_map(|p| match p {
            ContentPart::ToolUse { id, name, input } if id == "c1" => {
                Some((name.clone(), input.clone()))
            }
            _ => None,
        });
        let (name, input) = call.expect("the call survives an evicted input");
        assert_eq!(name, "file_read");
        assert!(
            input.get("_unavailable").is_some(),
            "expected the stand-in input, got {input}"
        );
    }

    /// The daily log is a permanent, searchable record. A placeholder
    /// sentence from an evicted result has no business in it.
    #[test]
    fn the_daily_log_projection_carries_no_tool_traffic() {
        let (_s, _c, store, sid) = cached_store();
        store
            .append(&sid, &ChatMessage::user("what is in a.rs"))
            .unwrap();
        store.append(&sid, &tool_use_msg("c1")).unwrap();
        store
            .append(&sid, &tool_result_msg("c1", "fn main() {}"))
            .unwrap();

        let today = local_date_for_timestamp(Local::now(), 4);
        let days = store.sessions_for_day(today, 4);
        let parts: Vec<&ContentPart> = days
            .iter()
            .flat_map(|(_, ms)| ms)
            .flat_map(|m| &m.parts)
            .collect();
        assert!(
            parts.iter().all(|p| matches!(
                p,
                ContentPart::Text(_)
                    | ContentPart::ToolUseRef { .. }
                    | ContentPart::ToolResultRef { .. }
            )),
            "no hydrated tool payload may appear: {parts:?}"
        );
        assert!(
            !parts.iter().any(|p| matches!(
                p,
                ContentPart::ToolResult { .. } | ContentPart::ToolUse { .. }
            )),
            "sessions_for_day must not hydrate: {parts:?}"
        );
    }

    /// The text parts of a loaded history, in order.
    fn texts(history: &[ChatMessage]) -> Vec<String> {
        history
            .iter()
            .flat_map(|m| &m.parts)
            .filter_map(|p| match p {
                ContentPart::Text(t) => Some(t.clone()),
                _ => None,
            })
            .collect()
    }

    /// Hand-write a legacy `.jsonl` session — a meta line, then `lines` —
    /// and return its id.
    fn legacy_session(store: &SessionStore, lines: &[String]) -> String {
        let sid = Uuid::now_v7().to_string();
        let dir = store.base_dir.join("default").join(store.kind);
        fs::create_dir_all(&dir).unwrap();
        let meta = serde_json::json!({ "meta": {
            "session_id": sid,
            "room_id": "!room:example.org",
            "thread_id": null,
            "channel": "matrix",
            "created_at": "2026-04-06T09:00:00Z",
            "namespace": "default",
        }});
        let mut body = format!("{meta}\n");
        for line in lines {
            body.push_str(line);
            body.push('\n');
        }
        fs::write(dir.join(format!("{sid}.jsonl")), body).unwrap();
        sid
    }

    fn legacy_message(i: usize, text: &str, id: Option<Uuid>) -> String {
        let mut v = serde_json::json!({
            "timestamp": format!("2026-04-06T10:{i:02}:00Z"),
            "role": "user",
            "parts": [{"Text": text}],
        });
        if let Some(id) = id {
            v["id"] = serde_json::json!(id);
        }
        v.to_string()
    }

    /// Load `sid`, convert it with a write, and load it again: both reads
    /// must agree on everything before the write.
    fn texts_before_and_after_conversion(
        store: &SessionStore,
        sid: &str,
    ) -> (Vec<String>, Vec<String>) {
        let before = texts(&store.load_session(sid).expect("the legacy session loads"));
        assert!(matches!(store.resolve_path(sid), Some(Location::Jsonl(_))));
        store
            .append(sid, &ChatMessage::user("written after"))
            .unwrap();
        assert!(
            matches!(store.resolve_path(sid), Some(Location::Dir(_))),
            "a write converts the session"
        );
        let mut after = texts(
            &store
                .load_session(sid)
                .expect("the converted session loads"),
        );
        assert_eq!(after.pop().as_deref(), Some("written after"));
        (before, after)
    }

    // ── compaction チェックポイント ──────────────────────────────────

    /// The checkpoint covers everything but the trailing `keep_recent`
    /// messages, so a restore reproduces the state the process had when
    /// it went down rather than replaying the whole file.
    #[test]
    fn a_restore_replays_the_summary_and_the_kept_tail() {
        let (_s, _c, store, sid) = cached_store();
        for i in 0..5 {
            store
                .append(&sid, &ChatMessage::user(format!("m{i}")))
                .unwrap();
        }
        store.append_summary(&sid, "the first three", 2).unwrap();

        let loaded = store.load_session(&sid).expect("the session loads");
        let texts: Vec<String> = loaded
            .iter()
            .flat_map(|m| &m.parts)
            .filter_map(|p| match p {
                ContentPart::Text(t) => Some(t.clone()),
                _ => None,
            })
            .collect();

        assert!(
            texts[0].contains("the first three"),
            "stub first: {texts:?}"
        );
        assert!(
            texts.iter().any(|t| t == "m3"),
            "kept tail missing: {texts:?}"
        );
        assert!(
            texts.iter().any(|t| t == "m4"),
            "kept tail missing: {texts:?}"
        );
        assert!(
            !texts.iter().any(|t| t == "m0"),
            "covered message replayed: {texts:?}"
        );
        assert!(
            !texts.iter().any(|t| t == "m2"),
            "covered message replayed: {texts:?}"
        );
    }

    /// A file with no summary replays whole — there is no checkpoint to
    /// start from.
    #[test]
    fn a_file_without_a_summary_replays_everything() {
        let (_s, _c, store, sid) = cached_store();
        store.append(&sid, &ChatMessage::user("only one")).unwrap();
        let loaded = store.load_session(&sid).unwrap();
        assert_eq!(loaded.len(), 1);
    }

    /// A SummaryLine written before covers_through existed means "this
    /// summary stands in for everything before it in the file" — which
    /// is what the shutdown summary it almost certainly is did mean. The
    /// conversion must keep that meaning.
    #[test]
    fn a_legacy_summary_line_replays_only_what_follows_it() {
        let (_s, _c, store, _) = cached_store();
        let sid = legacy_session(
            &store,
            &[
                legacy_message(0, "before", None),
                r#"{"summary_at":"2026-04-06T11:00:00Z","summary":"what happened"}"#.to_string(),
                legacy_message(30, "after", None),
            ],
        );
        let (before, after) = texts_before_and_after_conversion(&store, &sid);
        for texts in [before, after] {
            assert!(texts[0].contains("what happened"), "stub first: {texts:?}");
            assert!(texts.iter().any(|t| t == "after"), "{texts:?}");
            assert!(!texts.iter().any(|t| t == "before"), "{texts:?}");
        }
    }

    /// A file with two legacy summary lines must use the count at the
    /// *latest* one, not the first — a bug that captured the count only
    /// once (at the first summary) would still pass a test with a single
    /// legacy line, but would silently replay messages the newer summary
    /// already covers.
    #[test]
    fn a_second_legacy_summary_uses_its_own_count_not_the_first() {
        let (_s, _c, store, _) = cached_store();
        let sid = legacy_session(
            &store,
            &[
                legacy_message(0, "m0", None),
                r#"{"summary_at":"2026-04-06T11:00:00Z","summary":"first summary"}"#.to_string(),
                legacy_message(1, "m1", None),
                r#"{"summary_at":"2026-04-06T12:00:00Z","summary":"second summary"}"#.to_string(),
                legacy_message(2, "m2", None),
            ],
        );
        let (before, after) = texts_before_and_after_conversion(&store, &sid);
        for texts in [before, after] {
            assert!(
                texts[0].contains("second summary"),
                "stub must be the latest summary: {texts:?}"
            );
            assert!(texts.iter().any(|t| t == "m2"), "{texts:?}");
            assert!(
                !texts.iter().any(|t| t == "m1"),
                "must not replay what the second summary already covers: {texts:?}"
            );
            assert!(!texts.iter().any(|t| t == "m0"), "{texts:?}");
        }
    }

    /// A checkpoint id that is not in the session (corruption, or a
    /// partial sync) must not silently return nothing — the whole session
    /// replays behind the stub instead, same as one with no checkpoint at
    /// all. This is the failure mode `model_history`'s fallback branch
    /// exists to prevent.
    #[test]
    fn a_checkpoint_not_found_in_the_file_replays_everything() {
        let (_s, _c, store, _) = cached_store();
        let line = serde_json::to_string(&SummaryLine {
            summary_at: "2026-04-06T11:00:00Z".parse().unwrap(),
            summary: "stale checkpoint".to_string(),
            up_to_timestamp: None,
            covers_through: Some(Uuid::now_v7()),
        })
        .unwrap();
        let sid = legacy_session(
            &store,
            &[
                legacy_message(0, "m0", Some(Uuid::now_v7())),
                legacy_message(1, "m1", Some(Uuid::now_v7())),
                line,
            ],
        );
        let (before, after) = texts_before_and_after_conversion(&store, &sid);
        for texts in [before, after] {
            assert!(
                texts[0].contains("stale checkpoint"),
                "stub first: {texts:?}"
            );
            assert!(
                texts.iter().any(|t| t == "m0"),
                "checkpoint miss must fall back to replaying everything: {texts:?}"
            );
            assert!(texts.iter().any(|t| t == "m1"), "{texts:?}");
        }
    }

    /// keep_recent = 0 is the day-boundary compaction: it replaced the
    /// whole in-memory history with a stub, so a restore should too.
    #[test]
    fn a_boundary_compaction_leaves_only_the_stub() {
        let (_s, _c, store, sid) = cached_store();
        for i in 0..3 {
            store
                .append(&sid, &ChatMessage::user(format!("m{i}")))
                .unwrap();
        }
        store.append_summary(&sid, "yesterday", 0).unwrap();

        let loaded = store.load_session(&sid).unwrap();
        assert_eq!(loaded.len(), 2, "stub only: {loaded:?}");
    }

    /// A legacy session's messages may have no id. Converting gives each
    /// one, so a checkpoint written afterwards points at an exact message
    /// rather than degrading to a file-order rule.
    #[test]
    fn a_checkpoint_over_id_less_messages_is_exact_after_conversion() {
        let (_s, _c, store, _) = cached_store();
        let sid = legacy_session(
            &store,
            &(0..3)
                .map(|i| legacy_message(i, &format!("old{i}"), None))
                .collect::<Vec<_>>(),
        );

        store.append_summary(&sid, "the old ones", 1).unwrap();
        store.append(&sid, &ChatMessage::user("new")).unwrap();

        let texts = texts(&store.load_session(&sid).expect("the session still loads"));
        assert!(texts[0].contains("the old ones"), "stub first: {texts:?}");
        // The stub is the summary and its acknowledgement.
        assert_eq!(&texts[2..], ["old2", "new"], "{texts:?}");
    }

    /// Conversion keeps everything the legacy file said — ids, order, the
    /// title, the close — and leaves the `.jsonl` where it was.
    #[test]
    fn conversion_keeps_the_record_and_the_original_file() {
        let (_s, _c, store, _) = cached_store();
        let kept = Uuid::now_v7();
        let sid = legacy_session(
            &store,
            &[
                legacy_message(0, "first", Some(kept)),
                r#"{"session_title":"old title"}"#.to_string(),
                r#"{"digest_at":"2026-04-06T10:30:00Z","since":"x","digest":"ignored"}"#
                    .to_string(),
                legacy_message(1, "second", None),
            ],
        );
        let jsonl = store.absolute_path_for(&sid).unwrap();
        let row = |store: &SessionStore| {
            let rows: Vec<SessionRow> = store
                .session_rows()
                .into_iter()
                .filter(|r| r.meta.session_id == sid)
                .collect();
            assert_eq!(rows.len(), 1, "one session, not two: {rows:?}");
            rows.into_iter().next().unwrap()
        };
        let before = row(&store);

        store.set_title(&sid, "new title").unwrap();

        assert!(jsonl.exists(), "the legacy file is never deleted");
        let (messages, _) = store.load_session_full(&sid).unwrap();
        assert_eq!(messages[0].id, Some(kept), "an existing id survives");
        assert!(messages[1].id.is_some(), "a missing id is filled in");
        assert_eq!(
            texts(
                &messages
                    .into_iter()
                    .map(|m| m.into_chat_message())
                    .collect::<Vec<_>>()
            ),
            ["first", "second"]
        );

        // The directory supersedes the file.
        let after = row(&store);
        assert_eq!(before.meta.title.as_deref(), Some("old title"));
        assert_eq!(after.meta.title.as_deref(), Some("new title"));
        assert_eq!(after.message_count, 2);

        store.close_session(&sid).unwrap();
        assert!(row(&store).is_closed);
    }

    /// A store that has never seen a session finds its converted
    /// directory, not the `.jsonl` beside it.
    #[test]
    fn a_cold_store_resolves_the_converted_directory() {
        let (sessions, _c, store, _) = cached_store();
        let sid = legacy_session(&store, &[legacy_message(0, "hi", None)]);
        store.append(&sid, &ChatMessage::user("again")).unwrap();

        let cold = SessionStore::new(sessions.path().to_path_buf(), "channel", None);
        assert!(matches!(cold.resolve_path(&sid), Some(Location::Dir(_))));
        assert_eq!(cold.load_session(&sid).unwrap().len(), 2);
    }

    /// Reading never converts: only a write does, so sessions that are
    /// only ever looked at stay one file.
    #[test]
    fn reading_a_legacy_session_does_not_convert_it() {
        let (_s, _c, store, _) = cached_store();
        let sid = legacy_session(&store, &[legacy_message(0, "hi", None)]);
        store.load_session(&sid).unwrap();
        store.session_rows();
        let today = local_date_for_timestamp(Local::now(), 4);
        store.sessions_for_day(today, 4);
        assert!(matches!(store.resolve_path(&sid), Some(Location::Jsonl(_))));
        assert!(
            !store
                .absolute_path_for(&sid)
                .unwrap()
                .with_extension("")
                .exists()
        );
    }

    /// Two writers that did not see each other fork the chain. The model
    /// follows one branch; the daily log keeps both.
    #[test]
    fn a_fork_is_kept_in_the_daily_log() {
        let (sessions, _c, store, sid) = cached_store();
        store.append(&sid, &ChatMessage::user("shared")).unwrap();
        // A second process — its own tip cache — appends after the same
        // parent as the next append from `store` will.
        let other = SessionStore::new(sessions.path().to_path_buf(), "channel", None);
        other
            .append(&sid, &ChatMessage::user("from the other host"))
            .unwrap();
        store
            .append(&sid, &ChatMessage::user("from this host"))
            .unwrap();

        let model = texts(&store.load_session(&sid).unwrap());
        assert_eq!(model.len(), 2, "one branch: {model:?}");

        let today = local_date_for_timestamp(Local::now(), 4);
        let day: Vec<String> = store
            .sessions_for_day(today, 4)
            .into_iter()
            .flat_map(|(_, ms)| ms)
            .map(|m| m.into_chat_message())
            .flat_map(|m| m.parts)
            .filter_map(|p| match p {
                ContentPart::Text(t) => Some(t),
                _ => None,
            })
            .collect();
        assert_eq!(day.len(), 3, "both branches: {day:?}");
    }

    /// A message's text is the file's body, readable as Markdown.
    #[test]
    fn a_message_is_its_own_markdown_file() {
        let (_s, _c, store, sid) = cached_store();
        store
            .append(&sid, &ChatMessage::user("# a heading\n\nand a paragraph"))
            .unwrap();
        let dir = store.absolute_path_for(&sid).unwrap();
        let files: Vec<PathBuf> = fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.file_name().unwrap() != session_log::META_FILE)
            .collect();
        assert_eq!(files.len(), 1);
        let raw = fs::read_to_string(&files[0]).unwrap();
        assert!(raw.starts_with("---\n"), "{raw}");
        assert!(raw.ends_with("\n\n# a heading\n\nand a paragraph"), "{raw}");
        assert!(raw.contains("type: message"), "{raw}");
        assert!(
            !raw.contains("parts"),
            "a lone text needs no part list: {raw}"
        );
    }

    /// The checkpoint must not land between a tool_use and its result.
    /// find_safe_split_point guarantees it on the write side; this pins
    /// that the read side stays loadable if it ever does not.
    #[test]
    fn a_checkpoint_cutting_a_pair_still_loads_paired() {
        let (_s, _c, store, sid) = cached_store();
        store.append(&sid, &tool_use_msg("c1")).unwrap();
        store.append(&sid, &tool_result_msg("c1", "ok")).unwrap();
        // Cover the tool_use, keep only the result — the gap the repair
        // exists for.
        store.append_summary(&sid, "did a thing", 1).unwrap();

        let loaded = store.load_session(&sid).unwrap();
        let stray = loaded.iter().enumerate().any(|(i, m)| {
            m.parts
                .iter()
                .any(|p| matches!(p, ContentPart::ToolResult { .. }))
                && !loaded.get(i.wrapping_sub(1)).is_some_and(|prev| {
                    prev.parts
                        .iter()
                        .any(|p| matches!(p, ContentPart::ToolUse { .. }))
                })
        });
        assert!(!stray, "an unpaired tool_result survived: {loaded:?}");
    }

    /// The point of the whole change: a restarted room remembers what it
    /// did, not just what was said.
    #[test]
    fn load_all_restores_tool_traffic_for_an_active_session() {
        let (_s, _c, store, sid) = cached_store();
        store.append(&sid, &ChatMessage::user("read a.rs")).unwrap();
        store.append(&sid, &tool_use_msg("c1")).unwrap();
        store
            .append(&sid, &tool_result_msg("c1", "fn main() {}"))
            .unwrap();

        let (active, histories) = store.load_all();
        let key = ("!room:example.org".to_string(), None);
        assert_eq!(active.get(&key).map(String::as_str), Some(sid.as_str()));

        let history = histories.get(&key).expect("history restored");
        assert!(
            history.iter().flat_map(|m| &m.parts).any(|p| matches!(p, ContentPart::ToolResult { tool_use_id, content } if tool_use_id == "c1" && content == "fn main() {}")),
            "the tool result did not come back: {history:?}"
        );
    }

    /// A closed session is not resumed — the day boundary rotated it on
    /// purpose.
    #[test]
    fn load_all_skips_closed_sessions() {
        let (_s, _c, store, sid) = cached_store();
        store.append(&sid, &ChatMessage::user("hi")).unwrap();
        store.close_session(&sid).unwrap();

        let (active, histories) = store.load_all();
        assert!(active.is_empty(), "{active:?}");
        assert!(histories.is_empty(), "{histories:?}");
    }

    /// The subtlest property in `load_all`: when a key has more than one
    /// active session file, the newest wins — both for `active` (which
    /// file gets appended to next) and for `histories` (the older
    /// session's history must not leak through just because it happened
    /// to load first). A newer session that is itself empty must evict
    /// whatever the older one contributed, not merely fail to overwrite
    /// it.
    #[test]
    fn load_all_prefers_the_newest_session_and_drops_history_when_it_is_empty() {
        let (_s, _c, store, old_sid) = cached_store();
        store.append(&old_sid, &ChatMessage::user("old")).unwrap();

        let key = ("!room:example.org".to_string(), None);
        let new_sid = store.create_session(&key, "matrix", "default").unwrap();
        assert!(
            new_sid > old_sid,
            "UUIDv7 ids must sort newer-last for this test to mean anything: \
             {old_sid} vs {new_sid}"
        );

        let (active, histories) = store.load_all();
        assert_eq!(
            active.get(&key),
            Some(&new_sid),
            "the newest session file must be the active one"
        );
        assert!(
            !histories.contains_key(&key),
            "the newer, empty session must evict the older session's \
             history, not lose to it: {histories:?}"
        );
    }

    /// The autonomous store names its files by agent-day, and every write
    /// path has to still find them: `append` is the first thing that runs
    /// after the session is created.
    ///
    /// Both halves matter. The first store sees every resolution as a
    /// `path_cache` hit, so it only pins the name `create_session`
    /// wrote; the second is opened cold over the same directory and is
    /// what actually exercises the Dated branch of `resolve_path` — the
    /// scan that strips `-{session_id}.jsonl` and checks for the
    /// 10-character date in front of it.
    #[test]
    fn a_dated_store_creates_and_resolves_its_own_filenames() {
        let tmp = tempfile::tempdir().unwrap();
        let store =
            SessionStore::new(tmp.path().to_path_buf(), "autonomous", None).with_dated_files(4);

        let sid = store
            .create_autonomous_session("refactor", "default")
            .unwrap();

        let path = store
            .absolute_path_for(&sid)
            .expect("the file must resolve");
        let name = path.file_name().unwrap().to_str().unwrap();
        let expected_date = local_date_for_timestamp(Local::now(), 4).to_string();
        assert_eq!(name, format!("{expected_date}-{sid}"));
        assert!(path.starts_with(tmp.path().join("default").join("autonomous")));

        store.append(&sid, &ChatMessage::user("hello")).unwrap();
        assert_eq!(store.load_session(&sid).unwrap().len(), 1);

        // The meta line carries what the loop searches by.
        let rows = store.session_rows();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].meta.room_id, "refactor");
        assert_eq!(rows[0].meta.channel, "server");
        assert_eq!(rows[0].meta.namespace.as_deref(), Some("default"));

        // Cold cache: a store that never created this session must still
        // find it by name, through the Dated suffix scan rather than the
        // path cache.
        let cold =
            SessionStore::new(tmp.path().to_path_buf(), "autonomous", None).with_dated_files(4);
        let resolved = cold
            .absolute_path_for(&sid)
            .expect("a cold store must resolve the dated file");
        assert_eq!(resolved, path);
        assert_eq!(cold.load_session(&sid).unwrap().len(), 1);
        assert_eq!(cold.session_rows().len(), 1);

        // The scan is keyed on the id, and the date prefix is not one:
        // an id embedded in a different day's name is a different file.
        let mut longer_sid = sid.clone();
        longer_sid.push('0');
        assert!(
            cold.absolute_path_for(&longer_sid).is_none(),
            "the id must match whole, not as a prefix of a longer name"
        );

        // ... and a name that ends in the right id but has no 10-char
        // date before it is not this session either.
        std::fs::write(
            tmp.path()
                .join("default")
                .join("autonomous")
                .join(format!("backup-{sid}.jsonl")),
            "",
        )
        .unwrap();
        let cold_again =
            SessionStore::new(tmp.path().to_path_buf(), "autonomous", None).with_dated_files(4);
        assert_eq!(
            cold_again.absolute_path_for(&sid).unwrap(),
            path,
            "the 10-character-date requirement must pick the dated file"
        );
    }

    /// A plain store's directory is named exactly `{session_id}`, and a
    /// dated-looking neighbour does not resolve.
    ///
    /// The collision check goes through a fresh store, because the one
    /// that created the session answers from `path_cache` and would not
    /// notice if the exact-name match had been replaced by the Dated
    /// suffix match.
    #[test]
    fn a_plain_store_still_matches_its_filename_exactly() {
        let tmp = tempfile::tempdir().unwrap();
        let store = SessionStore::new(tmp.path().to_path_buf(), "channel", None);
        let sid = store
            .create_session(&("room".to_string(), None), "matrix", "default")
            .unwrap();

        let path = store.absolute_path_for(&sid).unwrap();
        assert_eq!(path.file_name().unwrap().to_str().unwrap(), sid);

        // Same id, a dated filename: must not be picked up by the exact match.
        let ns = tmp.path().join("default").join("channel");
        std::fs::write(ns.join(format!("2026-09-12-{sid}.jsonl")), "").unwrap();
        assert_eq!(
            store
                .resolve_path_for_test(&sid)
                .unwrap()
                .file_name()
                .unwrap(),
            std::ffi::OsStr::new(&sid)
        );

        // Cold cache: the exact-name branch of `resolve_path` is what
        // decides here, and it must ignore the dated neighbour.
        let cold = SessionStore::new(tmp.path().to_path_buf(), "channel", None);
        assert_eq!(
            cold.absolute_path_for(&sid).unwrap().file_name().unwrap(),
            std::ffi::OsStr::new(&sid)
        );
    }
}
