//! The ACP session store.
//!
//! Separate from `SessionStore` because ACP is an externally-defined
//! standard that will drift from the format we chose for ourselves, and
//! because an editor's thread list should not be showing Matrix
//! conversations. The directory-walking machinery is shared; the line
//! format is not.
//!
//! Every event carries a UUIDv7 `id` and the `parent` it was appended
//! after. That pair cannot be reconstructed later, which is why it is
//! here now even though nothing reads it yet: it is what will make an
//! offline divergence detectable if remote-workspace sync is ever
//! implemented, and what makes splitting one file per event a
//! mechanical move rather than a redesign.
//!
//! **The chain is the authority on order, not the id's timestamp.**
//! UUIDv7 embeds the writer's clock, so two devices with skewed clocks
//! produce ids that sort wrongly against each other.

use crate::provider::{ChatMessage, ContentPart, Role};
use crate::session::{SessionMeta, StoredMessage};
use crate::session_storage::{MISSING_RESULT, missing_input};
use crate::tool_payload_cache::ToolPayloadCache;
use anyhow::Result;
use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tracing::warn;
use uuid::Uuid;

/// The directory name under `<sessions>/<namespace>/`.
const KIND: &str = "acp";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionHeader {
    pub session_id: String,
    pub namespace: String,
    /// The client's workspace root. Required — an ACP session always
    /// reports one, which is why this is not an `Option` the way the
    /// old store's `cwd` had to be.
    pub cwd: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Event {
    pub id: Uuid,
    /// `None` means this is the session's first event. The header is
    /// not a root event — making it one would need a special case in
    /// every traversal.
    pub parent: Option<Uuid>,
    pub at: DateTime<Utc>,
    pub body: EventBody,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum EventBody {
    Message {
        role: Role,
        parts: Vec<StoredPart>,
    },
    Title {
        title: String,
    },
    Closed,
    /// A compaction summary and the message it absorbed up to.
    ///
    /// Not a message: `history()` and the daily-log projection skip it,
    /// because the editor's transcript and the permanent record are both
    /// about what was said. Only `history_for_model` reads it.
    Summary {
        summary: String,
        covers_through: Uuid,
    },
}

/// A message part as it appears on disk.
///
/// Distinct from `provider::ContentPart` on purpose: a tool result is a
/// hash here and the real content in memory, and that difference exists
/// only at the storage boundary. `image_cache` put its `ImageRef` into
/// `ContentPart` because those references stay in the in-memory history
/// to avoid re-billing; a tool result has to be whole when the model
/// sees it, so the reference never leaves the disk.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum StoredPart {
    Text(String),
    /// **Legacy, read-only.** What this store wrote before inputs were
    /// cached (#212). Nothing produces it any more; it stays so that a
    /// session file written by an earlier build still loads its calls
    /// instead of silently dropping them — and dropping a `tool_use`
    /// while keeping its `tool_result` is exactly the orphan the repair
    /// pass then has to clean up.
    ///
    /// Delete it once no reachable session predates the change.
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolUseRef {
        id: String,
        /// Stays inline. It is short, and it is what `generate_summary`
        /// renders as `[Called tool: {name}]` — behind the hash, a
        /// summary of an evicted session would say nothing at all about
        /// what the agent did.
        name: String,
        /// `None` means the input had nowhere to be stored, exactly as
        /// for a result below.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sha256: Option<String>,
    },
    ToolResultRef {
        tool_use_id: String,
        /// `None` means the result had nowhere to be stored — the cache
        /// was unavailable when this line was written. Distinct from a
        /// hash whose entry has since been evicted, but only in how it
        /// arose: a reader treats both as "the pairing is here, the
        /// content is not", and `load_part` produces the same
        /// `MISSING_RESULT` for each.
        ///
        /// `Option<String>` also reads a bare string as `Some`, so a
        /// line written before this field became optional still loads.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sha256: Option<String>,
    },
    /// Anything the storage layer does not model — images, for now.
    /// Kept as a marker so the message is not silently emptied.
    Other,
}

/// One line of the log. Everything is `kind`-tagged, including the
/// header: a single internally-tagged enum has no ambiguity, whereas
/// mixing a tagged event with an untagged `{"header": …}` wrapper does.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Line {
    Header(SessionHeader),
    Message {
        id: Uuid,
        parent: Option<Uuid>,
        at: DateTime<Utc>,
        role: Role,
        parts: Vec<StoredPart>,
    },
    Title {
        id: Uuid,
        parent: Option<Uuid>,
        at: DateTime<Utc>,
        title: String,
    },
    Closed {
        id: Uuid,
        parent: Option<Uuid>,
        at: DateTime<Utc>,
    },
    Summary {
        id: Uuid,
        parent: Option<Uuid>,
        at: DateTime<Utc>,
        summary: String,
        covers_through: Uuid,
    },
}

pub struct AcpSessionStore {
    base_dir: PathBuf,
    /// `None` when the tool-payload cache directory could not be opened
    /// at startup (read-only or missing `~/.cache` / `%LOCALAPPDATA%`).
    /// Degrades rather than making the whole store unusable: a session
    /// must still load. ACP has persisted tool traffic since #191, and
    /// #194 put every transport on this cache, so `None` is live here in
    /// production: `store_part`'s `ToolResult` arm writes a
    /// `ToolResultRef` with no hash instead of the content, and a reload
    /// reads that back as `MISSING_RESULT` — the pairing survives, only
    /// the result body is lost.
    cache: Option<Arc<ToolPayloadCache>>,
    /// `session_id` → the id of the last event written.
    ///
    /// An append needs its parent, and re-reading the whole file to
    /// find the tip would make every write cost the length of the
    /// conversation. Populated lazily on the first append to a session.
    tips: Mutex<HashMap<String, Uuid>>,
}

impl AcpSessionStore {
    pub fn new(base_dir: PathBuf, cache: Option<Arc<ToolPayloadCache>>) -> Self {
        Self {
            base_dir,
            cache,
            tips: Mutex::new(HashMap::new()),
        }
    }

    fn path(&self, session_id: &str, namespace: &str) -> PathBuf {
        self.base_dir
            .join(namespace)
            .join(KIND)
            .join(format!("{session_id}.jsonl"))
    }

    /// Find an existing session's file by scanning namespaces.
    fn find(&self, session_id: &str) -> Option<PathBuf> {
        let target = format!("{session_id}.jsonl");
        crate::session::collect_session_files(&self.base_dir, KIND)
            .into_iter()
            .find(|p| p.file_name().and_then(|s| s.to_str()) == Some(target.as_str()))
    }

    #[cfg(test)]
    pub fn path_for_test(&self, session_id: &str) -> PathBuf {
        self.find(session_id).expect("the session exists")
    }

    /// Absolute path of an existing session's file, for callers that
    /// need to read raw bytes (e.g. `mtime` for `updated_at`) rather than
    /// going through the store's own accessors. Mirrors the old
    /// `SessionStore::absolute_path_for`, which `session/list` used the
    /// same way.
    pub fn absolute_path_for(&self, session_id: &str) -> Option<PathBuf> {
        self.find(session_id)
    }

    pub fn create(&self, session_id: &str, namespace: &str, cwd: &str) -> Result<()> {
        let path = self.path(session_id, namespace);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let header = Line::Header(SessionHeader {
            session_id: session_id.to_string(),
            namespace: namespace.to_string(),
            cwd: cwd.to_string(),
            created_at: Utc::now(),
        });
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        writeln!(file, "{}", serde_json::to_string(&header)?)?;
        Ok(())
    }

    /// Read the tip, write `body(parent)` as the next line, and record
    /// the new tip — all under one held lock.
    ///
    /// `tip()` and the write used to be separate lock acquisitions, so
    /// two threads appending to the *same session* on the *same store*
    /// could both observe the same tip and both write with that parent,
    /// fabricating a fork that means nothing (unlike the cross-process
    /// case this format is meant to tolerate, where a fork reflects a
    /// writer that genuinely didn't know about the other's latest
    /// event). Holding `tips` across the read-modify-write closes that
    /// window: the whole "who is the tip, append after them, become the
    /// tip" sequence is now one atomic step. Reading the file to find a
    /// lazy tip happens while the lock is held too — `events()` never
    /// touches `tips`, so this cannot deadlock.
    fn append_line(
        &self,
        session_id: &str,
        id: Uuid,
        body: impl FnOnce(Option<Uuid>) -> Line,
    ) -> Result<()> {
        let mut tips = self.tips.lock().unwrap();
        let parent = match tips.get(session_id) {
            Some(tip) => Some(*tip),
            None => self
                .events(session_id)
                .and_then(|evs| evs.last().map(|e| e.id)),
        };
        let path = self
            .find(session_id)
            .ok_or_else(|| anyhow::anyhow!("no ACP session '{session_id}'"))?;
        let mut file = std::fs::OpenOptions::new().append(true).open(&path)?;
        writeln!(file, "{}", serde_json::to_string(&body(parent))?)?;
        tips.insert(session_id.to_string(), id);
        Ok(())
    }

    pub fn append_message(&self, session_id: &str, msg: &ChatMessage) -> Result<()> {
        let parts = msg
            .parts
            .iter()
            .map(|part| self.store_part(part))
            .collect::<Result<Vec<_>>>()?;
        let id = Uuid::now_v7();
        let role = msg.role.clone();
        self.append_line(session_id, id, move |parent| Line::Message {
            id,
            parent,
            at: Utc::now(),
            role,
            parts,
        })
    }

    pub fn append_title(&self, session_id: &str, title: &str) -> Result<()> {
        let id = Uuid::now_v7();
        let title = title.to_string();
        self.append_line(session_id, id, move |parent| Line::Title {
            id,
            parent,
            at: Utc::now(),
            title,
        })
    }

    /// Record a compaction summary and how far it reaches.
    ///
    /// `keep_recent` is how many trailing messages the caller kept
    /// verbatim. The cursor is resolved here, against this store's own
    /// events — the caller holds an index into its in-memory history and
    /// cannot map it onto the log, which holds at least as many
    /// messages because earlier compactions trimmed memory and not the
    /// file.
    ///
    /// Resolved against the chain, not file order: `history_for_model`
    /// walks the chain to find `covers_through` again, and file order
    /// only agrees with the chain for a session one process wrote — the
    /// same reason `chain`'s own doc gives for not trusting file order.
    /// A session with no messages gets no checkpoint; there is nothing
    /// for one to point at.
    pub fn append_summary(
        &self,
        session_id: &str,
        summary: &str,
        keep_recent: usize,
    ) -> Result<()> {
        let events = self.events(session_id).unwrap_or_default();
        let message_ids: Vec<Uuid> = self
            .chain(session_id, &events)
            .into_iter()
            .filter(|e| matches!(e.body, EventBody::Message { .. }))
            .map(|e| e.id)
            .collect();
        if message_ids.is_empty() {
            return Ok(());
        }
        let covered = message_ids.len().saturating_sub(keep_recent).max(1);
        let covers_through = message_ids[covered - 1];
        let id = Uuid::now_v7();
        let summary = summary.to_string();
        self.append_line(session_id, id, move |parent| Line::Summary {
            id,
            parent,
            at: Utc::now(),
            summary,
            covers_through,
        })
    }

    /// ACP exposes no close-or-delete operation yet, so nothing in
    /// production ever writes a `Closed` event — the read path's refusal
    /// of a closed session (`adopt_session` in `src/serve/acp.rs`) is
    /// defensive, ahead of there being any way to produce one. This is
    /// compiled only for the tests that exercise that refusal path,
    /// which is why it is `#[cfg(test)]` rather than a real store method
    /// right now: once a real close/delete request lands, its handler
    /// becomes this method's production caller and the attribute comes
    /// off.
    #[cfg(test)]
    pub fn close(&self, session_id: &str) -> Result<()> {
        let id = Uuid::now_v7();
        self.append_line(session_id, id, move |parent| Line::Closed {
            id,
            parent,
            at: Utc::now(),
        })
    }

    /// Both halves of a tool call go to the cache; the log keeps hashes.
    fn store_part(&self, part: &ContentPart) -> Result<StoredPart> {
        Ok(match part {
            ContentPart::Text(t) => StoredPart::Text(t.clone()),
            ContentPart::ToolUse { id, name, input } => {
                // An input is cached for the same reason a result is: a
                // `file_write` call carries the file's contents, this
                // file lives under `workspace_dir/sessions`, and the
                // retrieve indexer walks it line by line. It used to be
                // written inline and merely elided above a size
                // threshold, which bounded the worst case without
                // addressing what the requirement actually was (#212).
                let sha256 = match &self.cache {
                    Some(cache) => Some(cache.put_input(input)?),
                    None => {
                        warn!(
                            "Tool-payload cache unavailable; recording the input of \
                             '{id}' with no content"
                        );
                        None
                    }
                };
                StoredPart::ToolUseRef {
                    id: id.clone(),
                    name: name.clone(),
                    sha256,
                }
            }
            ContentPart::ToolResult {
                tool_use_id,
                content,
            } => {
                // A cache miss on read degrades gracefully; a missing
                // cache on write must not be allowed to degrade any
                // further. Writing `Other` here would drop the
                // `tool_use_id`, leaving a `tool_use` with no matching
                // `tool_result` — which the API rejects outright, so the
                // session would fail to load rather than load thinner.
                let sha256 = match &self.cache {
                    Some(cache) => Some(cache.put(content)?),
                    None => {
                        warn!(
                            "Tool-payload cache unavailable; recording '{tool_use_id}' \
                             with no content"
                        );
                        None
                    }
                };
                StoredPart::ToolResultRef {
                    tool_use_id: tool_use_id.clone(),
                    sha256,
                }
            }
            // Images are not carried by this version. Recorded as a
            // marker rather than dropped, so a message that was only an
            // image does not read back as an empty one.
            _ => StoredPart::Other,
        })
    }
}

impl AcpSessionStore {
    fn lines(&self, session_id: &str) -> Option<Vec<Line>> {
        let path = self.find(session_id)?;
        let text = std::fs::read_to_string(path).ok()?;
        Some(
            text.lines()
                .filter(|l| !l.trim().is_empty())
                .filter_map(|l| match serde_json::from_str::<Line>(l) {
                    Ok(line) => Some(line),
                    Err(e) => {
                        warn!("ACP session {session_id}: skipping unreadable line: {e}");
                        None
                    }
                })
                .collect(),
        )
    }

    /// Every event in the order the file holds them.
    ///
    /// File order, not chain order — Task 3's reader is what walks the
    /// chain. They agree for a session written by one process, which is
    /// every session today.
    pub fn events(&self, session_id: &str) -> Option<Vec<Event>> {
        Some(
            self.lines(session_id)?
                .into_iter()
                .filter_map(|l| match l {
                    Line::Header(_) => None,
                    Line::Message {
                        id,
                        parent,
                        at,
                        role,
                        parts,
                    } => Some(Event {
                        id,
                        parent,
                        at,
                        body: EventBody::Message { role, parts },
                    }),
                    Line::Title {
                        id,
                        parent,
                        at,
                        title,
                    } => Some(Event {
                        id,
                        parent,
                        at,
                        body: EventBody::Title { title },
                    }),
                    Line::Closed { id, parent, at } => Some(Event {
                        id,
                        parent,
                        at,
                        body: EventBody::Closed,
                    }),
                    Line::Summary {
                        id,
                        parent,
                        at,
                        summary,
                        covers_through,
                    } => Some(Event {
                        id,
                        parent,
                        at,
                        body: EventBody::Summary {
                            summary,
                            covers_through,
                        },
                    }),
                })
                .collect(),
        )
    }

    /// What the listing needs about one session, oldest first.
    ///
    /// Scoped to a namespace rather than global: another namespace's
    /// sessions are not this caller's to see. The ACP handler checks
    /// the boundary too — this is the store declining to hand them over
    /// in the first place.
    ///
    /// Everything here comes out of the one read the header already
    /// requires, which is why `has_messages` and `title` are computed
    /// eagerly rather than left to per-session follow-up calls.
    pub fn list_summaries(&self, namespace: &str) -> Vec<SessionSummary> {
        let dir = self.base_dir.join(namespace).join(KIND);
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return Vec::new();
        };
        let mut out: Vec<SessionSummary> = entries
            .flatten()
            .filter_map(|e| {
                let stem = e.path().file_stem()?.to_str()?.to_string();
                self.summary(&stem)
            })
            .collect();
        out.sort_by_key(|s| s.header.created_at);
        out
    }

    /// One session's listing row. Public because `session/load` and
    /// `session/resume` need `cwd`, `title` and `is_closed` for a single
    /// session without paying for the whole namespace's listing.
    pub fn summary(&self, session_id: &str) -> Option<SessionSummary> {
        let lines = self.lines(session_id)?;
        let mut header = None;
        let mut title = None;
        let mut has_messages = false;
        let mut is_closed = false;
        for line in lines {
            match line {
                Line::Header(h) => header = Some(h),
                Line::Message { .. } => has_messages = true,
                // Last title wins: a session can be retitled.
                Line::Title { title: t, .. } => title = Some(t),
                Line::Closed { .. } => is_closed = true,
                Line::Summary { .. } => {}
            }
        }
        Some(SessionSummary {
            header: header?,
            title,
            has_messages,
            is_closed,
        })
    }

    /// The events on the chain, in order, starting from the root.
    ///
    /// File order is only accidentally right — it agrees with the chain
    /// for a session one process wrote, and says nothing useful for one
    /// that was synced or merged.
    fn chain<'a>(&self, session_id: &str, events: &'a [Event]) -> Vec<&'a Event> {
        // parent -> children, so the walk is a lookup rather than a scan.
        let mut children: HashMap<Option<Uuid>, Vec<&Event>> = HashMap::new();
        for event in events {
            children.entry(event.parent).or_default().push(event);
        }
        let mut out = Vec::with_capacity(events.len());
        let mut cursor = None;
        loop {
            let Some(next) = children.get(&cursor) else {
                break;
            };
            if next.len() > 1 {
                warn!(
                    "ACP session {session_id}: {} events share one parent; taking the first branch. Two writers appended to this session and there is no merge story yet.",
                    next.len()
                );
            }
            let event = next[0];
            out.push(event);
            cursor = Some(event.id);
            // A hand-edited or partially-synced file could contain a
            // cycle. The chain cannot be longer than the file.
            if out.len() > events.len() {
                warn!("ACP session {session_id}: the parent chain cycles; stopping");
                break;
            }
        }
        out
    }

    /// The whole conversation, for the editor's `session/load` replay.
    ///
    /// Whole on purpose: the editor keeps no transcript of its own, so a
    /// trimmed replay would render a thread with a hole in it. What the
    /// *model* sees is `history_for_model`, which starts from the latest
    /// compaction checkpoint.
    pub fn history(&self, session_id: &str) -> Option<Vec<ChatMessage>> {
        let events = self.events(session_id)?;
        let out = self.messages_from(&self.chain(session_id, &events));
        Some(crate::session_storage::repair_tool_pairing(out))
    }

    /// The conversation as the model should see it: the latest
    /// compaction summary rendered as a stub, then the messages it did
    /// not absorb.
    ///
    /// Compression runs on ACP turns like every other transport, and
    /// used to throw its summary away on the grounds that the events
    /// answer the same question. They do — but they answer it with the
    /// *whole* session, so every reload replayed everything and paid for
    /// the same compaction again on the first turn back.
    pub fn history_for_model(&self, session_id: &str) -> Option<Vec<ChatMessage>> {
        let events = self.events(session_id)?;
        let chain = self.chain(session_id, &events);

        let checkpoint = chain.iter().rev().find_map(|e| match &e.body {
            EventBody::Summary {
                summary,
                covers_through,
            } => Some((summary.clone(), *covers_through)),
            _ => None,
        });

        let Some((summary, covers_through)) = checkpoint else {
            let out = self.messages_from(&chain);
            return Some(crate::session_storage::repair_tool_pairing(out));
        };

        let messages: Vec<&Event> = chain
            .iter()
            .copied()
            .filter(|e| matches!(e.body, EventBody::Message { .. }))
            .collect();
        let start = match messages.iter().position(|e| e.id == covers_through) {
            Some(pos) => pos + 1,
            None => {
                warn!(
                    "ACP session {session_id}: checkpoint {covers_through} is not on the chain; replaying everything"
                );
                0
            }
        };

        let mut out = crate::context_compression::compaction_stub(&summary);
        out.extend(self.messages_from(&messages[start..]));
        Some(crate::session_storage::repair_tool_pairing(out))
    }

    /// Project the message events among `events` into `ChatMessage`s,
    /// hydrating each part.
    fn messages_from(&self, events: &[&Event]) -> Vec<ChatMessage> {
        events
            .iter()
            .filter_map(|e| match &e.body {
                EventBody::Message { role, parts } => Some(ChatMessage {
                    role: role.clone(),
                    parts: parts.iter().map(|p| self.load_part(p)).collect(),
                    input_kind: None,
                    user_id: None,
                }),
                _ => None,
            })
            .collect()
    }

    fn load_part(&self, part: &StoredPart) -> ContentPart {
        match part {
            StoredPart::Text(t) => ContentPart::Text(t.clone()),
            // Legacy: the input is inline, so there is nothing to fetch.
            StoredPart::ToolUse { id, name, input } => ContentPart::ToolUse {
                id: id.clone(),
                name: name.clone(),
                input: input.clone(),
            },
            StoredPart::ToolUseRef { id, name, sha256 } => ContentPart::ToolUse {
                id: id.clone(),
                name: name.clone(),
                // Absent either way — no hash was ever written, or the
                // hash's entry is gone. The call itself survives, which
                // is what the pairing needs; `name` keeps the record of
                // what was attempted from being nothing at all.
                input: sha256
                    .as_ref()
                    .and_then(|sha| self.cache.as_ref()?.get_input(sha))
                    .unwrap_or_else(missing_input),
            },
            StoredPart::ToolResultRef {
                tool_use_id,
                sha256,
            } => ContentPart::ToolResult {
                tool_use_id: tool_use_id.clone(),
                // Absent either way — no hash was ever written, or the
                // hash's entry is gone. The model can call the tool
                // again if it needs to; what it cannot recover from is
                // an unpaired `tool_use`.
                content: sha256
                    .as_ref()
                    .and_then(|sha| self.cache.as_ref()?.get(sha))
                    .unwrap_or_else(|| MISSING_RESULT.to_string()),
            },
            StoredPart::Other => {
                ContentPart::Text("[a message part that this version does not store]".to_string())
            }
        }
    }
}

/// One row of the session list.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionSummary {
    pub header: SessionHeader,
    pub title: Option<String>,
    /// False for a session the editor opened and never typed into.
    pub has_messages: bool,
    pub is_closed: bool,
}

impl AcpSessionStore {
    /// Listing rows for every session this store holds, newest activity
    /// last — the ACP half of what `session_list` renders.
    ///
    /// One pass per file, and no message bodies: a listing wants counts
    /// and timestamps, and an ACP session's events carry whole tool
    /// inputs. `SessionRow` is `session::`'s type on purpose — the tool
    /// merges rows from every store and should not be teaching itself a
    /// second shape per store.
    pub fn session_rows(&self) -> Vec<crate::session::SessionRow> {
        let mut rows = Vec::new();
        for session_id in self.all_session_ids() {
            let Some(lines) = self.lines(&session_id) else {
                continue;
            };
            let mut header = None;
            let mut title = None;
            let mut is_closed = false;
            let mut message_count = 0usize;
            let mut last_at = None;
            for line in lines {
                match line {
                    Line::Header(h) => header = Some(h),
                    Line::Message { at, .. } => {
                        message_count += 1;
                        last_at = Some(at);
                    }
                    // Last title wins: a session can be retitled.
                    Line::Title { title: t, .. } => title = Some(t),
                    Line::Closed { .. } => is_closed = true,
                    Line::Summary { .. } => {}
                }
            }
            let Some(header) = header else {
                continue;
            };
            let summary = SessionSummary {
                header,
                title,
                has_messages: message_count > 0,
                is_closed,
            };
            rows.push(crate::session::SessionRow {
                meta: self.project_meta(&summary),
                message_count,
                last_at,
                is_closed,
            });
        }
        rows.sort_by_key(|r| r.last_at.unwrap_or(r.meta.created_at));
        rows
    }

    /// This store's sessions in the vocabulary the log and listing
    /// builders share. `channel` is `"acp"` so those builders can route
    /// on it the way they already route on `"rpc"` and
    /// `"device-default"`.
    fn project_meta(&self, summary: &SessionSummary) -> SessionMeta {
        let header = &summary.header;
        SessionMeta {
            session_id: header.session_id.clone(),
            // ACP has no rooms. Empty rather than synthetic, so a
            // room-derived namespace lookup can never accidentally
            // match one.
            room_id: String::new(),
            thread_id: None,
            channel: "acp".to_string(),
            created_at: header.created_at,
            public_id: None,
            namespace: Some(header.namespace.clone()),
            device_id: None,
            room_profile: None,
            title: summary.title.clone(),
        }
    }

    /// Every session id this store holds, across namespaces.
    fn all_session_ids(&self) -> Vec<String> {
        crate::session::collect_session_files(&self.base_dir, KIND)
            .into_iter()
            .filter_map(|p| Some(p.file_stem()?.to_str()?.to_string()))
            .collect()
    }
}

impl AcpSessionStore {
    /// This store's sessions for one local day, in the shape
    /// `format_sessions` reads.
    ///
    /// Only text is projected. A tool result whose content has fallen
    /// out of the cache would otherwise write its placeholder sentence
    /// into a permanent, searchable record — and the daily log is a
    /// narrative of what was said, not a transcript of what was read.
    pub fn sessions_for_day(
        &self,
        date: NaiveDate,
        boundary_hour: u8,
    ) -> Vec<(SessionMeta, Vec<StoredMessage>)> {
        let (day_start, day_end) = crate::session::day_window(date, boundary_hour);
        let mut out = Vec::new();
        for session_id in self.all_session_ids() {
            let Some(summary) = self.summary(&session_id) else {
                continue;
            };
            let Some(events) = self.events(&session_id) else {
                continue;
            };
            let messages: Vec<StoredMessage> = events
                .iter()
                .filter(|e| e.at >= day_start && e.at < day_end)
                .filter_map(|e| match &e.body {
                    EventBody::Message { role, parts } => {
                        let text: Vec<ContentPart> = parts
                            .iter()
                            .filter_map(|p| match p {
                                StoredPart::Text(t) if !t.trim().is_empty() => {
                                    Some(ContentPart::Text(t.clone()))
                                }
                                _ => None,
                            })
                            .collect();
                        if text.is_empty() {
                            return None;
                        }
                        Some(StoredMessage {
                            id: None,
                            timestamp: e.at,
                            role: role.clone(),
                            parts: text,
                            input_kind: None,
                            user_id: None,
                        })
                    }
                    _ => None,
                })
                .collect();
            if messages.is_empty() {
                continue;
            }
            out.push((self.project_meta(&summary), messages));
        }
        out.sort_by_key(|(meta, _)| meta.created_at);
        out
    }

    /// Local dates on which this store has at least one message from a
    /// session `predicate` accepts, so daily-log catch-up knows which
    /// days are pending *for its own namespace*. Without this filter
    /// every namespace's catch-up would see every other namespace's
    /// ACP dates as pending too: `generate_daily_log` would then find
    /// no in-namespace sessions for that date, write nothing, and the
    /// date would stay pending forever — re-walked on every startup
    /// and catch-up tick.
    pub fn session_dates<F>(&self, boundary_hour: u8, predicate: F) -> Vec<NaiveDate>
    where
        F: Fn(&SessionMeta) -> bool,
    {
        let mut dates = std::collections::HashSet::new();
        for session_id in self.all_session_ids() {
            let Some(summary) = self.summary(&session_id) else {
                continue;
            };
            if !predicate(&self.project_meta(&summary)) {
                continue;
            }
            let Some(events) = self.events(&session_id) else {
                continue;
            };
            for event in events {
                if !matches!(event.body, EventBody::Message { .. }) {
                    continue;
                }
                let local = event.at.with_timezone(&chrono::Local);
                dates.insert(crate::session::local_date_for_timestamp(
                    local,
                    boundary_hour,
                ));
            }
        }
        let mut sorted: Vec<NaiveDate> = dates.into_iter().collect();
        sorted.sort();
        sorted
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{ChatMessage, ContentPart, Role};

    /// The local date "now" belongs to under `boundary_hour`.
    ///
    /// Deliberately not `Local::now().date_naive()`. A timestamp before
    /// the boundary hour belongs to the *previous* local day, so the
    /// naive date and the store's day window disagree for those hours —
    /// which made these tests pass all day and fail only when CI
    /// happened to run between midnight and 04:00.
    fn today(boundary_hour: u8) -> chrono::NaiveDate {
        crate::session::local_date_for_timestamp(chrono::Local::now(), boundary_hour)
    }

    /// The rule the helper above encodes, pinned on its own so a future
    /// reader does not have to infer it from a failing CI run.
    #[test]
    fn a_timestamp_before_the_boundary_belongs_to_the_previous_day() {
        use chrono::TimeZone as _;
        let at_two_am = chrono::Local
            .with_ymd_and_hms(2026, 9, 1, 2, 0, 0)
            .single()
            .expect("an unambiguous local time");
        assert_eq!(
            crate::session::local_date_for_timestamp(at_two_am, 4),
            chrono::NaiveDate::from_ymd_opt(2026, 8, 31).unwrap(),
            "02:00 under a 04:00 boundary is still the previous day"
        );

        let at_ten_am = chrono::Local
            .with_ymd_and_hms(2026, 9, 1, 10, 0, 0)
            .single()
            .expect("an unambiguous local time");
        assert_eq!(
            crate::session::local_date_for_timestamp(at_ten_am, 4),
            chrono::NaiveDate::from_ymd_opt(2026, 9, 1).unwrap(),
        );
    }

    fn store() -> (tempfile::TempDir, AcpSessionStore) {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path().join("cache");
        let cache = crate::tool_payload_cache::ToolPayloadCache::open(cache_dir).unwrap();
        let store = AcpSessionStore::new(dir.path().join("sessions"), Some(cache));
        (dir, store)
    }

    /// A store whose tool-result cache failed to open — the degraded
    /// shape `AcpSessionStore::new` must still function under.
    fn store_without_cache() -> (tempfile::TempDir, AcpSessionStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = AcpSessionStore::new(dir.path().join("sessions"), None);
        (dir, store)
    }

    #[test]
    fn a_header_round_trips() {
        let (_d, store) = store();
        store.create("s1", "default", "/home/u/project").unwrap();

        let summary = store.summary("s1").expect("the session exists");
        assert_eq!(summary.header.session_id, "s1");
        assert_eq!(summary.header.namespace, "default");
        assert_eq!(summary.header.cwd, "/home/u/project");
        assert!(!summary.is_closed);
    }

    /// The first event has no parent; every later one points at the
    /// event that was the tip when it was written. This chain is what
    /// makes an offline divergence detectable, so it is the property
    /// most worth pinning.
    #[test]
    fn the_parent_chain_records_append_order() {
        let (_d, store) = store();
        store.create("s1", "default", "/p").unwrap();
        store
            .append_message("s1", &ChatMessage::user("one"))
            .unwrap();
        store
            .append_message("s1", &ChatMessage::assistant("two"))
            .unwrap();
        store.append_title("s1", "a title").unwrap();

        let events = store.events("s1").expect("the session exists");
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].parent, None, "the first event has no parent");
        assert_eq!(events[1].parent, Some(events[0].id));
        assert_eq!(events[2].parent, Some(events[1].id));
    }

    /// Ids are UUIDv7 so a directory listing sorts chronologically once
    /// events become files. The chain, not the timestamp, is the
    /// authority on order — but the ids must still be v7.
    #[test]
    fn event_ids_are_uuid_v7() {
        let (_d, store) = store();
        store.create("s1", "default", "/p").unwrap();
        store.append_message("s1", &ChatMessage::user("x")).unwrap();

        let events = store.events("s1").unwrap();
        assert_eq!(events[0].id.get_version_num(), 7);
    }

    /// Closing appends an event rather than rewriting anything, so the
    /// log stays append-only.
    #[test]
    fn closing_appends_an_event() {
        let (_d, store) = store();
        store.create("s1", "default", "/p").unwrap();
        store.append_message("s1", &ChatMessage::user("x")).unwrap();
        store.close("s1").unwrap();

        assert!(store.summary("s1").unwrap().is_closed);
        let events = store.events("s1").unwrap();
        assert_eq!(events.len(), 2);
        assert!(matches!(events[1].body, EventBody::Closed));
    }

    /// A tool result is written to the cache and referenced by hash;
    /// the log never carries the content.
    #[test]
    fn a_tool_result_is_stored_by_reference() {
        let (dir, store) = store();
        store.create("s1", "default", "/p").unwrap();
        store
            .append_message(
                "s1",
                &ChatMessage {
                    role: Role::User,
                    parts: vec![ContentPart::ToolResult {
                        tool_use_id: "c1".to_string(),
                        content: "a very long file listing".to_string(),
                    }],
                    input_kind: None,
                    user_id: None,
                },
            )
            .unwrap();

        let raw = std::fs::read_to_string(store.path_for_test("s1")).unwrap();
        assert!(
            !raw.contains("a very long file listing"),
            "the content must live in the cache, not the log: {raw}"
        );
        assert!(raw.contains("tool_result_ref"), "got {raw}");
        drop(dir);
    }

    /// A cache that failed to open at startup must degrade the store,
    /// not break it: a session must still be creatable and a tool
    /// result still appendable — as a `ToolResultRef` with `sha256: None`,
    /// keeping the `tool_use_id` pairing intact — rather than erroring
    /// the whole turn.
    #[test]
    fn a_tool_result_appends_cleanly_when_the_cache_is_unavailable() {
        let (dir, store) = store_without_cache();
        store.create("s1", "default", "/p").unwrap();
        let result = store.append_message(
            "s1",
            &ChatMessage {
                role: Role::User,
                parts: vec![ContentPart::ToolResult {
                    tool_use_id: "c1".to_string(),
                    content: "a very long file listing".to_string(),
                }],
                input_kind: None,
                user_id: None,
            },
        );
        assert!(
            result.is_ok(),
            "appending must not fail just because the cache is unavailable: {result:?}"
        );
        drop(dir);
    }

    /// The regression this task exists for. A tool result stored with no
    /// cache must still read back as a `ToolResult` carrying its
    /// `tool_use_id` — an unpaired `tool_use` is rejected by the API,
    /// which is a broken session rather than a degraded one.
    ///
    /// A preceding `tool_use` message is required for the `tool_result`
    /// to survive `history()`'s positional repair (Fix 2) at all — a
    /// `tool_result` with no `tool_use` anywhere before it is exactly
    /// the invalid pairing that repair now drops, so this fixture must
    /// be a valid pair to exercise what the test is actually about: the
    /// hash-cache round trip.
    #[test]
    fn a_result_stored_without_a_cache_keeps_its_pairing() {
        let (_d, store) = store_without_cache();
        store.create("s1", "default", "/p").unwrap();
        store.append_message("s1", &tool_use_message("c1")).unwrap();
        store
            .append_message(
                "s1",
                &tool_result_message("c1", "content that cannot be cached"),
            )
            .unwrap();

        let history = store.history("s1").expect("the session loads");
        assert_eq!(
            history[1].parts[0],
            ContentPart::ToolResult {
                tool_use_id: "c1".to_string(),
                content: MISSING_RESULT.to_string(),
            },
            "the id survives even though the content never had anywhere to go"
        );
    }

    /// The two ways a result can be absent — never cached, or cached and
    /// later evicted — must read back identically. A reader has no
    /// reason to tell them apart: both mean "the pairing is here, the
    /// content is not".
    #[test]
    fn never_cached_and_evicted_read_back_the_same() {
        let (dir_a, cached) = store();
        cached.create("s1", "default", "/p").unwrap();
        cached
            .append_message("s1", &tool_use_message("c1"))
            .unwrap();
        cached
            .append_message("s1", &tool_result_message("c1", "gone later"))
            .unwrap();
        std::fs::remove_dir_all(dir_a.path().join("cache")).unwrap();
        let evicted = cached.history("s1").unwrap();

        let (_dir_b, uncached) = store_without_cache();
        uncached.create("s1", "default", "/p").unwrap();
        uncached
            .append_message("s1", &tool_use_message("c1"))
            .unwrap();
        uncached
            .append_message("s1", &tool_result_message("c1", "never stored"))
            .unwrap();
        let never = uncached.history("s1").unwrap();

        assert_eq!(evicted[1].parts, never[1].parts);
    }

    /// #212: a tool *input* is cached exactly the way a result is, at
    /// any size. A `file_write` call carries the file's contents, and
    /// the JSONL lives under `workspace_dir/sessions`, which the
    /// retrieve indexer walks line by line. It must not reach disk, and
    /// it must come back whole.
    #[test]
    fn a_tool_input_is_cached_rather_than_written_to_the_jsonl() {
        let (_d, store) = store();
        store.create("s1", "default", "/p").unwrap();

        let huge = "x".repeat(60_000);
        store
            .append_message(
                "s1",
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

        let raw = std::fs::read_to_string(store.path_for_test("s1")).unwrap();
        assert!(
            !raw.contains(&"x".repeat(60_000)),
            "the oversized input must not reach the (indexed) JSONL verbatim"
        );

        assert!(
            raw.contains("file_write"),
            "the tool name must stay inline so a summary can still name it"
        );

        let history = store.history("s1").expect("the session loads");
        match &history[0].parts[0] {
            ContentPart::ToolUse { id, name, input } => {
                assert_eq!(id, "c1");
                assert_eq!(name, "file_write");
                assert_eq!(
                    input,
                    &serde_json::json!({ "content": huge }),
                    "the input must round-trip byte-for-byte"
                );
            }
            other => panic!("expected a ToolUse, got {other:?}"),
        }
    }

    /// A file written before inputs were cached holds `{"tool_use": …}`
    /// with the arguments inline. It must still load its call: dropping
    /// the part would leave the `tool_result` answering it orphaned, and
    /// the repair pass would then delete that too — turning a readable
    /// old session into a conversation with a hole where the work was.
    #[test]
    fn a_legacy_inline_tool_use_still_loads() {
        let (_d, store) = store();
        store.create("s1", "default", "/p").unwrap();
        store
            .append_message("s1", &ChatMessage::user("do it"))
            .unwrap();

        let parent = store.events("s1").unwrap().last().unwrap().id;
        let legacy = serde_json::json!({
            "kind": "message",
            "id": Uuid::now_v7(),
            "parent": parent,
            "at": Utc::now(),
            "role": "assistant",
            "parts": [{"tool_use": {
                "id": "c1",
                "name": "file_read",
                "input": {"path": "a.rs"},
            }}],
        });
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(store.path_for_test("s1"))
            .unwrap();
        use std::io::Write as _;
        writeln!(f, "{legacy}").unwrap();
        drop(f);

        let history = store.history("s1").expect("the session loads");
        let call = history.iter().flat_map(|m| &m.parts).find_map(|p| match p {
            ContentPart::ToolUse { id, name, input } if id == "c1" => {
                Some((name.clone(), input.clone()))
            }
            _ => None,
        });
        assert_eq!(
            call,
            Some((
                "file_read".to_string(),
                serde_json::json!({ "path": "a.rs" })
            )),
            "the legacy call must load with its inline input: {history:?}"
        );
    }

    /// An ordinary input takes the same path — there is no size
    /// threshold any more, so the common case must round-trip too.
    #[test]
    fn an_ordinary_tool_input_round_trips() {
        let (_d, store) = store();
        store.create("s1", "default", "/p").unwrap();

        let input = serde_json::json!({ "path": "notes.md", "content": "hello" });
        store
            .append_message(
                "s1",
                &ChatMessage {
                    role: Role::Assistant,
                    parts: vec![ContentPart::ToolUse {
                        id: "c1".to_string(),
                        name: "file_write".to_string(),
                        input: input.clone(),
                    }],
                    input_kind: None,
                    user_id: None,
                },
            )
            .unwrap();

        let history = store.history("s1").expect("the session loads");
        match &history[0].parts[0] {
            ContentPart::ToolUse { input: got, .. } => {
                assert_eq!(got, &input, "an ordinary input must round-trip unchanged");
            }
            other => panic!("expected a ToolUse, got {other:?}"),
        }
    }

    /// A cache that is present still stores the hash, not the content.
    #[test]
    fn a_cached_result_still_records_its_hash() {
        let (_d, store) = store();
        store.create("s1", "default", "/p").unwrap();
        store.append_message("s1", &tool_use_message("c1")).unwrap();
        store
            .append_message("s1", &tool_result_message("c1", "the real output"))
            .unwrap();

        let raw = std::fs::read_to_string(store.path_for_test("s1")).unwrap();
        assert!(!raw.contains("the real output"), "got {raw}");
        assert!(raw.contains("tool_result_ref"), "got {raw}");
        assert_eq!(
            store.history("s1").unwrap()[1].parts[0],
            ContentPart::ToolResult {
                tool_use_id: "c1".to_string(),
                content: "the real output".to_string(),
            }
        );
    }

    /// The listing is per namespace. Another namespace's sessions are
    /// not this connection's to see — the boundary is enforced in the
    /// ACP handler too, but the store should not hand them over either.
    #[test]
    fn listing_is_scoped_to_one_namespace() {
        let (_d, store) = store();
        store.create("mine", "default", "/p").unwrap();
        store.create("theirs", "someone-else", "/p").unwrap();

        let ids: Vec<String> = store
            .list_summaries("default")
            .into_iter()
            .map(|s| s.header.session_id)
            .collect();
        assert_eq!(ids, vec!["mine".to_string()]);
    }

    /// The editor opens a thread on every panel open, so most sessions
    /// are created and never typed into. The listing says which ones
    /// carry a message, and the handler drops the rest — computed from
    /// the read the header already needs, so it costs nothing extra.
    #[test]
    fn a_summary_reports_whether_anything_was_said() {
        let (_d, store) = store();
        store.create("empty", "default", "/p").unwrap();
        store.create("used", "default", "/p").unwrap();
        store
            .append_message("used", &ChatMessage::user("hi"))
            .unwrap();
        store.append_title("used", "greetings").unwrap();

        let by_id: std::collections::HashMap<String, SessionSummary> = store
            .list_summaries("default")
            .into_iter()
            .map(|s| (s.header.session_id.clone(), s))
            .collect();

        assert!(!by_id["empty"].has_messages);
        assert_eq!(by_id["empty"].title, None);
        assert!(by_id["used"].has_messages);
        assert_eq!(by_id["used"].title.as_deref(), Some("greetings"));
    }

    /// Two threads appending to the *same session* on the *same store*
    /// must be ordered by the `tips` lock, not raced: the second writer
    /// must always see the first writer's freshly-written tip. Before
    /// the fix, `tip()` (read) and the write that followed it were two
    /// separate lock acquisitions, so both threads could observe the
    /// same tip and both write with that parent — a fabricated fork
    /// that means nothing, unlike the cross-process case this format
    /// tolerates on purpose.
    ///
    /// This is a thread-based test rather than a structural one: since
    /// the fix makes the whole "read tip, write, become tip" sequence
    /// one held lock, the two threads are fully serialized by
    /// construction and this test is deterministic post-fix (not
    /// flaky) — there is no longer a window where they could interleave
    /// mid-operation. It is the direct expression of the invariant:
    /// after both appends land, no two events may share a parent.
    #[test]
    fn concurrent_appends_to_one_session_do_not_fork() {
        let (_d, store) = store();
        store.create("s1", "default", "/p").unwrap();
        let store = std::sync::Arc::new(store);

        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let handles: Vec<_> = (0..2)
            .map(|i| {
                let store = std::sync::Arc::clone(&store);
                let barrier = std::sync::Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    store
                        .append_message("s1", &ChatMessage::user(format!("msg{i}")))
                        .unwrap();
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }

        let events = store.events("s1").expect("the session exists");
        assert_eq!(events.len(), 2, "both appends must land: {events:?}");
        let parents: std::collections::HashSet<Option<uuid::Uuid>> =
            events.iter().map(|e| e.parent).collect();
        assert_eq!(
            parents.len(),
            2,
            "each event must have a distinct parent — a shared parent means \
             a fabricated fork: {events:?}"
        );
        assert!(
            events.iter().any(|e| e.parent.is_none()),
            "exactly one event is the session's first: {events:?}"
        );
    }

    #[test]
    fn history_comes_back_in_chain_order() {
        let (_d, store) = store();
        store.create("s1", "default", "/p").unwrap();
        store
            .append_message("s1", &ChatMessage::user("one"))
            .unwrap();
        store
            .append_message("s1", &ChatMessage::assistant("two"))
            .unwrap();
        store.append_title("s1", "ignored by history").unwrap();
        store
            .append_message("s1", &ChatMessage::user("three"))
            .unwrap();

        let history = store.history("s1").expect("the session exists");
        let texts: Vec<&str> = history
            .iter()
            .filter_map(|m| match m.parts.first() {
                Some(ContentPart::Text(t)) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(texts, vec!["one", "two", "three"]);
    }

    /// `session/load` replays for the editor, which keeps no transcript
    /// of its own — that one stays whole. The LLM's copy does not.
    #[test]
    fn a_checkpoint_trims_the_model_history_but_not_the_editor_replay() {
        let (_d, store) = store();
        store.create("s1", "default", "/tmp").unwrap();
        for i in 0..5 {
            store
                .append_message("s1", &ChatMessage::user(&format!("m{i}")))
                .unwrap();
        }
        store.append_summary("s1", "the first three", 2).unwrap();

        let full = store.history("s1").expect("the editor replay");
        assert_eq!(full.len(), 5, "session/load must still see everything");

        let model = store.history_for_model("s1").expect("the model history");
        let texts: Vec<String> = model
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
        // `compaction_stub` contributes two messages (indices 0 and 1);
        // the checkpoint kept 2 trailing messages, so the tail must be
        // exactly m3, m4 — not m2..m4 (start = pos instead of pos + 1)
        // and not m4 alone.
        assert_eq!(
            &texts[2..],
            &["m3", "m4"],
            "the checkpoint boundary is off: {texts:?}"
        );
    }

    /// No summary yet: the two reads agree.
    #[test]
    fn without_a_checkpoint_the_model_sees_the_whole_session() {
        let (_d, store) = store();
        store.create("s1", "default", "/tmp").unwrap();
        store
            .append_message("s1", &ChatMessage::user("only"))
            .unwrap();
        assert_eq!(
            store.history_for_model("s1").unwrap(),
            store.history("s1").unwrap()
        );
    }

    /// A Summary event must never register as message activity in the
    /// daily log's shape. Structural — `EventBody::Summary` carries no
    /// `role`/`parts`, so it cannot literally decode into a logged
    /// message — but the projection is what the permanent record is
    /// built from, so it is asserted rather than assumed.
    #[test]
    fn a_summary_event_is_not_mistaken_for_recent_activity() {
        let (_d, store) = store();
        store.create("s1", "default", "/tmp").unwrap();
        store
            .append_message("s1", &ChatMessage::user("hello"))
            .unwrap();
        store.append_summary("s1", "a recap", 0).unwrap();

        let days = store.sessions_for_day(today(4), 4);
        assert_eq!(days.len(), 1);
        let (_, messages) = &days[0];
        assert_eq!(
            messages.len(),
            1,
            "the summary must not add a second entry to the daily log: {messages:?}"
        );
    }

    /// A tool result survives a round trip through the cache.
    #[test]
    fn a_cached_tool_result_is_restored_whole() {
        let (_d, store) = store();
        store.create("s1", "default", "/p").unwrap();
        store.append_message("s1", &tool_use_message("c1")).unwrap();
        store
            .append_message("s1", &tool_result_message("c1", "the original output"))
            .unwrap();

        let history = store.history("s1").unwrap();
        assert_eq!(
            history[1].parts[0],
            ContentPart::ToolResult {
                tool_use_id: "c1".to_string(),
                content: "the original output".to_string(),
            }
        );
    }

    /// The whole reason a resume summary is unnecessary: a lost result
    /// degrades to a sentence, and the `tool_use`/`tool_result` pairing
    /// the API validates stays intact. Losing the cache must never make
    /// a session unloadable.
    #[test]
    fn a_lost_tool_result_becomes_a_placeholder_rather_than_an_error() {
        let (dir, store) = store();
        store.create("s1", "default", "/p").unwrap();
        store.append_message("s1", &tool_use_message("c1")).unwrap();
        store
            .append_message("s1", &tool_result_message("c1", "gone tomorrow"))
            .unwrap();

        // Simulate the cache being cleared between runs.
        std::fs::remove_dir_all(dir.path().join("cache")).unwrap();

        let history = store.history("s1").expect("the session still loads");
        assert_eq!(
            history[1].parts[0],
            ContentPart::ToolResult {
                tool_use_id: "c1".to_string(),
                content: MISSING_RESULT.to_string(),
            },
            "the pairing must survive even though the content did not"
        );
    }

    /// Two events claiming the same parent means two writers appended to
    /// one session — the divergence the parent chain exists to catch.
    /// Until there is a merge story, take the first branch and say so
    /// rather than interleaving two conversations by timestamp.
    #[test]
    fn a_branch_is_reported_and_the_first_child_is_taken() {
        let (_d, store) = store();
        store.create("s1", "default", "/p").unwrap();
        store
            .append_message("s1", &ChatMessage::user("root"))
            .unwrap();

        // Hand-write a second child of the root to forge a divergence.
        let root = store.events("s1").unwrap()[0].id;
        let path = store.path_for_test("s1");
        let forged = serde_json::json!({
            "kind": "message",
            "id": Uuid::now_v7(),
            "parent": root,
            "at": Utc::now(),
            "role": "user",
            "parts": [{"text": "the other branch"}],
        });
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        use std::io::Write as _;
        writeln!(f, "{forged}").unwrap();
        drop(f);

        // The legitimate continuation, appended after the forgery, is
        // the file's *last* line but the root's *second* child.
        store
            .append_message("s1", &ChatMessage::user("mine"))
            .unwrap();

        let history = store.history("s1").expect("a branched session still loads");
        let texts: Vec<&str> = history
            .iter()
            .filter_map(|m| match m.parts.first() {
                Some(ContentPart::Text(t)) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            texts,
            vec!["root", "the other branch"],
            "one branch, not both interleaved"
        );
    }

    /// An event whose parent is not in the file — a truncated sync, a
    /// hand-edit — must not silently drop the rest of the conversation
    /// or spin. Everything reachable from the root is returned.
    #[test]
    fn an_orphan_event_is_skipped_rather_than_ending_the_walk() {
        let (_d, store) = store();
        store.create("s1", "default", "/p").unwrap();
        store
            .append_message("s1", &ChatMessage::user("root"))
            .unwrap();

        let path = store.path_for_test("s1");
        let orphan = serde_json::json!({
            "kind": "message",
            "id": Uuid::now_v7(),
            "parent": Uuid::now_v7(),
            "at": Utc::now(),
            "role": "user",
            "parts": [{"text": "unreachable"}],
        });
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        use std::io::Write as _;
        writeln!(f, "{orphan}").unwrap();
        drop(f);

        let history = store.history("s1").expect("the session still loads");
        assert_eq!(
            history.len(),
            1,
            "the orphan is not reachable from the root"
        );
    }

    /// The read-side fix for the write side's warn-and-continue: an
    /// `append_message` for a `tool_result` can fail after its
    /// `tool_use` sibling already landed, or the process can die between
    /// the two. Either way, a `tool_use` with nothing answering it must
    /// not brick the session — `history()` closes the gap itself rather
    /// than handing the API an unpaired call on reload.
    #[test]
    fn an_unanswered_tool_use_gets_a_synthesised_result() {
        let (_d, store) = store();
        store.create("s1", "default", "/p").unwrap();
        store
            .append_message("s1", &ChatMessage::user("run it"))
            .unwrap();

        // Hand-write a `tool_use` with no matching `tool_result` after
        // it — standing in for a crash or a failed cache write between
        // the two appends `run_llm_turn` makes.
        let parent = store.events("s1").unwrap()[0].id;
        let path = store.path_for_test("s1");
        let orphan_use = serde_json::json!({
            "kind": "message",
            "id": Uuid::now_v7(),
            "parent": parent,
            "at": Utc::now(),
            "role": "assistant",
            "parts": [{"tool_use": {"id": "call-1", "name": "risky", "input": {}}}],
        });
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        use std::io::Write as _;
        writeln!(f, "{orphan_use}").unwrap();
        drop(f);

        let history = store.history("s1").expect("the session still loads");
        let last = history.last().expect("a synthesised message was appended");
        assert_eq!(
            last.parts,
            vec![ContentPart::ToolResult {
                tool_use_id: "call-1".to_string(),
                content: MISSING_RESULT.to_string(),
            }],
            "the unanswered tool_use gets a placeholder result so the \
             pairing holds on reload"
        );
    }

    /// The case that exposed the trailing-message version as wrong: the
    /// API requires a `tool_result` to sit in the message *immediately
    /// following* its `tool_use`, not merely somewhere later in the
    /// transcript. If a real message chains onto a not-yet-repaired
    /// orphan — a later turn continues the conversation without ever
    /// writing the read-side repair back to disk — a trailing repair
    /// would land after that unrelated message instead of between the
    /// two, leaving the session unloadable all over again on the next
    /// fresh read.
    #[test]
    fn a_repair_lands_right_after_its_orphan_even_when_later_messages_follow() {
        let (_d, store) = store();
        store.create("s1", "default", "/p").unwrap();

        // Hand-write the `tool_use` as the session's very first event.
        // No `append_message` call has happened yet, so the store's
        // `tips` cache holds nothing for this session — the next append
        // falls back to reading the file for the tip, exactly as a
        // real writer resuming after a crash would, and correctly
        // chains onto this hand-written event.
        let path = store.path_for_test("s1");
        let orphan_id = Uuid::now_v7();
        let orphan_use = serde_json::json!({
            "kind": "message",
            "id": orphan_id,
            "parent": null,
            "at": Utc::now(),
            "role": "assistant",
            "parts": [{"tool_use": {"id": "call-1", "name": "risky", "input": {}}}],
        });
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        use std::io::Write as _;
        writeln!(f, "{orphan_use}").unwrap();
        drop(f);

        // A later turn carries the conversation on, on disk, without
        // the orphan ever having been repaired in place.
        store
            .append_message("s1", &ChatMessage::user("continue"))
            .unwrap();
        store
            .append_message("s1", &ChatMessage::assistant("ok"))
            .unwrap();

        let history = store.history("s1").expect("the session still loads");
        assert_eq!(
            history.len(),
            4,
            "orphan, repair, then the two later messages: {history:?}"
        );
        assert!(
            matches!(&history[0].parts[..], [ContentPart::ToolUse { id, .. }] if id == "call-1"),
            "message 0 is the orphaned tool_use: {history:?}"
        );
        assert_eq!(
            history[1].parts,
            vec![ContentPart::ToolResult {
                tool_use_id: "call-1".to_string(),
                content: MISSING_RESULT.to_string(),
            }],
            "the repair must be message 1, immediately after the orphan \
             and before the later real messages: {history:?}"
        );
        assert!(
            matches!(&history[2].parts[..], [ContentPart::Text(t)] if t == "continue"),
            "the later real message must follow the repair, not precede it: {history:?}"
        );
        assert!(
            matches!(&history[3].parts[..], [ContentPart::Text(t)] if t == "ok"),
            "got {history:?}"
        );
    }

    /// Two separate orphaned `tool_use`s, far apart in the conversation,
    /// each get their own repair spliced in right after themselves —
    /// not merged into one message anywhere.
    #[test]
    fn two_far_apart_orphans_each_get_their_own_repair_in_position() {
        let (_d, store) = store();
        store.create("s1", "default", "/p").unwrap();
        let path = store.path_for_test("s1");

        let write_line = |parent: Option<Uuid>, id: Uuid, role: &str, parts: serde_json::Value| {
            let line = serde_json::json!({
                "kind": "message",
                "id": id,
                "parent": parent,
                "at": Utc::now(),
                "role": role,
                "parts": parts,
            });
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            use std::io::Write as _;
            writeln!(f, "{line}").unwrap();
        };

        let e1 = Uuid::now_v7();
        write_line(None, e1, "user", serde_json::json!([{"text": "start"}]));
        let e2 = Uuid::now_v7();
        write_line(
            Some(e1),
            e2,
            "assistant",
            serde_json::json!([{"tool_use": {"id": "call-a", "name": "risky", "input": {}}}]),
        );
        let e3 = Uuid::now_v7();
        write_line(
            Some(e2),
            e3,
            "user",
            serde_json::json!([{"text": "middle"}]),
        );
        let e4 = Uuid::now_v7();
        write_line(
            Some(e3),
            e4,
            "assistant",
            serde_json::json!([{"tool_use": {"id": "call-b", "name": "risky", "input": {}}}]),
        );
        write_line(
            Some(e4),
            Uuid::now_v7(),
            "user",
            serde_json::json!([{"text": "end"}]),
        );

        let history = store.history("s1").expect("the session still loads");
        let shapes: Vec<String> = history
            .iter()
            .map(|m| match &m.parts[..] {
                [ContentPart::Text(t)] => format!("text:{t}"),
                [ContentPart::ToolUse { id, .. }] => format!("tool_use:{id}"),
                [
                    ContentPart::ToolResult {
                        tool_use_id,
                        content,
                    },
                ] => format!(
                    "tool_result:{tool_use_id}:{}",
                    if content == MISSING_RESULT {
                        "missing"
                    } else {
                        "other"
                    }
                ),
                other => format!("unexpected:{other:?}"),
            })
            .collect();
        assert_eq!(
            shapes,
            vec![
                "text:start".to_string(),
                "tool_use:call-a".to_string(),
                "tool_result:call-a:missing".to_string(),
                "text:middle".to_string(),
                "tool_use:call-b".to_string(),
                "tool_result:call-b:missing".to_string(),
                "text:end".to_string(),
            ],
            "each orphan's repair must sit right after its own tool_use, \
             not gathered together or moved to the end: {shapes:?}"
        );
    }

    /// The fix above must only fire on a genuine gap — a session whose
    /// `tool_use`/`tool_result` pair is already complete gains nothing.
    #[test]
    fn a_complete_pair_gains_no_synthesised_result() {
        let (_d, store) = store();
        store.create("s1", "default", "/p").unwrap();
        store
            .append_message(
                "s1",
                &ChatMessage::assistant_with_tools(
                    None,
                    vec![crate::provider::ToolCall {
                        id: "call-1".to_string(),
                        name: "risky".to_string(),
                        input: serde_json::json!({}),
                    }],
                ),
            )
            .unwrap();
        store
            .append_message("s1", &tool_result_message("call-1", "the real output"))
            .unwrap();

        let history = store.history("s1").expect("the session loads");
        let result_count = history
            .iter()
            .flat_map(|m| &m.parts)
            .filter(|p| matches!(p, ContentPart::ToolResult { .. }))
            .count();
        assert_eq!(
            result_count, 1,
            "no extra result should be synthesised when the pairing is already complete"
        );
    }

    /// Fix 2, failure shape 1: two separate `tool_use`s reuse the same
    /// id "c1" — the first is genuinely answered by the message right
    /// after it, the second is not. A set-based "has `c1` been answered
    /// anywhere" check would see the first answer and wrongly suppress
    /// the second `tool_use`'s repair. The positional check must not
    /// make that mistake: each `tool_use` is judged only by its own
    /// immediate neighbour.
    #[test]
    fn two_tool_uses_sharing_an_id_are_checked_independently_by_position() {
        let (_d, store) = store();
        store.create("s1", "default", "/p").unwrap();
        store
            .append_message("s1", &ChatMessage::user("start"))
            .unwrap();
        store.append_message("s1", &tool_use_message("c1")).unwrap();
        store
            .append_message("s1", &tool_result_message("c1", "first answer"))
            .unwrap();
        store
            .append_message("s1", &ChatMessage::user("middle"))
            .unwrap();
        // A second, unrelated tool_use that happens to reuse id "c1" —
        // and this one is never answered.
        store.append_message("s1", &tool_use_message("c1")).unwrap();
        store
            .append_message("s1", &ChatMessage::user("end"))
            .unwrap();

        let history = store.history("s1").expect("the session loads");
        let results: Vec<&ContentPart> = history
            .iter()
            .flat_map(|m| &m.parts)
            .filter(|p| matches!(p, ContentPart::ToolResult { .. }))
            .collect();
        assert_eq!(
            results,
            vec![
                &ContentPart::ToolResult {
                    tool_use_id: "c1".to_string(),
                    content: "first answer".to_string(),
                },
                &ContentPart::ToolResult {
                    tool_use_id: "c1".to_string(),
                    content: MISSING_RESULT.to_string(),
                },
            ],
            "the first call-1 stays answered by its real result; the second \
             must still get its own repair even though the id was already \
             seen: {results:?}"
        );
    }

    /// Fix 2, failure shape 2: a `tool_result` sits many messages before
    /// the `tool_use` it claims to answer — reachable through a fork the
    /// walk resolves differently, or a partial sync that landed events
    /// out of their real order. The pairing is positionally invalid
    /// (nothing in the immediately preceding message has that id), so
    /// the stray result must be dropped, and the real, unanswered
    /// `tool_use` still gets its own repair.
    #[test]
    fn a_result_answering_a_non_adjacent_tool_use_is_dropped() {
        let (_d, store) = store();
        store.create("s1", "default", "/p").unwrap();
        let path = store.path_for_test("s1");

        let write_line = |parent: Option<Uuid>, id: Uuid, role: &str, parts: serde_json::Value| {
            let line = serde_json::json!({
                "kind": "message",
                "id": id,
                "parent": parent,
                "at": Utc::now(),
                "role": role,
                "parts": parts,
            });
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            use std::io::Write as _;
            writeln!(f, "{line}").unwrap();
        };

        // e1: an ordinary root message with no tool_use at all.
        let e1 = Uuid::now_v7();
        write_line(None, e1, "user", serde_json::json!([{"text": "root"}]));
        // e2: a tool_result for "orphan" — positionally invalid, since
        // e1 carries no matching tool_use.
        let e2 = Uuid::now_v7();
        write_line(
            Some(e1),
            e2,
            "user",
            serde_json::json!([{"tool_result_ref": {"tool_use_id": "orphan", "sha256": null}}]),
        );
        // e3: unrelated text.
        let e3 = Uuid::now_v7();
        write_line(Some(e2), e3, "user", serde_json::json!([{"text": "later"}]));
        // e4: the real tool_use, never answered.
        let e4 = Uuid::now_v7();
        write_line(
            Some(e3),
            e4,
            "assistant",
            serde_json::json!([{"tool_use": {"id": "orphan", "name": "risky", "input": {}}}]),
        );

        let history = store.history("s1").expect("the session still loads");
        let shapes: Vec<String> = history
            .iter()
            .map(|m| match &m.parts[..] {
                [ContentPart::Text(t)] => format!("text:{t}"),
                [ContentPart::ToolUse { id, .. }] => format!("tool_use:{id}"),
                [
                    ContentPart::ToolResult {
                        tool_use_id,
                        content,
                    },
                ] => format!(
                    "tool_result:{tool_use_id}:{}",
                    if content == MISSING_RESULT {
                        "missing"
                    } else {
                        "other"
                    }
                ),
                other => format!("unexpected:{other:?}"),
            })
            .collect();
        assert_eq!(
            shapes,
            vec![
                "text:root".to_string(),
                // e2's tool_result_ref is gone entirely: it was the only
                // part on that message, so the whole message is dropped.
                "text:later".to_string(),
                "tool_use:orphan".to_string(),
                "tool_result:orphan:missing".to_string(),
            ],
            "the non-adjacent result must be dropped, and the real \
             orphan must still get its own repair: {shapes:?}"
        );
    }

    /// Fix 2, failure shape 3: two `ToolUse` parts in the same message
    /// share an id. If neither is answered, the repair must carry that
    /// id exactly once — not two `ToolResult`s with the same
    /// `tool_use_id`, which would itself be a malformed message.
    #[test]
    fn duplicate_tool_use_ids_in_one_message_get_one_repair_each() {
        let (_d, store) = store();
        store.create("s1", "default", "/p").unwrap();
        store
            .append_message(
                "s1",
                &ChatMessage::assistant_with_tools(
                    None,
                    vec![
                        crate::provider::ToolCall {
                            id: "dup".to_string(),
                            name: "risky".to_string(),
                            input: serde_json::json!({}),
                        },
                        crate::provider::ToolCall {
                            id: "dup".to_string(),
                            name: "risky".to_string(),
                            input: serde_json::json!({}),
                        },
                    ],
                ),
            )
            .unwrap();
        store
            .append_message("s1", &ChatMessage::user("no answer follows"))
            .unwrap();

        let history = store.history("s1").expect("the session loads");
        // [tool_use(dup, dup), repair(dup), "no answer follows"] — the
        // repair is spliced immediately after the tool_use message, not
        // gathered at the end.
        let repair = &history[1];
        assert_eq!(
            repair.parts,
            vec![ContentPart::ToolResult {
                tool_use_id: "dup".to_string(),
                content: MISSING_RESULT.to_string(),
            }],
            "one tool_use id must produce exactly one repaired result, even \
             though it appeared twice: {repair:?}"
        );
    }

    fn tool_result_message(tool_use_id: &str, content: &str) -> ChatMessage {
        ChatMessage {
            role: Role::User,
            parts: vec![ContentPart::ToolResult {
                tool_use_id: tool_use_id.to_string(),
                content: content.to_string(),
            }],
            input_kind: None,
            user_id: None,
        }
    }

    /// A minimal assistant message carrying one `tool_use`, for tests
    /// that need `history()`'s positional repair (Fix 2) to see a valid
    /// pair rather than dropping a `tool_result` with nothing before it.
    fn tool_use_message(id: &str) -> ChatMessage {
        ChatMessage::assistant_with_tools(
            None,
            vec![crate::provider::ToolCall {
                id: id.to_string(),
                name: "risky".to_string(),
                input: serde_json::json!({}),
            }],
        )
    }

    /// The daily log is the durable record — the one that stays in the
    /// workspace and stays searchable. An ACP session has to reach it.
    #[test]
    fn a_days_conversation_projects_into_the_daily_log_shape() {
        let (_d, store) = store();
        store.create("s1", "work", "/p").unwrap();
        store
            .append_message("s1", &ChatMessage::user("what broke?"))
            .unwrap();
        store
            .append_message("s1", &ChatMessage::assistant("the parser"))
            .unwrap();
        store.append_title("s1", "parser hunt").unwrap();

        let today = today(4);
        let sessions = store.sessions_for_day(today, 4);
        assert_eq!(sessions.len(), 1);
        let (meta, messages) = &sessions[0];
        assert_eq!(meta.session_id, "s1");
        assert_eq!(meta.channel, "acp");
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, Role::User);
        assert_eq!(messages[1].role, Role::Assistant);
        assert_eq!(
            messages[0].parts[0],
            ContentPart::Text("what broke?".to_string())
        );
    }

    /// A tool result that has fallen out of the cache must not put a
    /// placeholder sentence into the permanent record. The daily log
    /// wants what was said, and `format_sessions` keeps only text parts —
    /// so tool traffic should not be projected at all.
    #[test]
    fn tool_traffic_is_left_out_of_the_daily_log_projection() {
        let (_d, store) = store();
        store.create("s1", "default", "/p").unwrap();
        store
            .append_message("s1", &tool_result_message("c1", "a big file listing"))
            .unwrap();
        store
            .append_message("s1", &ChatMessage::user("thanks"))
            .unwrap();

        let today = today(4);
        let (_, messages) = store.sessions_for_day(today, 4).remove(0);
        let texts: Vec<&str> = messages
            .iter()
            .flat_map(|m| m.parts.iter())
            .filter_map(|p| match p {
                ContentPart::Text(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(texts, vec!["thanks"]);
    }

    #[test]
    fn session_dates_lists_the_days_that_have_messages() {
        let (_d, store) = store();
        store.create("s1", "default", "/p").unwrap();
        store.append_message("s1", &ChatMessage::user("x")).unwrap();

        let today = today(4);
        assert_eq!(store.session_dates(4, |_meta| true), vec![today]);
    }

    /// A date that only has messages in another namespace's ACP session
    /// must not be reported pending for this namespace — otherwise every
    /// namespace's catch-up sees every other namespace's dates as
    /// pending forever (`generate_daily_log` finds nothing in-namespace,
    /// writes nothing, and the phantom date is re-walked every tick).
    #[test]
    fn session_dates_excludes_a_date_that_belongs_only_to_another_namespace() {
        let (_d, store) = store();
        store.create("s-default", "default", "/p").unwrap();
        store
            .append_message("s-default", &ChatMessage::user("x"))
            .unwrap();
        store.create("s-work", "work", "/p").unwrap();
        store
            .append_message("s-work", &ChatMessage::user("y"))
            .unwrap();

        let today = today(4);
        let default_dates =
            store.session_dates(4, |meta| meta.namespace.as_deref() == Some("default"));
        assert_eq!(default_dates, vec![today]);

        let work_dates = store.session_dates(4, |meta| meta.namespace.as_deref() == Some("work"));
        assert_eq!(work_dates, vec![today]);

        let ghost_dates = store.session_dates(4, |meta| meta.namespace.as_deref() == Some("ghost"));
        assert!(ghost_dates.is_empty());
    }
}
