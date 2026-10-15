use crate::channel::{Attachment, Channels, OutgoingMessage};
use crate::config::{Config, SessionPolicy};
use crate::context_compression::{
    estimate_request_tokens, generate_summary, maybe_compress, record_prompt_usage,
};
use crate::image_cache::{ImageCache, hydrate_history, scrub_history_inplace};
use crate::provider::registry::ProviderRegistry;
use crate::provider::{ChatMessage, ContentPart, Provider, ToolCall};
use crate::session::{ConversationKey, SessionStore, local_date_for_timestamp};
use crate::tools::ToolSet;
use crate::workspace::Workspace;
use chrono::{Local, NaiveDate};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{Mutex, mpsc};
use tracing::{debug, error, info, warn};

impl Agent {
    /// Resolve the provider that should serve `room_id`, honouring the
    /// room → profile → provider mapping defined in `Config`. Falls back to
    /// the Anthropic provider when no profile applies.
    fn provider_for(&self, room_id: &str) -> Arc<dyn Provider> {
        self.providers.for_room(&self.config, room_id)
    }
}

// ---------------------------------------------------------------------------
// System prompt snapshot (frozen per ConversationKey, refreshed daily)
// ---------------------------------------------------------------------------

struct SystemSnapshot {
    system_prompt: String,
    date: NaiveDate,
}

// ---------------------------------------------------------------------------
// Agent
// ---------------------------------------------------------------------------

pub struct Agent {
    config: Config,
    channels: Arc<Channels>,
    providers: Arc<ProviderRegistry>,
    workspace: Arc<Workspace>,
    tools: Option<Arc<ToolSet>>,
    session_store: Arc<SessionStore>,
    /// Workspace-external image cache. `None` disables in-memory image
    /// scrubbing entirely (PR1 hash-marker on disk still applies).
    image_cache: Option<Arc<ImageCache>>,
    /// In-memory conversation history, keyed by (room_id, thread_id).
    /// Seeded at startup from the session log — including tool traffic,
    /// which is why there is no restart summary any more (#194).
    history: Mutex<HashMap<ConversationKey, Vec<ChatMessage>>>,
    /// Maps each ConversationKey to its current active session file (ULID string).
    active_sessions: Mutex<HashMap<ConversationKey, String>>,
    /// Per-ConversationKey system prompt snapshot, refreshed when the local date changes.
    snapshots: Mutex<HashMap<ConversationKey, SystemSnapshot>>,
    /// Background prefetch cache: workspace search results for the next turn.
    prefetch_cache: Mutex<HashMap<ConversationKey, String>>,
    /// Local date on which the last day-boundary action fired for each key.
    /// Prevents re-firing within the same day for policies that don't rotate
    /// the session file (Compact, None).
    boundary_handled: Mutex<HashMap<ConversationKey, NaiveDate>>,
}

impl Agent {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config: Config,
        channels: Arc<Channels>,
        providers: Arc<ProviderRegistry>,
        workspace: Arc<Workspace>,
        tools: Option<Arc<ToolSet>>,
        session_store: Arc<SessionStore>,
        image_cache: Option<Arc<ImageCache>>,
    ) -> Self {
        let (active_sessions, mut histories) = session_store.load_all();

        // Restrict *histories only* to rooms this instance is configured
        // to serve. `active_sessions` stays complete — it's cheap, and
        // `get_or_create_session` reads it regardless of room.
        //
        // This isn't a correctness guard: `ConversationKey` is
        // `(room_id, thread_id)`, so a foreign room's history could never
        // be served to a message this instance handles. It's here to cap
        // startup I/O and retained memory, which otherwise scale with an
        // unbounded and never-cleaned count of stale session files (e.g.
        // test rooms sharing the same sessions directory as production).
        // Don't "simplify" this away as pointless — it's a resource guard,
        // not a safety one.
        //
        // If any configured channel uses the "listen everywhere" wildcard
        // (empty room_ids / channel_ids), the whole agent is unrestricted
        // — narrowing to only the other channel's explicit set would
        // silently drop everything the wildcard channel was supposed to
        // handle.
        let mut wildcard = false;
        let allowed_rooms: std::collections::HashSet<String> = {
            let mut set = std::collections::HashSet::new();
            if let Some(m) = &config.matrix {
                if m.room_ids.is_empty() {
                    wildcard = true;
                } else {
                    set.extend(m.room_ids.iter().cloned());
                }
            }
            if let Some(d) = &config.discord {
                if d.channel_ids.is_empty() {
                    wildcard = true;
                } else {
                    set.extend(d.channel_ids.iter().cloned());
                }
            }
            set
        };
        if !wildcard && !allowed_rooms.is_empty() {
            let before = histories.len();
            histories.retain(|key, _| allowed_rooms.contains(&key.0));
            let skipped = before - histories.len();
            if skipped > 0 {
                info!(
                    "Startup: skipping {} restored session history entries in unconfigured room(s)",
                    skipped
                );
            }
        }

        info!(
            "Restored {} active session(s) from disk ({} with history)",
            active_sessions.len(),
            histories.len(),
        );
        Self {
            config,
            channels,
            providers,
            workspace,
            tools,
            session_store,
            image_cache,
            history: Mutex::new(histories),
            active_sessions: Mutex::new(active_sessions),
            snapshots: Mutex::new(HashMap::new()),
            prefetch_cache: Mutex::new(HashMap::new()),
            boundary_handled: Mutex::new(HashMap::new()),
        }
    }

    /// Drop all cached system-prompt snapshots AND the workspace's pinned file
    /// cache, so the next build re-reads every file from disk. Called after a
    /// daily/weekly/monthly/yearly log is regenerated (so the fresh log shows
    /// up) and by the `refresh_system_prompt` tool. Pinned reads mean the
    /// prompt is otherwise byte-stable; this is one of the two moments it
    /// legitimately changes (the other being the day-boundary snapshot rebuild,
    /// which re-reads only after a clear).
    pub async fn invalidate_system_prompts(&self) {
        self.workspace.clear_pinned_cache().await;
        self.snapshots.lock().await.clear();
    }

    /// Heartbeat trigger: inject a system-style prompt as if it were an
    /// incoming message, so the agent runs through the normal handle_message
    /// pipeline (history, tool loop, channel send) without faking a user
    /// utterance. The content is wrapped with `[Heartbeat: <name>]` so the
    /// agent can recognise (via AGENTS.md / HEARTBEAT.md instructions) that
    /// this is a system trigger, not a real user message.
    pub async fn trigger(
        self: &Arc<Self>,
        task_name: &str,
        prompt: &str,
        room_id: &str,
    ) -> anyhow::Result<()> {
        let content = format!("[Heartbeat: {task_name}]\n\n{prompt}");
        let now_ms = chrono::Utc::now().timestamp_millis().max(0) as u64;
        let incoming = crate::channel::IncomingMessage {
            id: format!("heartbeat-{task_name}-{now_ms}"),
            sender: "heartbeat".to_string(),
            content,
            room_id: room_id.to_string(),
            timestamp: now_ms,
            thread_id: None,
            attachments: Vec::new(),
        };
        Arc::clone(self).handle_message(incoming).await
    }

    /// Handle channel messages until `shutdown` is cancelled.
    pub async fn run(
        self: Arc<Self>,
        shutdown: tokio_util::sync::CancellationToken,
    ) -> anyhow::Result<()> {
        let (tx, mut rx) = mpsc::channel(64);

        let channels = Arc::clone(&self.channels);
        let listen_handle = tokio::spawn(async move {
            if let Err(e) = channels.listen_all(tx).await {
                error!("Channel listen error: {e:#}");
            }
        });

        info!("Agent is running. Press Ctrl-C to stop.");

        loop {
            tokio::select! {
                msg = rx.recv() => {
                    match msg {
                        Some(incoming) => {
                            let agent = Arc::clone(&self);
                            tokio::spawn(async move {
                                if let Err(e) = agent.handle_message(incoming).await {
                                    error!("Error handling message: {e:#}");
                                }
                            });
                        }
                        None => {
                            warn!("Message channel closed");
                            break;
                        }
                    }
                }
                () = shutdown.cancelled() => {
                    info!("Shutting down...");
                    break;
                }
            }
        }

        listen_handle.abort();
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Session helpers
    // -----------------------------------------------------------------------

    /// Return the active session_id for `key`, creating a new session file if needed.
    async fn get_or_create_session(&self, key: &ConversationKey) -> String {
        let mut sessions = self.active_sessions.lock().await;
        if let Some(id) = sessions.get(key) {
            return id.clone();
        }
        let channel_name = self
            .channels
            .channel_name_for_room(&key.0)
            .await
            .unwrap_or_else(|| {
                self.channels
                    .names()
                    .first()
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| "unknown".to_string())
            });
        let namespace = self.config.namespace_for_room(&key.0).to_string();
        match self
            .session_store
            .create_session(key, &channel_name, &namespace)
        {
            Ok(id) => {
                sessions.insert(key.clone(), id.clone());
                id
            }
            Err(e) => {
                warn!("Failed to create session file: {e}");
                String::new()
            }
        }
    }

    /// Persist `msg` to the session store. Returns whether it landed, so
    /// a caller holding the other half of a `tool_use` / `tool_result`
    /// pair can decline to write it alone.
    ///
    /// Tool parts used to be stripped here — raw history was never
    /// reloaded, so persisting them would only have bloated the JSONL.
    /// It will be reloaded from these files (#194, Phase 4), and a
    /// history that remembers what was said but not what was done is
    /// the gap this exists to close.
    ///
    /// The storage shape is decided inside [`SessionStore::append`]:
    /// both halves of a tool call go to the workspace-external cache,
    /// images become hash markers. Every persist path (agent, `/rpc`,
    /// `/a2a`, MCP) inherits it.
    ///
    /// A no-op returns `true`: there is no pairing to break when nothing
    /// was ever a candidate for the store.
    fn persist(&self, session_id: &str, msg: &ChatMessage) -> bool {
        if session_id.is_empty() {
            return true;
        }
        let has_content = msg.parts.iter().any(|p| match p {
            ContentPart::Text(t) => !t.is_empty(),
            _ => true,
        });
        if !has_content {
            return true;
        }
        match self.session_store.append(session_id, msg) {
            Ok(()) => true,
            Err(e) => {
                warn!("Failed to persist message: {e}");
                false
            }
        }
    }

    /// Dispatch the configured day-boundary action for `key` when the local
    /// date on the active session file is older than today. Each policy is
    /// idempotent within a day via `boundary_handled`.
    async fn maybe_handle_day_boundary(&self, key: &ConversationKey) {
        let boundary = self.config.day_boundary_hour;
        let today = local_date_for_timestamp(Local::now(), boundary);
        let policy = self.config.session_policy_for(&key.0);

        if policy == SessionPolicy::None {
            return;
        }

        let session_id = {
            let sessions = self.active_sessions.lock().await;
            match sessions.get(key) {
                Some(id) if !id.is_empty() => id.clone(),
                _ => return,
            }
        };

        let Some(created_at) = self.session_store.created_at(&session_id) else {
            return;
        };
        if local_date_for_timestamp(created_at.with_timezone(&Local), boundary) >= today {
            return;
        }

        // Idempotence: don't re-fire within the same local day if we already
        // handled it (only relevant for policies that don't rotate the file).
        {
            let handled = self.boundary_handled.lock().await;
            if handled.get(key).copied() == Some(today) {
                return;
            }
        }

        match policy {
            SessionPolicy::Reset => {
                info!("Day boundary crossed for {key:?}; resetting session");

                if let Err(e) = self.session_store.close_session(&session_id) {
                    warn!("Failed to close session {session_id}: {e}");
                }

                self.history.lock().await.remove(key);
                self.active_sessions.lock().await.remove(key);
                self.snapshots.lock().await.remove(key);
                self.prefetch_cache.lock().await.remove(key);
                // Reset rotates the session file, so no need to mark handled;
                // the new session will have today's created_at.
            }
            SessionPolicy::Compact => {
                self.compact_at_boundary(key, &session_id).await;
                self.boundary_handled
                    .lock()
                    .await
                    .insert(key.clone(), today);
            }
            SessionPolicy::None => unreachable!(),
        }
    }

    /// Force-summarize the current in-memory history for `key` and replace
    /// it with a summary stub so the session can keep growing into the new
    /// day without re-sending stale context to the model.
    async fn compact_at_boundary(&self, key: &ConversationKey, session_id: &str) {
        let messages = {
            let history = self.history.lock().await;
            history.get(key).cloned().unwrap_or_default()
        };

        // Nothing to compact if history is empty or already just a stub.
        let has_real_content = messages.iter().any(|m| {
            m.parts
                .iter()
                .any(|p| matches!(p, ContentPart::Text(t) if !t.is_empty()))
        });
        if messages.len() < 2 || !has_real_content {
            return;
        }

        info!("Day boundary crossed for {key:?}; compacting session {session_id}");

        let provider = self.provider_for(&key.0);
        let summary = match generate_summary(&*provider, &messages).await {
            Ok(s) if !s.trim().is_empty() => s,
            Ok(_) => {
                warn!("Boundary summary for {session_id} was empty; skipping");
                return;
            }
            Err(e) => {
                warn!("Boundary summary generation failed for {session_id}: {e:#}");
                return;
            }
        };

        if let Err(e) = self.session_store.append_summary(session_id, &summary, 0) {
            warn!("Failed to persist boundary summary for {session_id}: {e}");
        }

        let stub = crate::context_compression::compaction_stub(&summary);

        self.history.lock().await.insert(key.clone(), stub);
        // Refresh the system prompt snapshot so it rebuilds with today's date.
        self.snapshots.lock().await.remove(key);
        self.prefetch_cache.lock().await.remove(key);
    }

    // -----------------------------------------------------------------------
    // System prompt snapshot
    // -----------------------------------------------------------------------

    /// Return the system prompt for `key`, rebuilding if the local date has changed.
    async fn get_system_prompt(&self, key: &ConversationKey) -> Option<String> {
        let boundary = self.config.day_boundary_hour;
        let today = local_date_for_timestamp(Local::now(), boundary);

        {
            let snapshots = self.snapshots.lock().await;
            if let Some(snap) = snapshots.get(key)
                && snap.date == today
            {
                return if snap.system_prompt.is_empty() {
                    None
                } else {
                    Some(snap.system_prompt.clone())
                };
            }
        }

        let namespace = self.config.namespace_for_room(&key.0);
        let chain = self.config.resolve_namespace_chain(namespace);
        // Best-effort: a missing or unreachable RoomInfo just leaves the
        // "# Current Room" block out — never blocks the turn.
        let room_info = self.channels.room_info(&key.0).await;
        let system_prompt = self
            .workspace
            .build_system_prompt(
                self.config.anthropic.system_prompt.as_deref(),
                self.config.day_boundary_hour,
                &chain,
                room_info.as_ref(),
                // Matrix/Discord rooms have no client working directory —
                // the block's absence there is the whole point of `None`.
                None,
            )
            .await;

        info!(
            "Rebuilt system-prompt snapshot for {key:?} (date={today}, {} chars)",
            system_prompt.len()
        );
        debug!("System-prompt snapshot for {key:?}:\n{system_prompt}");

        self.snapshots.lock().await.insert(
            key.clone(),
            SystemSnapshot {
                system_prompt: system_prompt.clone(),
                date: today,
            },
        );

        if system_prompt.is_empty() {
            None
        } else {
            Some(system_prompt)
        }
    }

    // -----------------------------------------------------------------------
    // Message handling
    // -----------------------------------------------------------------------

    /// Send one piece of the turn's prose, as the round that produced it
    /// finishes.
    ///
    /// A failure is logged and swallowed rather than ending the turn. A
    /// turn can now send up to `[tools.tool_rounds] unattended` messages,
    /// which is enough to meet a channel's rate limit, and losing the
    /// whole turn's remaining work to one refused send would be a much
    /// worse outcome than a gap in the narration.
    ///
    /// Typing is restarted after a send only when `more_to_come` is
    /// true: the indicator stops on its own when a message lands, and a
    /// turn that goes quiet while it is still working is exactly what
    /// this change exists to prevent — but that only holds while more
    /// of the turn is still ahead. Callers pass `true` from the
    /// tool-call branch (another round is coming) and `false` from the
    /// tool-less branch that ends the turn. Restarting unconditionally,
    /// including on that last send, is harmless on Matrix but not on
    /// Discord: `stop_typing` there is a no-op (Discord's indicator
    /// expires on its own and cannot be cancelled — see the comment at
    /// `channel/discord.rs:217`), and `broadcast_typing` lasts about
    /// ten seconds, so a restart on the final send strands the bot
    /// looking like it's still typing for ten seconds after the turn is
    /// actually done.
    async fn send_turn_message(
        &self,
        incoming: &crate::channel::IncomingMessage,
        text: &str,
        more_to_come: bool,
    ) {
        let out = OutgoingMessage {
            content: text.to_string(),
            room_id: incoming.room_id.clone(),
            thread_id: incoming.thread_id.clone(),
        };
        if let Err(e) = self.channels.send(&out).await {
            warn!("Failed to send a turn message: {e:#}");
        }
        if more_to_come {
            let _ = self.channels.start_typing(&incoming.room_id).await;
        }
    }

    async fn handle_message(
        self: Arc<Self>,
        incoming: crate::channel::IncomingMessage,
    ) -> anyhow::Result<()> {
        info!("Message from {}: {}", incoming.sender, incoming.content);

        let key: ConversationKey = (incoming.room_id.clone(), incoming.thread_id.clone());

        // Check for day boundary → dispatch configured session policy
        self.maybe_handle_day_boundary(&key).await;

        let session_id = self.get_or_create_session(&key).await;

        // Get system prompt (snapshot: rebuilt at most once per local day per key)
        let system = self.get_system_prompt(&key).await;

        // Inject prefetch context from previous turn (if any)
        let prefetch_result = self.prefetch_cache.lock().await.remove(&key);
        let system_with_context = match (system, prefetch_result) {
            (Some(sys), Some(ctx)) if !ctx.is_empty() && ctx != "No results found." => Some(
                format!("{sys}\n\n---\n\n<memory-context>\n{ctx}\n</memory-context>"),
            ),
            (sys, _) => sys,
        };

        // Append user message
        {
            let msg = build_user_message(&incoming.content, &incoming.attachments);
            self.history
                .lock()
                .await
                .entry(key.clone())
                .or_default()
                .push(msg.clone());
            let _ = self.persist(&session_id, &msg);
        }
        let _ = self.channels.start_typing(&incoming.room_id).await;

        // Refresh MCP tools if any server signalled a change.
        if let Some(tools) = &self.tools {
            tools.refresh_if_needed().await;
        }
        // Filtered the same way `run_llm_turn` filters an ACP turn's
        // specs (see `visible_tool_predicate`'s doc): a Matrix/Discord
        // turn has no ACP client, so every client-capability flag is
        // forced `false` here. That hides the names with no agent-side
        // body — the lifecycle trio and the skill tools, which could
        // otherwise only ever answer "no editor is connected" — while
        // `host_access_enabled` remains the sole gate on the seven
        // unified names (`file_read`, `file_write`, `file_append`,
        // `file_delete`, `dir_list`, `dir_walk`, `shell`), whose
        // agent-side bodies are what this transport runs.
        let host_access_enabled = self.config.tools.host_access.enabled;
        let tool_specs = match &self.tools {
            Some(t) => Some(
                t.specs_filtered(crate::serve::visible_tool_predicate(
                    host_access_enabled,
                    false,
                    false,
                    false,
                    false,
                ))
                .await,
            ),
            None => None,
        };

        // Context compression config
        let compression_config = &self.config.compression;

        // Tool-calling loop
        // Rounds this turn, not tool calls in the history. History is
        // restored from disk now, so counting the whole thing would spend a
        // room's entire budget on what it did before the process started —
        // and the check runs before compaction, so nothing could ever trim
        // it back.
        let mut round = 0usize;
        // Matrix and Discord have no way to cancel a turn in flight, so
        // they are judged `unattended` — the same budget every route
        // without an interrupt gets. See `[tools.tool_rounds]`.
        let round_limit = self
            .config
            .tools
            .tool_rounds
            .limit(crate::serve::RoundBudget::Unattended);
        loop {
            let messages = {
                self.history
                    .lock()
                    .await
                    .get(&key)
                    .cloned()
                    .unwrap_or_default()
            };

            if round_limit.is_some_and(|max| round >= max) {
                warn!("Reached max tool rounds ({round}), stopping");
                break;
            }

            // Check if context compression is needed
            let provider = self.provider_for(&incoming.room_id);
            let messages = match maybe_compress(
                &*provider,
                system_with_context.as_deref(),
                &messages,
                tool_specs.as_deref(),
                compression_config,
            )
            .await
            {
                Ok(Some(result)) => {
                    // Replace in-memory history with compressed version
                    *self.history.lock().await.entry(key.clone()).or_default() =
                        result.compressed.clone();
                    // No summary means nothing was summarized — the pass
                    // only trimmed oversized parts down to the budget, and
                    // there is no checkpoint to move.
                    if let Some(summary) = &result.summary
                        && let Err(e) = self.session_store.append_summary(
                            &session_id,
                            summary,
                            result.keep_recent,
                        )
                    {
                        warn!("Failed to persist compaction summary: {e}");
                    }
                    result.compressed
                }
                Ok(None) => messages,
                Err(e) => {
                    warn!("Context compression failed, continuing with full history: {e}");
                    messages
                }
            };

            // Hydrate `ImageRef` parts back to `Image` so the provider
            // sees the actual bytes. Cache misses degrade to a text
            // marker that preserves the hash for context references.
            let messages_for_provider = hydrate_history(&messages);
            // Measured against exactly what goes on the wire, hydration
            // included — the response's own token count is what teaches the
            // estimate how wrong it is.
            let estimated = estimate_request_tokens(
                system_with_context.as_deref(),
                &messages_for_provider,
                tool_specs.as_deref(),
            );
            let response = provider
                .chat(
                    system_with_context.as_deref(),
                    &messages_for_provider,
                    tool_specs.as_deref(),
                )
                .await;
            if let Ok(resp) = &response {
                record_prompt_usage(estimated, resp.prompt_usage.as_ref());
            }

            match response {
                Err(e) => {
                    error!("Provider error: {e:#}");
                    let _ = self.channels.stop_typing(&incoming.room_id).await;
                    let _ = self
                        .channels
                        .send(&OutgoingMessage::new(
                            format!("⚠️ Error: {e}"),
                            incoming.room_id.clone(),
                        ))
                        .await;
                    return Ok(());
                }
                Ok(resp) if !resp.has_tool_calls() => {
                    let text = resp.text.unwrap_or_default();
                    let msg = ChatMessage::assistant(&text);
                    self.history
                        .lock()
                        .await
                        .entry(key.clone())
                        .or_default()
                        .push(msg.clone());
                    let _ = self.persist(&session_id, &msg);
                    if !text.is_empty() {
                        // Turn's last send: nothing follows, so don't
                        // restart typing.
                        self.send_turn_message(&incoming, &text, false).await;
                    }
                    break;
                }
                Ok(resp) => {
                    round += 1;
                    let tool_calls = resp.tool_calls.clone();
                    if let Some(t) = resp.text.as_ref().filter(|s| !s.is_empty()) {
                        // Another round is still ahead: restart typing
                        // so it doesn't lapse while that round runs.
                        self.send_turn_message(&incoming, t, true).await;
                    }
                    let msg =
                        ChatMessage::assistant_with_tools(resp.text.clone(), tool_calls.clone());
                    self.history
                        .lock()
                        .await
                        .entry(key.clone())
                        .or_default()
                        .push(msg.clone());
                    // Carried down to the tool_result append below. A
                    // half-persisted pair is worse than neither: the
                    // in-memory history is correct either way and gets
                    // rebuilt next turn, but a tool_result alone on disk
                    // chains onto the preceding user message with no
                    // tool_use before it. `repair_tool_pairing` keeps the
                    // session loadable by dropping that orphan on
                    // reload — not a brick, but the record of what the
                    // tool returned is gone either way, so there is
                    // nothing to gain by writing it in the first place.
                    let tool_use_persisted = self.persist(&session_id, &msg);

                    // Execute tools concurrently. Each task gets the
                    // room's memory namespace via task_local so the
                    // memory tool writes under `memory/<namespace>/...`.
                    // tokio::spawn does not propagate task_local, so we
                    // re-bind it inside each spawned future.
                    let tools = Arc::clone(self.tools.as_ref().unwrap());
                    let ns = self
                        .config
                        .namespace_for_room(&incoming.room_id)
                        .to_string();
                    let room_id_for_timer = incoming.room_id.clone();
                    // Permission gate for the channel path.
                    //
                    // `Origin::Channel` never returns `Ask` — a channel
                    // turn is asynchronous, so there is nobody to hold
                    // it open for — which is why this needs no `approve`
                    // and no serialisation. Refused calls simply never
                    // reach `tokio::spawn`.
                    //
                    // Shadowing `tool_calls` is safe: the assistant
                    // message above was built from a clone, so the
                    // tool_use blocks in history keep every call, and
                    // the refusals below supply their tool_results.
                    let kinds = tools.kinds().await;
                    // `routed_to_client` is `false`, and it is `false`
                    // for the whole channel path rather than a value
                    // read from somewhere: a Matrix/Discord turn never
                    // has an ACP client, so the host gate speaks for
                    // every one of these names here exactly as it always
                    // has. #270 changed the gate's meaning on ACP turns
                    // only; this path is the control case.
                    let (tool_calls, refused) = crate::tools::policy::partition_without_asking(
                        crate::tools::policy::Origin::Channel,
                        &tool_calls,
                        &kinds,
                        self.config.tools.host_access.enabled,
                        false,
                    );
                    for (id, reason) in &refused {
                        info!("Refused tool call {id} on the channel path: {reason}");
                    }

                    let mut handles = Vec::with_capacity(tool_calls.len());
                    for call in tool_calls {
                        let tools = Arc::clone(&tools);
                        let ns = ns.clone();
                        let origin = crate::timer::TimerOrigin::Chat {
                            room_id: room_id_for_timer.clone(),
                        };
                        handles.push(tokio::spawn(
                            crate::tools::workspace_tools::scope_memory_namespace(
                                ns,
                                crate::timer::scope_timer_origin(origin, async move {
                                    info!("Executing tool: {} (id={})", call.name, call.id);
                                    let output = tools.execute(&call).await;
                                    info!(
                                        "Tool {} result ({} image(s) attached): {}",
                                        call.name,
                                        output.images.len(),
                                        output.text
                                    );
                                    (call.id, output)
                                }),
                            ),
                        ));
                    }

                    let mut results = Vec::with_capacity(handles.len());
                    for handle in handles {
                        match handle.await {
                            Ok(r) => results.push(r),
                            Err(e) => warn!("Tool task panicked: {e}"),
                        }
                    }

                    // Every tool_use owes the model a tool_result, and
                    // the reason is more useful than silence. The turn
                    // continues — the model may have another route.
                    for (id, reason) in refused {
                        results.push((id, crate::tools::ToolOutput::from(reason)));
                    }

                    // Split each ToolOutput into the text body (becomes a
                    // tool_result block) and any images (sibling Image
                    // blocks on the same user message).
                    let mut text_results = Vec::with_capacity(results.len());
                    let mut images = Vec::new();
                    for (id, output) in results {
                        text_results.push((id, output.text));
                        images.extend(output.images);
                    }
                    let msg = ChatMessage::tool_results_with_images(text_results, images);
                    self.history
                        .lock()
                        .await
                        .entry(key.clone())
                        .or_default()
                        .push(msg.clone());
                    if tool_use_persisted {
                        let _ = self.persist(&session_id, &msg);
                    }
                }
            }
        }

        // Scrub `Image` parts in the just-completed history into compact
        // `ImageRef` references backed by the workspace-external image
        // cache. Long-lived in-memory storage stays hash-only between
        // turns; subsequent turns re-hydrate from cache for provider
        // calls. No-op when the image cache is disabled (PR1 marker
        // still applies on the persistence side).
        {
            let mut hist = self.history.lock().await;
            if let Some(entry) = hist.get_mut(&key) {
                scrub_history_inplace(entry, self.image_cache.as_deref());
            }
        }

        let _ = self.channels.stop_typing(&incoming.room_id).await;

        // Spawn background prefetch for next turn. This turn's prose was
        // already sent round by round via `send_turn_message` above, so
        // there is nothing left to send here — this is purely priming the
        // cache the *next* turn will read from.
        if let Some(tools) = &self.tools {
            let tools = Arc::clone(tools);
            let agent = Arc::clone(&self);
            let key_clone = key.clone();
            let query = incoming.content.clone();
            tokio::spawn(async move {
                let input = serde_json::json!({ "query": query, "limit": 5 });
                let result = tools
                    .execute(&ToolCall {
                        id: "prefetch".to_string(),
                        name: "workspace_search".to_string(),
                        input,
                    })
                    .await;
                agent
                    .prefetch_cache
                    .lock()
                    .await
                    .insert(key_clone, result.text);
            });
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Build a user `ChatMessage` from incoming text + binary attachments.
/// Attachments are base64-encoded and emitted as `ContentPart::Image` parts.
fn build_user_message(text: &str, attachments: &[Attachment]) -> ChatMessage {
    if attachments.is_empty() {
        return ChatMessage::user(text);
    }
    use base64::{Engine, engine::general_purpose::STANDARD};
    let images = attachments
        .iter()
        .map(|a| (a.media_type.clone(), STANDARD.encode(&a.data)));
    ChatMessage::user_with_images(text, images)
}

#[cfg(test)]
mod tests {
    use crate::provider::ToolCall;
    use crate::tools::ToolKind;
    use crate::tools::policy::{Origin, partition_without_asking};
    use sapphire_framework::workspace::{AppContext, Workspace, WorkspaceState};
    use std::sync::{Arc, Mutex};

    static TEST_CTX: AppContext = AppContext::new("sapphire-agent").allow_external_paths();

    fn test_workspace() -> Arc<WorkspaceState> {
        // `AppContext` panics on `cache_dir()` access until this is set,
        // and `Workspace::from_root` reads it. First writer wins, so
        // calling it on every invocation is safe.
        TEST_CTX.set_cache_dir(std::env::temp_dir().join("sapphire-agent-channel-gate-test-cache"));
        // Leaked on purpose: this is a test binary and the OS reclaims
        // the directory when it exits.
        let dir = Box::leak(Box::new(tempfile::tempdir().unwrap()));
        std::fs::create_dir_all(dir.path().join(".sapphire-agent")).unwrap();
        let ws = Workspace::from_root(&TEST_CTX, dir.path()).unwrap();
        Arc::new(WorkspaceState::open(ws).unwrap())
    }

    /// Six calls, run through the channel gate under both `host_access`
    /// states. Shared by both phases below so the two assertions stay
    /// about the same call list rather than drifting apart.
    async fn channel_gate_test_calls() -> (Vec<ToolCall>, Vec<(String, ToolKind)>) {
        let tools = crate::tools::default_tool_set(
            test_workspace(),
            Some("test-tavily-key".to_string()),
            &[],
            4,
        )
        .await;
        let kinds = tools.kinds().await;

        let calls: Vec<ToolCall> = [
            "shell",
            "web_search",
            "memory_add",
            "file_read",
            "workspace_sync",
            "mcp__somewhere__do_thing",
        ]
        .iter()
        .enumerate()
        .map(|(i, name)| ToolCall {
            id: format!("c{i}"),
            name: (*name).to_string(),
            input: serde_json::json!({}),
        })
        .collect();

        (calls, kinds)
    }

    /// `shell` (`Execute`) and `workspace_sync`/the MCP tool (`Other`)
    /// are refused over chat regardless of `host_access` — that is
    /// `decide`'s risky bucket, which the host switch does not touch.
    /// `web_search`/`memory_add` (workspace-scoped, not host tools) keep
    /// working either way. `file_read` is the one call that moves
    /// between the two phases below: this test pins it with
    /// `host_access` **on**, matching the pre-host-switch behaviour this
    /// permission work otherwise preserves; the sibling test below pins
    /// what changed.
    #[tokio::test]
    async fn the_channel_gate_refuses_shell_but_keeps_the_chat_tools() {
        let (calls, kinds) = channel_gate_test_calls().await;

        let (permitted, refused) =
            partition_without_asking(Origin::Channel, &calls, &kinds, true, false);

        let kept: Vec<&str> = permitted.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(
            kept,
            vec!["web_search", "memory_add", "file_read"],
            "the chat bots' own tools must keep working, and file_read is \
             reachable once host access is on"
        );

        let name_of = |id: &str| {
            calls
                .iter()
                .find(|c| c.id == id)
                .map(|c| c.name.clone())
                .unwrap()
        };
        let blocked: Vec<String> = refused.iter().map(|(id, _)| name_of(id)).collect();
        assert_eq!(
            blocked,
            vec!["shell", "workspace_sync", "mcp__somewhere__do_thing"],
            "Execute and Other must not be reachable over chat even with host \
             access on — the switch does not override the risky bucket"
        );

        // Every refusal names its tool, so the model can say which call
        // it lost and why.
        for (id, reason) in &refused {
            let name = name_of(id);
            assert!(reason.contains(&name), "{reason} should name {name}");
        }

        // Every call is accounted for exactly once: the model is owed a
        // tool_result for each tool_use, and a call lost here would make
        // the next provider request fail rather than merely misbehave.
        assert_eq!(permitted.len() + refused.len(), calls.len());
    }

    /// The hole this permission work closes: with `host_access` **off**
    /// (the default), `file_read` — like the other six `HOST_TOOLS` —
    /// is refused over chat too, not just `Execute`/`Other`. Before
    /// this, `file_write`/`file_delete` (`Edit`/`Delete`) were reachable
    /// from an unasked Discord message; `file_read` sits in the same
    /// `HOST_TOOLS` list and is pinned here as the reachable proxy for
    /// all of them. `shell`/`workspace_sync`/the MCP tool stay refused
    /// for the unrelated, pre-existing reason (`Execute`/`Other`).
    #[tokio::test]
    async fn the_channel_gate_also_refuses_host_tools_when_host_access_is_off() {
        let (calls, kinds) = channel_gate_test_calls().await;

        let (permitted, refused) =
            partition_without_asking(Origin::Channel, &calls, &kinds, false, false);

        let kept: Vec<&str> = permitted.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(
            kept,
            vec!["web_search", "memory_add"],
            "only the non-host workspace tools survive with host access off"
        );

        let name_of = |id: &str| {
            calls
                .iter()
                .find(|c| c.id == id)
                .map(|c| c.name.clone())
                .unwrap()
        };
        let blocked: Vec<String> = refused.iter().map(|(id, _)| name_of(id)).collect();
        assert_eq!(
            blocked,
            vec![
                "shell",
                "file_read",
                "workspace_sync",
                "mcp__somewhere__do_thing"
            ],
            "file_read joins the refusal list; shell and Other are still \
             refused for their own, unrelated reason"
        );

        for (id, reason) in &refused {
            let name = name_of(id);
            assert!(reason.contains(&name), "{reason} should name {name}");
        }

        assert_eq!(permitted.len() + refused.len(), calls.len());
    }

    // -------------------------------------------------------------------
    // Turn-driving harness
    //
    // Everything below builds a real `Agent` — scripted provider, tool
    // set, session store — so a test can drive `handle_message` end to
    // end and observe what got sent, rather than only reaching the
    // tool-policy gate as the tests above do. Nothing here is production
    // code: it exists so Task 6b's delivery-shape tests have something
    // to run against.
    // -------------------------------------------------------------------

    /// Records what the agent sent, so a test can assert on the shape of a
    /// turn's delivery rather than only its final text.
    ///
    /// `fail_first` exists for one test: a send that the channel refuses
    /// must not take the rest of the turn down with it. Counting calls
    /// rather than holding a queue of outcomes keeps the stub to the one
    /// behaviour that is actually needed.
    ///
    /// `typing_started_after` exists for FIX 2's test: it records, for
    /// each `start_typing` call, how many messages `sent` already held
    /// at that moment. That ties a typing restart to its position in
    /// the turn rather than just counting restarts, so a test can
    /// assert the indicator is restarted after a mid-turn send but not
    /// after the turn's last one — the exact ordering `send_turn_message`
    /// is responsible for.
    struct RecordingChannel {
        sent: Arc<Mutex<Vec<crate::channel::OutgoingMessage>>>,
        fail_first: bool,
        calls: Mutex<usize>,
        typing_started_after: Arc<Mutex<Vec<usize>>>,
    }

    #[async_trait::async_trait]
    impl crate::channel::Channel for RecordingChannel {
        fn name(&self) -> &str {
            "test"
        }

        async fn send(&self, message: &crate::channel::OutgoingMessage) -> anyhow::Result<()> {
            let mut calls = self.calls.lock().unwrap();
            *calls += 1;
            if self.fail_first && *calls == 1 {
                return Err(anyhow::anyhow!("simulated send failure"));
            }
            self.sent.lock().unwrap().push(message.clone());
            Ok(())
        }

        async fn start_typing(&self, _room_id: &str) -> anyhow::Result<()> {
            let sent_so_far = self.sent.lock().unwrap().len();
            self.typing_started_after.lock().unwrap().push(sent_so_far);
            Ok(())
        }

        // `Channels::listen_all` is never driven in these tests —
        // `handle_message` is called directly — so this only needs to
        // satisfy the trait, not do anything.
        async fn listen(
            &self,
            _tx: tokio::sync::mpsc::Sender<crate::channel::IncomingMessage>,
        ) -> anyhow::Result<()> {
            Ok(())
        }
    }

    /// What a test gets back from `build_test_agent` and its wrappers: the
    /// agent itself, what it sent, and — per FIX 2's
    /// `typing_started_after` — how many sends preceded each typing
    /// restart. Named so the three-`Arc` return type doesn't trip
    /// clippy's `type_complexity` lint at every one of these functions.
    type TestAgentHandles = (
        Arc<super::Agent>,
        Arc<Mutex<Vec<crate::channel::OutgoingMessage>>>,
        Arc<Mutex<Vec<usize>>>,
    );

    /// Build an `Agent` whose provider replays `responses` in order and
    /// whose only channel records what it is asked to send.
    ///
    /// Mirrors `ServeState::build_for_test` (src/serve/mod.rs): leak the
    /// `TempDir` on purpose — this is a test binary and the OS reclaims
    /// the directory when it exits.
    fn build_test_agent(
        responses: Vec<crate::provider::ChatResponse>,
        fail_first: bool,
    ) -> TestAgentHandles {
        let dir = Box::leak(Box::new(tempfile::tempdir().unwrap()));
        let base = dir.path().to_path_buf();

        // Minimal config: no room_profile mapping for the test room, so
        // `ProviderRegistry::for_room` falls through to the "anthropic"
        // key — which `for_test` below points at the same stub.
        let config = crate::config::Config::parse_for_test(
            r#"
[anthropic]
api_key = "test"
"#,
        );

        let provider = crate::serve::StubProvider::new(responses);
        // Registered under both names, as `ServeState::build_for_test_with`
        // does: the fallback path (no room_profile match) resolves the
        // built-in "anthropic" key.
        let registry = crate::provider::registry::ProviderRegistry::for_test(
            &["anthropic", "stub"],
            Arc::new(provider),
        );

        let sent = Arc::new(Mutex::new(Vec::new()));
        let typing_started_after = Arc::new(Mutex::new(Vec::new()));
        let channel = RecordingChannel {
            sent: Arc::clone(&sent),
            fail_first,
            calls: Mutex::new(0),
            typing_started_after: Arc::clone(&typing_started_after),
        };
        let channels = crate::channel::Channels::new(
            vec![(
                "test".to_string(),
                Arc::new(channel) as Arc<dyn crate::channel::Channel>,
            )],
            std::collections::HashMap::new(),
        );

        let workspace = Arc::new(crate::workspace::Workspace::new(
            base.join("workspace"),
            crate::config::DigestConfig::default(),
        ));

        // Registers the same `echo` tool `ServeState::build_for_test`
        // advertises, so Task 6b's scripted tool calls (which name
        // `echo`) resolve without needing the full
        // `crate::tools::default_tool_set` wiring.
        let tools = Arc::new(crate::tools::ToolSet::new(
            vec![Box::new(crate::serve::EchoTool::new())],
            Vec::new(),
        ));

        let session_store = Arc::new(crate::session::SessionStore::new(
            base.join("sessions"),
            "test",
            None,
        ));

        let agent = super::Agent::new(
            config,
            Arc::new(channels),
            Arc::new(registry),
            workspace,
            Some(tools),
            session_store,
            None,
        );

        (Arc::new(agent), sent, typing_started_after)
    }

    /// Convenience wrapper over `build_test_agent` with a channel that
    /// accepts every send.
    fn agent_with_scripted_provider(
        responses: Vec<crate::provider::ChatResponse>,
    ) -> TestAgentHandles {
        build_test_agent(responses, false)
    }

    /// Same, but the channel refuses the first send.
    ///
    /// Exists for the delivery-shape tests that need to prove a refused
    /// send doesn't take the rest of a turn down with it — see
    /// `a_failed_send_does_not_end_the_turn`.
    fn agent_with_failing_first_send(
        responses: Vec<crate::provider::ChatResponse>,
    ) -> TestAgentHandles {
        build_test_agent(responses, true)
    }

    fn incoming(text: &str) -> crate::channel::IncomingMessage {
        crate::channel::IncomingMessage {
            id: "m1".to_string(),
            sender: "@tester:example.com".to_string(),
            content: text.to_string(),
            room_id: "test-room".to_string(),
            timestamp: 0,
            thread_id: None,
            attachments: Vec::new(),
        }
    }

    /// The test harness itself. `Agent` gets built, `handle_message` runs
    /// one turn, and the reply lands on the stub channel. Task 6b's
    /// behaviour tests all ride on this, so nothing further proceeds
    /// until this passes.
    #[tokio::test]
    async fn the_test_harness_drives_a_turn_end_to_end() {
        let (agent, sent, _typing) =
            agent_with_scripted_provider(vec![crate::provider::ChatResponse {
                prompt_usage: None,
                text: Some("ok".to_string()),
                tool_calls: Vec::new(),
                stop_reason: None,
            }]);

        agent.handle_message(incoming("hello")).await.unwrap();

        let bodies: Vec<String> = sent
            .lock()
            .unwrap()
            .iter()
            .map(|m| m.content.clone())
            .collect();
        assert_eq!(bodies, vec!["ok".to_string()]);
    }

    /// ツールを呼ぶターンは複数メッセージに分かれる。「○○するぞ」と
    /// 「○○したぞ」が1通に潰れていたのが、この変更の直す対象である。
    #[tokio::test]
    async fn a_tool_using_turn_is_delivered_as_several_messages() {
        let (agent, sent, _typing) = agent_with_scripted_provider(vec![
            crate::provider::ChatResponse {
                prompt_usage: None,
                text: Some("調べます".to_string()),
                tool_calls: vec![crate::provider::ToolCall {
                    id: "call-1".to_string(),
                    name: "echo".to_string(),
                    input: serde_json::json!({ "text": "ping" }),
                }],
                stop_reason: None,
            },
            crate::provider::ChatResponse {
                prompt_usage: None,
                text: Some("見つかりました".to_string()),
                tool_calls: Vec::new(),
                stop_reason: None,
            },
        ]);

        agent.handle_message(incoming("なぜ遅い")).await.unwrap();

        let bodies: Vec<String> = sent
            .lock()
            .unwrap()
            .iter()
            .map(|m| m.content.clone())
            .collect();
        assert_eq!(
            bodies,
            vec!["調べます".to_string(), "見つかりました".to_string()]
        );
    }

    /// ツールを呼ばない普通の会話は、今までどおり1通のまま。この変更で
    /// 日常の会話が細切れになってはいけない。
    #[tokio::test]
    async fn an_ordinary_reply_is_still_one_message() {
        let (agent, sent, _typing) =
            agent_with_scripted_provider(vec![crate::provider::ChatResponse {
                prompt_usage: None,
                text: Some("こんにちは".to_string()),
                tool_calls: Vec::new(),
                stop_reason: None,
            }]);

        agent.handle_message(incoming("やあ")).await.unwrap();

        let bodies: Vec<String> = sent
            .lock()
            .unwrap()
            .iter()
            .map(|m| m.content.clone())
            .collect();
        assert_eq!(bodies, vec!["こんにちは".to_string()]);
    }

    /// 送信に失敗してもターンは落ちない。上限ぶんの連投がレート制限に
    /// 触れうる以上、1通の失敗でターン全体を失うのは高すぎる。
    #[tokio::test]
    async fn a_failed_send_does_not_end_the_turn() {
        let (agent, sent, _typing) = agent_with_failing_first_send(vec![
            crate::provider::ChatResponse {
                prompt_usage: None,
                text: Some("最初".to_string()),
                tool_calls: vec![crate::provider::ToolCall {
                    id: "call-1".to_string(),
                    name: "echo".to_string(),
                    input: serde_json::json!({ "text": "ping" }),
                }],
                stop_reason: None,
            },
            crate::provider::ChatResponse {
                prompt_usage: None,
                text: Some("最後".to_string()),
                tool_calls: Vec::new(),
                stop_reason: None,
            },
        ]);

        let result = agent.handle_message(incoming("やって")).await;

        assert!(result.is_ok(), "1通の失敗でターンを落とさない");
        let bodies: Vec<String> = sent
            .lock()
            .unwrap()
            .iter()
            .map(|m| m.content.clone())
            .collect();
        assert_eq!(bodies, vec!["最後".to_string()], "後続は届く");
    }

    /// FIX 2: typing restarts after a mid-turn send (more of the turn is
    /// still coming) but must not restart after the turn's *last* send.
    /// On Discord that restart would strand the ~10s indicator with
    /// nothing left in the turn to clear it — see the doc on
    /// `send_turn_message`.
    ///
    /// `typing_started_after` records, for every restart, how many
    /// messages `sent` held at that moment: one entry for
    /// `handle_message`'s own start-of-turn call (before anything is
    /// sent, so `0`), one for the mid-turn send that follows the first,
    /// tool-calling round (so `1`) — and, if the old unconditional
    /// restart ever came back, a trailing `2` for the final send. The
    /// exact sequence `[0, 1]` is the pin.
    #[tokio::test]
    async fn typing_is_not_restarted_after_the_turns_last_send() {
        let (agent, _sent, typing) = agent_with_scripted_provider(vec![
            crate::provider::ChatResponse {
                prompt_usage: None,
                text: Some("調べます".to_string()),
                tool_calls: vec![crate::provider::ToolCall {
                    id: "call-1".to_string(),
                    name: "echo".to_string(),
                    input: serde_json::json!({ "text": "ping" }),
                }],
                stop_reason: None,
            },
            crate::provider::ChatResponse {
                prompt_usage: None,
                text: Some("見つかりました".to_string()),
                tool_calls: Vec::new(),
                stop_reason: None,
            },
        ]);

        agent.handle_message(incoming("なぜ遅い")).await.unwrap();

        assert_eq!(*typing.lock().unwrap(), vec![0, 1]);
    }

    /// Task 2 の本題: `invalidate_system_prompts` はスナップショット全消去
    /// とピン留めキャッシュ全クリアの *両方* を行う。スナップショットだけ
    /// 消えてもピンが古いままなら編集は反映されず、ピンだけ消えてもスナッ
    /// プショットが古いままなら反映されない。両方消えて初めて、ディスク上
    /// の編集がプロンプトに現れる。
    #[tokio::test]
    async fn invalidate_system_prompts_clears_snapshots_and_pins() {
        let (agent, _sent, _typing) = agent_with_scripted_provider(vec![]);
        let workspace = Arc::clone(&agent.workspace);
        let dir = workspace.dir().to_path_buf();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("AGENTS.md"), "first content.\n").unwrap();

        let key: crate::session::ConversationKey = ("test-room".to_string(), None);
        let first = agent
            .get_system_prompt(&key)
            .await
            .expect("AGENTS.md exists, so the prompt is non-empty");
        assert!(first.contains("first content."), "{first}");

        // pin とスナップショットの両方が v1 を保持している状態で編集する
        std::fs::write(dir.join("AGENTS.md"), "second content.\n").unwrap();
        let pinned = agent.get_system_prompt(&key).await.expect("pinned prompt");
        assert!(
            pinned.contains("first content."),
            "ピンとスナップショット中は編集が反映されない"
        );
        assert!(!pinned.contains("second content."));

        agent.invalidate_system_prompts().await;

        let refreshed = agent.get_system_prompt(&key).await.expect("rebuilt prompt");
        assert!(
            refreshed.contains("second content."),
            "両方のキャッシュが消えた後は編集が反映される"
        );
    }
}
