//! HTTP API server for sapphire-agent control API (JSON-RPC 2.0 over HTTP).
//!
//! Endpoint: POST /rpc  (chat, initialize, list_sessions, get_session, voice/*)
//!           GET  /rpc  (Phase 2: server→client SSE push, currently 405)
//!           POST /a2a  (Agent2Agent Protocol; gated by [a2a].enabled)
//!           GET  /.well-known/agent-card.json
//!           POST /mcp  (MCP server; write_report / recall_memory tools)
//!           GET  /acp  (Agent Client Protocol over WebSocket; gated by [acp].enabled)
//!
//! Session management uses a `Session-Id` request/response header.

pub mod a2a;
pub mod acp;
pub mod acp_permissions;
pub mod mcp;

use crate::acp_session::AcpSessionStore;
use crate::channel::RoomInfo;
use crate::config::Config;
use crate::context_compression::{estimate_request_tokens, maybe_compress, record_prompt_usage};
use crate::provider::registry::ProviderRegistry;
use crate::provider::{ChatMessage, ContentPart, Provider, ToolSpec, UserInputKind};
use crate::session::{ConversationKey, SessionStore};
use crate::subagent_cache::SubagentCache;
use crate::tools::ToolSet;
use crate::voice::VoiceProviders;
use crate::workspace::Workspace;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::IntoResponse;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::routing::post;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::convert::Infallible;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tracing::{debug, error, info, warn};

// ---------------------------------------------------------------------------
// Shared server state
// ---------------------------------------------------------------------------

/// Map of `device_id` → (`room_profile`, push channel), populated by
/// `voice/subscribe`. Heartbeat (and any future server-initiated voice
/// notifier) looks up subscribers here to deliver TTS audio.
///
/// Keyed by `device_id` alone because a single satellite only ever
/// holds one active voice session at a time — the satellite tells us
/// which room_profile it's bound to when it subscribes, and we keep
/// that around as the reverse index so heartbeat tasks don't have to
/// duplicate the value.
pub type VoiceSubscribers =
    tokio::sync::Mutex<HashMap<String, (String, mpsc::Sender<crate::voice::VoicePushItem>)>>;

pub struct ServeState {
    pub(crate) config: Config,
    pub(crate) registry: Arc<ProviderRegistry>,
    pub(crate) workspace: Arc<Workspace>,
    pub(crate) tools: Arc<ToolSet>,
    /// Standing answers to `session/request_permission`. Shared across
    /// connections because the record is host-wide, keyed by room
    /// profile inside.
    pub(crate) permissions: Arc<acp_permissions::PermissionStore>,
    /// Cross-device session store (kind = `"rpc"` for now; #122 PR 3
    /// renames the on-disk dir to `cross-device/`). Holds the
    /// user-selectable, multi-device sessions resumed via
    /// `--resume <grain-id>`.
    pub(crate) cross_device_session_store: Arc<SessionStore>,
    /// Device-default session store (kind = `"device-default"`). Holds the
    /// per-`(device_id, room_profile)` always-on session that heartbeat
    /// pushes target and that a satellite falls into when no other session
    /// is selected. Lazy-created, daily-rotated. See #122.
    pub(crate) device_default_session_store: Arc<SessionStore>,
    /// Autonomous session store (kind = `"autonomous"`). Holds the
    /// sessions the idle loop runs, one per task — kept in its own
    /// directory for the same reason the ACP store is: they are ordinary
    /// sessions for the purpose of reading them back, and not chat for
    /// the purpose of anything else. Its files are named by agent-day
    /// (`{date}-{uuid}.jsonl`) — see `SessionStore::with_dated_files`.
    pub(crate) autonomous_session_store: Arc<SessionStore>,
    /// MCP session store (kind = `"mcp"`). Holds long-lived
    /// per-project sessions written through `/mcp`'s `write_report`
    /// tool — kept physically separate from `cross_device_session_store` so the
    /// project index scan and any future MCP-specific retention only
    /// see MCP traffic.
    pub(crate) mcp_session_store: Arc<SessionStore>,
    /// Reverse index `(namespace, project) → session_id` for the MCP
    /// session store. Seeded at startup from `SessionMeta.project` and
    /// maintained on `create_mcp_session`. The mapping isn't
    /// persisted to its own file: each session file's first-line meta
    /// IS the source of truth, so a restart rebuilds the index by
    /// scanning `sessions/<ns>/mcp/*.jsonl` meta lines.
    pub(crate) mcp_project_index: tokio::sync::Mutex<HashMap<(String, String), String>>,
    /// In-memory conversation history, keyed by session_id.
    /// Lazy-loaded from JSONL on first access.
    pub(crate) sessions: tokio::sync::Mutex<HashMap<String, Vec<ChatMessage>>>,
    /// Sessions that have been issued an ID via `initialize` but have not yet
    /// received a message — file creation is deferred until the first chat so
    /// that quitting without sending anything leaves no empty file behind.
    /// Maps internal UUID → reserved public_id (grain-id).
    pub(crate) pending_sessions: tokio::sync::Mutex<HashMap<String, String>>,
    /// Per-session room_profile pin from `initialize`. Sessions absent
    /// from this map fall through to the background provider. Not
    /// persisted across restarts — clients must re-pass `room_profile`
    /// on resume.
    pub(crate) session_room_profiles: tokio::sync::Mutex<HashMap<String, String>>,
    /// Per-session room metadata supplied by the client at `initialize`
    /// (sapphire-agent-cli's `[device]` block, principally). Mirrors the
    /// channel-side `Channel::room_info()` lookup so the agent can tell
    /// the model "you are speaking through the living-room speaker; STT
    /// may have introduced typos" without baking that into AGENTS.md.
    /// Not persisted across restarts — clients must re-pass `device` on
    /// resume.
    pub(crate) session_room_metadata: tokio::sync::Mutex<HashMap<String, RoomInfo>>,
    /// Voice provider registry. `None` when no `[stt_provider.*]` /
    /// `[tts_provider.*]` blocks are configured — in that case the
    /// `voice/pipeline_run` method returns a method-not-available error.
    pub(crate) voice: Option<Arc<VoiceProviders>>,
    /// Workspace-external image cache. `None` when the operator set
    /// `[image_cache] enabled = false`, when cache directory resolution
    /// failed at startup, or when `dirs::cache_dir()` returned `None`
    /// (rare). Absent → no in-memory scrub; on-disk persistence still
    /// gets the hash-marker fallback from `SessionStore::append`.
    pub(crate) image_cache: Option<Arc<crate::image_cache::ImageCache>>,
    /// Active satellites, keyed by `(device_id, room_profile)`. Inserted
    /// by `voice/subscribe`, removed by the per-subscription writer task
    /// when its SSE channel closes (i.e. satellite disconnects).
    pub(crate) voice_subscribers: Arc<VoiceSubscribers>,
    /// Bearer token -> device -> room profile. Shared with ambient ingest so
    /// there is exactly one answer to "who is this token" in the process.
    pub(crate) device_auth: Arc<crate::device_auth::DeviceAuth>,
    /// How many live ACP connections hold each session.
    ///
    /// `session/load` lets two editors open one conversation. The
    /// history race in `run_llm_turn` — clone at the top, write the
    /// whole vector back at the end — then becomes reachable across
    /// connections instead of only within one. Fixing it needs a
    /// per-session lock around a whole turn, which is a separate job;
    /// this makes hitting it visible in the log, because a transcript
    /// that diverged with nothing written down cannot be debugged.
    pub(crate) open_acp_sessions: Arc<tokio::sync::Mutex<HashMap<String, usize>>>,
    /// Sessions an ACP client owns. Separate from the `/rpc` store
    /// because ACP is an externally-defined standard that will drift
    /// from the format we chose for ourselves.
    pub(crate) acp_session_store: Arc<AcpSessionStore>,
    /// Which session ids belong to ACP.
    ///
    /// Registered by the ACP adapter at `session/new` and on adopt.
    /// A set rather than a field on the session, because `run_llm_turn`
    /// is reached from several transports and only one of them knows
    /// the answer.
    pub(crate) acp_sessions: tokio::sync::Mutex<HashSet<String>>,
    /// Workspace-external cache of resumable subagent child
    /// conversations (`subagent_cache::SubagentCache`). `None` when the
    /// cache directory could not be opened at startup (read-only or
    /// missing `~/.cache` / `%LOCALAPPDATA%`); degrades rather than
    /// aborting startup — a resumed subagent falls back to one-shot
    /// rather than the process failing to start.
    pub(crate) subagent_cache: Option<Arc<SubagentCache>>,
    /// Terminals the model started and has not cleaned up, per agent
    /// session id.
    ///
    /// Keyed by session rather than by connection on purpose. ACP
    /// terminals are addressed by `session_id`, a session outlives a
    /// connection via `session/load`, and `terminal/release` kills the
    /// command — so releasing these because a socket dropped would
    /// kill a build over a network blip. Nothing here is cleaned up on
    /// disconnect; see `acp::release_connection_sessions`'s doc.
    ///
    /// `Arc`-wrapped, unlike most maps on this struct: the `AcpClient`
    /// each turn is handed (`AcpClientHandle` in `serve::acp`,
    /// `FakeClient` in `tools::acp_client`'s tests) reads and writes
    /// this same map directly, so `client_tools::ClientShellStart`'s
    /// cap check can see it without `tools::client_tools` depending on
    /// `ServeState` itself — see `TerminalRegistry`'s doc.
    pub(crate) acp_terminals: crate::tools::acp_client::TerminalRegistry,
}

impl ServeState {
    /// Construct a runtime ready for both the HTTP RPC server and
    /// the in-process channel handlers (Discord voice in particular).
    /// Shared across both so they read from the same session store /
    /// in-memory conversation map.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config: Config,
        registry: Arc<ProviderRegistry>,
        workspace: Arc<Workspace>,
        tools: Arc<ToolSet>,
        cross_device_session_store: Arc<SessionStore>,
        device_default_session_store: Arc<SessionStore>,
        // Autonomous sessions, one per task. See the field's doc.
        autonomous_session_store: Arc<SessionStore>,
        mcp_session_store: Arc<SessionStore>,
        voice: Option<Arc<VoiceProviders>>,
        image_cache: Option<Arc<crate::image_cache::ImageCache>>,
        device_auth: Arc<crate::device_auth::DeviceAuth>,
        acp_session_store: Arc<AcpSessionStore>,
        subagent_cache: Option<Arc<SubagentCache>>,
    ) -> Self {
        // Scan once on startup: each MCP session's first-line meta
        // carries `namespace` + `project`, so this reproduces the
        // logical mapping without a side-channel index file. The same
        // map is updated in-place when `write_report` creates a new
        // project session.
        let mut mcp_index: HashMap<(String, String), String> = HashMap::new();
        for meta in mcp_session_store.list_sessions() {
            let (Some(ns), Some(proj)) = (meta.namespace.clone(), meta.project.clone()) else {
                continue;
            };
            // `list_sessions` is sorted by `created_at`, so overwriting
            // here keeps the most recent session per (ns, project) —
            // matters only if a project ever ended up with multiple
            // session files (manual surgery, future reset semantics).
            mcp_index.insert((ns, proj), meta.session_id);
        }

        Self {
            config,
            registry,
            workspace,
            tools,
            // Built here rather than passed in: the path is fixed by the
            // host, nothing else needs to choose it, and threading an
            // eleventh argument through would earn nothing. Tests build
            // `ServeState` by struct literal and point this at a tempdir.
            permissions: Arc::new(acp_permissions::PermissionStore::open(
                acp_permissions::PermissionStore::default_path(),
            )),
            cross_device_session_store,
            device_default_session_store,
            autonomous_session_store,
            mcp_session_store,
            mcp_project_index: tokio::sync::Mutex::new(mcp_index),
            sessions: tokio::sync::Mutex::new(HashMap::new()),
            pending_sessions: tokio::sync::Mutex::new(HashMap::new()),
            session_room_profiles: tokio::sync::Mutex::new(HashMap::new()),
            session_room_metadata: tokio::sync::Mutex::new(HashMap::new()),
            voice,
            voice_subscribers: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            image_cache,
            device_auth,
            open_acp_sessions: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            acp_session_store,
            acp_sessions: tokio::sync::Mutex::new(HashSet::new()),
            subagent_cache,
            acp_terminals: Arc::new(std::sync::Mutex::new(HashMap::new())),
        }
    }

    /// Whether `session_id` belongs to an ACP client, and so should be
    /// persisted through `acp_session_store` instead of the `/rpc`
    /// store. Registered by the ACP adapter at `session/new` and on
    /// adopt.
    pub(crate) async fn is_acp(&self, session_id: &str) -> bool {
        self.acp_sessions.lock().await.contains(session_id)
    }

    /// Pick the [`SessionStore`] that owns `session_id`. Autonomous
    /// sessions land in `autonomous/`, device-default sessions in
    /// `device-default/`; everything else (cross-device text sessions,
    /// deferred sessions awaiting their first message) lives in
    /// `cross_device_session_store`'s `rpc/` tree. Falls back to the
    /// cross-device store so newly-`ensure_session`'d files (which
    /// haven't hit disk yet) commit to the right place. See #122.
    ///
    /// Autonomous is checked first because its store is the one whose
    /// files are named by agent-day: a session created by the loop is
    /// found through its store's own path cache and nowhere else, so
    /// asking the device-default store first would strand every append
    /// of an autonomous turn in the cross-device tree.
    pub(crate) fn store_for_session(&self, session_id: &str) -> &Arc<SessionStore> {
        if self
            .autonomous_session_store
            .absolute_path_for(session_id)
            .is_some()
        {
            &self.autonomous_session_store
        } else if self
            .device_default_session_store
            .absolute_path_for(session_id)
            .is_some()
        {
            &self.device_default_session_store
        } else {
            &self.cross_device_session_store
        }
    }

    /// Look up the MCP session id for `(namespace, project)`. Returns
    /// `None` if the project has never received a report.
    pub(crate) async fn mcp_session_for_project(
        &self,
        namespace: &str,
        project: &str,
    ) -> Option<String> {
        self.mcp_project_index
            .lock()
            .await
            .get(&(namespace.to_string(), project.to_string()))
            .cloned()
    }

    /// Look up (or create) the MCP session for `(namespace, project)`.
    /// First call for a project creates the underlying session file
    /// and registers it in the index; subsequent calls hit the index.
    /// Concurrent calls for the same new project are serialized
    /// through the index mutex so only one session file is created.
    pub(crate) async fn mcp_session_for_project_or_create(
        &self,
        namespace: &str,
        project: &str,
    ) -> anyhow::Result<String> {
        let key = (namespace.to_string(), project.to_string());
        {
            let idx = self.mcp_project_index.lock().await;
            if let Some(id) = idx.get(&key) {
                return Ok(id.clone());
            }
        }
        // Hold the lock across creation so two simultaneous
        // first-time writers for the same project don't each spawn
        // a session file. Double-check inside the lock in case
        // another task won the race between our two acquisitions.
        let mut idx = self.mcp_project_index.lock().await;
        if let Some(id) = idx.get(&key) {
            return Ok(id.clone());
        }
        let session_id = self
            .mcp_session_store
            .create_mcp_session(namespace, project)?;
        idx.insert(key, session_id.clone());
        Ok(session_id)
    }

    /// Provider that should serve the given session. Resolves the
    /// session's pinned room_profile to its `profile`, then to a
    /// concrete provider (with optional refusal fallback). Falls back
    /// to the background provider when no room_profile is pinned.
    pub(crate) async fn provider_for_session(&self, session_id: &str) -> Arc<dyn Provider> {
        let rp_name = self
            .session_room_profiles
            .lock()
            .await
            .get(session_id)
            .cloned();
        match rp_name.and_then(|n| self.config.room_profile(&n).map(|rp| rp.profile.clone())) {
            Some(profile_name) => self.registry.for_profile(&self.config, &profile_name),
            None => self.registry.background_provider(&self.config),
        }
    }
}

// ---------------------------------------------------------------------------
// JSON-RPC 2.0 types}

// ---------------------------------------------------------------------------
// JSON-RPC 2.0 types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct JsonRpcRequest {
    #[allow(dead_code)]
    jsonrpc: Option<String>,
    id: Option<Value>,
    method: String,
    params: Option<Value>,
}

fn error_response(id: Value, code: i32, message: &str) -> (StatusCode, axum::Json<Value>) {
    let body = json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message },
    });
    (StatusCode::OK, axum::Json(body))
}

fn notification_event(method: &'static str, params: Value) -> Event {
    let data = json!({
        "jsonrpc": "2.0",
        "method": method,
        "params": params,
    });
    Event::default().data(data.to_string())
}

fn result_event(id: &Value, result: Value) -> Event {
    let data = json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result,
    });
    Event::default().data(data.to_string())
}

fn error_event(id: &Value, code: i32, message: &str) -> Event {
    let data = json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message },
    });
    Event::default().data(data.to_string())
}

// ---------------------------------------------------------------------------
// Router entry point
// ---------------------------------------------------------------------------

/// `extra` mounts the ambient audio ingest router (`POST /audio/ingest`,
/// `POST /audio/hello`) alongside `/rpc`, when the subsystem started.
/// It is a plain `Router` — not a field on `ServeState` — because the
/// ingest endpoint owns its own state deliberately: it never starts an
/// LLM turn, so it has no use for the runtime `ServeState` exists to
/// serve. Both routers are already resolved to `Router<()>` via their
/// own `.with_state()` call, which is what makes `.merge` valid here
/// despite the two states having nothing in common.
pub async fn run(
    addr: String,
    state: Arc<ServeState>,
    extra: Option<axum::Router>,
) -> anyhow::Result<()> {
    // Routes are intentionally separated so future protocol endpoints
    // (`/mcp` for the MCP server in #79/#80) can be mounted alongside
    // `/rpc` without colliding with the methods (`chat`,
    // `initialize`, `voice/*`, …) that live here. The A2A protocol
    // endpoints below are mounted unconditionally — the handler refuses
    // requests when `[a2a].enabled = false` so we don't pay a route
    // table conditional but still preserve the opt-in semantic.
    let mut app = Router::new()
        .route("/rpc", post(rpc_post).get(rpc_get))
        .route("/a2a", post(a2a::handle_a2a_post))
        .route("/mcp", post(mcp::handle_mcp_post))
        .route("/acp", axum::routing::get(acp::handle_acp_ws))
        .route(
            "/.well-known/agent-card.json",
            axum::routing::get(a2a::handle_agent_card),
        )
        .layer(tower_http::cors::CorsLayer::permissive())
        .with_state(Arc::clone(&state));
    if let Some(extra) = extra {
        app = app.merge(extra);
    }

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    info!("sapphire-agent: API server listening on http://{addr}");
    spawn_session_residency_log(Arc::clone(&state));
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            if let Err(e) = tokio::signal::ctrl_c().await {
                error!("Failed to install Ctrl-C handler: {e}");
            }
            info!("HTTP server shutting down...");
        })
        .await?;
    Ok(())
}

/// What `state.sessions` is holding, for the idle-eviction decision that
/// has not been made yet.
///
/// `largest` is the point: a total cannot distinguish many accumulated
/// sessions from one very long one, and only the first of those is fixed
/// by dropping idle sessions.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SessionResidency {
    pub sessions: usize,
    pub messages: usize,
    pub text_bytes: usize,
    pub tool_result_bytes: usize,
    /// `(session_id, bytes)` for the heaviest single session.
    pub largest: Option<(String, usize)>,
}

impl ServeState {
    /// Holds `state.sessions` locked for the entire walk below, rather
    /// than cloning the map and computing off the clone. Cloning would
    /// allocate a second full copy of every session's history — exactly
    /// the thing this measurement exists to find out is too big, so a
    /// diagnostic must not be able to double the peak it is diagnosing.
    ///
    /// That is affordable because the walk does no I/O and no `.await`,
    /// and its per-element work is `String::len()`, which is O(1)
    /// (`String` stores its length rather than scanning bytes) — so the
    /// whole pass is linear over memory already resident, run once per
    /// 30-minute sweep. Even a million messages costs a few
    /// milliseconds of contention, twice an hour.
    ///
    /// This reasoning holds only while every per-element step here stays
    /// O(1). If a future field needs to walk bytes or hash content, that
    /// cost is no longer free under the lock and this needs a different
    /// shape (e.g. compute off a clone, or sample instead of scanning
    /// everything).
    pub(crate) async fn session_residency(&self) -> SessionResidency {
        let sessions = self.sessions.lock().await;
        let mut out = SessionResidency {
            sessions: sessions.len(),
            messages: 0,
            text_bytes: 0,
            tool_result_bytes: 0,
            largest: None,
        };
        for (id, history) in sessions.iter() {
            out.messages += history.len();
            let mut this_session = 0usize;
            for msg in history {
                for part in &msg.parts {
                    match part {
                        ContentPart::Text(t) => {
                            out.text_bytes += t.len();
                            this_session += t.len();
                        }
                        ContentPart::ToolResult { content, .. } => {
                            out.tool_result_bytes += content.len();
                            this_session += content.len();
                        }
                        // Tool inputs and images are not what this
                        // measurement is deciding about, and counting
                        // them would blur the two numbers that matter.
                        _ => {}
                    }
                }
            }
            if out.largest.as_ref().is_none_or(|(_, b)| this_session > *b) {
                out.largest = Some((id.clone(), this_session));
            }
        }
        out
    }
}

/// Log what `state.sessions` is holding, every half hour.
///
/// A rolling half-hour from process start, not wall-clock-aligned: the
/// loop just sleeps 1800s and checks again. Debug level — it is input
/// for a design decision (idle eviction), not an operational signal
/// anyone needs at info.
///
/// This timer used to also re-summarise every ACP session that had
/// grown since its last digest, so that summary could be injected into
/// other sessions' system prompts. Both halves are gone: a periodic
/// summary rewrote the system prompt out from under every provider's
/// prompt cache, and the model can now read the same thing on demand
/// through the `session_list` / `session_read` tools.
pub(crate) fn spawn_session_residency_log(state: Arc<ServeState>) {
    tokio::spawn(async move {
        let period = std::time::Duration::from_secs(1800);
        loop {
            tokio::time::sleep(period).await;

            let r = state.session_residency().await;
            debug!(
                "session residency: {} session(s), {} message(s), {} B text, \
                 {} B tool results; largest {}",
                r.sessions,
                r.messages,
                r.text_bytes,
                r.tool_result_bytes,
                match &r.largest {
                    Some((id, bytes)) => format!("{id} at {bytes} B"),
                    None => "none".to_string(),
                }
            );
        }
    });
}

// ---------------------------------------------------------------------------
// POST /rpc  — dispatch JSON-RPC methods
// ---------------------------------------------------------------------------

/// JSON-RPC error code returned when the Authorization header is present
/// but the bearer token does not resolve through `DeviceAuth`. Mirrors
/// `codes::AUTH_REQUIRED` used by `/a2a` and `/mcp` so the three protocol
/// surfaces stay symmetrical.
const RPC_AUTH_REQUIRED: i32 = -32001;

async fn rpc_post(
    State(state): State<Arc<ServeState>>,
    headers: HeaderMap,
    Json(req): Json<JsonRpcRequest>,
) -> impl IntoResponse {
    let session_id = headers
        .get("session-id")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    let req_id = req.id.clone().unwrap_or(Value::Null);

    // Bearer auth → room_profile reverse lookup. The token IS the
    // profile selector; clients no longer pass `room_profile` in
    // params. Missing/empty bearer → 401 at the HTTP layer (matches
    // /a2a); unknown token → JSON-RPC AUTH_REQUIRED.
    let bearer = match extract_bearer(&headers) {
        Some(b) => b,
        None => {
            return (StatusCode::UNAUTHORIZED, "missing bearer token").into_response();
        }
    };
    let (profile_name, authenticated_device_id) = match state.device_auth.resolve(&bearer) {
        Some(r) => (r.room_profile.to_string(), r.device.id.to_string()),
        None => {
            let body = error_response(req_id, RPC_AUTH_REQUIRED, "unknown or revoked bearer token");
            return body.into_response();
        }
    };

    match req.method.as_str() {
        "initialize" => {
            handle_initialize(state, req_id, req.params, session_id, profile_name).await
        }
        "chat" => handle_chat(state, req_id, req.params, session_id).await,
        "list_sessions" => handle_list_sessions(state, req_id).await,
        "get_session" => handle_get_session(state, req_id, session_id).await,
        "voice/config" => handle_voice_config(state, req_id, req.params).await,
        "voice/pipeline_run" => {
            handle_voice_pipeline_run(
                state,
                req_id,
                req.params,
                profile_name,
                authenticated_device_id,
            )
            .await
        }
        "voice/subscribe" => {
            handle_voice_subscribe(
                state,
                req_id,
                req.params,
                profile_name,
                authenticated_device_id,
            )
            .await
        }
        _ => {
            let body = error_response(req_id, -32601, "Method not found");
            body.into_response()
        }
    }
}

/// Extract a `Bearer <token>` from an `Authorization` header, trimming
/// whitespace.
///
/// Returns `None` when the header is absent, uses another scheme, or
/// carries an empty token — every one of which the endpoints treat as
/// "unauthenticated" rather than "malformed".
///
/// `pub(crate)` so `/rpc`, `/a2a`, `/mcp`, `/acp` and
/// `crate::ambient::ingest` share one set of parsing rules rather than
/// each duplicating them.
pub(crate) fn extract_bearer(headers: &HeaderMap) -> Option<String> {
    let value = headers.get(axum::http::header::AUTHORIZATION)?;
    let s = value.to_str().ok()?;
    let token = s
        .strip_prefix("Bearer ")
        .or_else(|| s.strip_prefix("bearer "))?;
    let trimmed = token.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

// ---------------------------------------------------------------------------
// GET /rpc  — Phase 2 placeholder (server→client push)
// ---------------------------------------------------------------------------

async fn rpc_get() -> impl IntoResponse {
    (
        StatusCode::METHOD_NOT_ALLOWED,
        "GET /rpc is reserved for Phase 2 server-initiated tool requests",
    )
}

// ---------------------------------------------------------------------------
// initialize
// ---------------------------------------------------------------------------

async fn handle_initialize(
    state: Arc<ServeState>,
    req_id: Value,
    params: Option<Value>,
    existing_header_session: Option<String>,
    profile_name: String,
) -> axum::response::Response {
    // `room_profile` is no longer accepted as a JSON-RPC param — the
    // bearer token resolved in `rpc_post` is the sole profile selector
    // (mirrors A2A / MCP). `DeviceAuth::resolve` only returns profile
    // names that exist in the config, so no extra validation is needed
    // here.

    // Resolve to an internal UUID session_id.
    // - Session-Id header: already a UUID (internal), use directly.
    // - params.session_id: must be a 7-char grain-id (public) or "new"/absent.
    let resolved: Option<String> = if let Some(uuid) = existing_header_session {
        // Header carries the internal UUID we issued — trust it directly.
        Some(uuid)
    } else {
        let param_id = params
            .as_ref()
            .and_then(|p| p["session_id"].as_str())
            .filter(|s| *s != "new")
            .map(|s| s.to_string());

        match param_id {
            None => None,
            Some(ref id) if id.len() == 7 => {
                match state.cross_device_session_store.find_by_public_id(id) {
                    Some(uuid) => Some(uuid),
                    None => {
                        let body = error_response(req_id, -32602, "Session not found");
                        return body.into_response();
                    }
                }
            }
            Some(_) => {
                let body = error_response(
                    req_id,
                    -32602,
                    "Invalid session id (expected 7-char grain-id)",
                );
                return body.into_response();
            }
        }
    };

    let (session_id, is_new) = match resolved {
        Some(id) => {
            // Existence only — `load_session` would parse the file,
            // prepend the compaction stub, hydrate every tool result out
            // of the cache, and run `repair_tool_pairing`'s two passes,
            // none of which this needs. `load_session` returns `Some` for
            // any file whose meta line parses, which is exactly what
            // `absolute_path_for` (a `resolve_path` lookup) already
            // answers.
            let exists = state
                .cross_device_session_store
                .absolute_path_for(&id)
                .is_some();
            (id, !exists)
        }
        None => (uuid::Uuid::now_v7().to_string(), true),
    };

    // For brand-new sessions, defer file creation until the first chat arrives.
    // Reserve the public_id now so the client can display it immediately.
    let public_id = if is_new {
        let pid = grain_id::GrainId::random().to_string();
        state
            .pending_sessions
            .lock()
            .await
            .insert(session_id.clone(), pid.clone());
        Some(pid)
    } else {
        // Existing session: load metadata to retrieve the stored public_id
        // and pre-load history into memory.
        let mut sessions = state.sessions.lock().await;
        sessions.entry(session_id.clone()).or_insert_with(|| {
            state
                .cross_device_session_store
                .load_session(&session_id)
                .unwrap_or_default()
        });
        // Look up the public_id from the existing file metadata.
        state
            .cross_device_session_store
            .list_sessions()
            .into_iter()
            .find(|m| m.session_id == session_id)
            .and_then(|m| m.public_id)
    };

    state
        .session_room_profiles
        .lock()
        .await
        .insert(session_id.clone(), profile_name.clone());

    // Optional `params.device = { name, description }` from sapphire-agent-cli /
    // other voice clients. We treat `name` as the device handle (e.g.
    // "living-room-speaker") and render the full room name server-side
    // — that way every voice client doesn't have to agree on a template
    // and the agent stays in control of how the metadata is presented.
    if let Some(device) = params.as_ref().and_then(|p| p.get("device")) {
        let device_name = device
            .get("name")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let device_description = device
            .get("description")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        if let Some(name) = device_name {
            let room_info = RoomInfo {
                name: format!("voice channel with {name}"),
                description: device_description,
                kind: "voice".to_string(),
            };
            state
                .session_room_metadata
                .lock()
                .await
                .insert(session_id.clone(), room_info);
        }
    }

    let mut result = json!({
        "session_id": session_id,
        "is_new": is_new,
    });
    if let Some(ref pub_id) = public_id {
        result["public_id"] = json!(pub_id);
    }
    if let Some(name) = state.session_room_profiles.lock().await.get(&session_id) {
        result["room_profile"] = json!(name);
    }

    let body = json!({
        "jsonrpc": "2.0",
        "id": req_id,
        "result": result,
    });

    let mut response = axum::response::Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/json")
        .header(
            "session-id",
            HeaderValue::from_str(&session_id).unwrap_or_else(|_| HeaderValue::from_static("")),
        )
        .body(axum::body::Body::from(body.to_string()))
        .unwrap();

    // Also set Session-Id in the response headers (accessible via response)
    response.headers_mut().insert(
        "session-id",
        HeaderValue::from_str(&session_id).unwrap_or_else(|_| HeaderValue::from_static("")),
    );

    response
}

// ---------------------------------------------------------------------------
// chat  — returns SSE stream
// ---------------------------------------------------------------------------

async fn handle_chat(
    state: Arc<ServeState>,
    req_id: Value,
    params: Option<Value>,
    session_id: Option<String>,
) -> axum::response::Response {
    let session_id = match session_id {
        Some(id) => id,
        None => {
            // No session: return JSON error
            let body = error_response(req_id, -32602, "Missing Session-Id header");
            return body.into_response();
        }
    };

    let content = match params.as_ref().and_then(|p| p["content"].as_str()) {
        Some(c) => c.to_string(),
        None => {
            let body = error_response(req_id, -32602, "Missing params.content");
            return body.into_response();
        }
    };

    // Clients may opt into out-of-band TTS by setting
    // `modalities: ["text", "audio"]`. Default = text-only (existing
    // behaviour preserved for the CLI REPL / one-shot / channel paths).
    // Unknown modality strings are silently dropped — additive future
    // modalities won't break older clients that don't recognise them.
    let want_audio = params
        .as_ref()
        .and_then(|p| p.get("modalities"))
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().any(|m| m.as_str() == Some("audio")))
        .unwrap_or(false);

    let (tx, rx) = mpsc::channel::<Result<Event, Infallible>>(32);

    // Spawn the turn processor
    tokio::spawn(async move {
        run_turn(state, session_id, content, want_audio, req_id, tx).await;
    });

    let stream = ReceiverStream::new(rx);
    Sse::new(stream)
        .keep_alive(KeepAlive::new().interval(std::time::Duration::from_secs(15)))
        .into_response()
}

// ---------------------------------------------------------------------------
// list_sessions
// ---------------------------------------------------------------------------

async fn handle_list_sessions(state: Arc<ServeState>, req_id: Value) -> axum::response::Response {
    let metas = state.cross_device_session_store.list_sessions();
    let items: Vec<Value> = metas
        .into_iter()
        .map(|m| {
            let mut v = json!({
                "session_id": m.session_id,
                "created_at": m.created_at,
            });
            if let Some(pub_id) = m.public_id {
                v["public_id"] = json!(pub_id);
            }
            if let Some(title) = m.title {
                v["title"] = json!(title);
            }
            v
        })
        .collect();

    let body = json!({
        "jsonrpc": "2.0",
        "id": req_id,
        "result": { "sessions": items },
    });
    (StatusCode::OK, axum::Json(body)).into_response()
}

// ---------------------------------------------------------------------------
// get_session  — returns stored messages for the current session
// ---------------------------------------------------------------------------

async fn handle_get_session(
    state: Arc<ServeState>,
    req_id: Value,
    session_id: Option<String>,
) -> axum::response::Response {
    let session_id = match session_id {
        Some(id) => id,
        None => {
            let body = error_response(req_id, -32602, "Missing Session-Id header");
            return body.into_response();
        }
    };

    // `load_session` is the model's view — a compaction checkpoint trims
    // covered messages and prefixes a synthetic summary stub the client
    // never sent. This endpoint hands the record back to the client that
    // wrote it, so it must read the untrimmed, unsummarised record instead.
    let messages = state
        .store_for_session(&session_id)
        .load_session_full(&session_id)
        .map(|(messages, _summary)| messages)
        .unwrap_or_default();

    let items: Vec<Value> = messages
        .iter()
        .map(|m| {
            let role = match m.role {
                crate::provider::Role::User => "user",
                crate::provider::Role::Assistant => "assistant",
            };
            let parts: Vec<Value> = m
                .parts
                .iter()
                .map(|p| match p {
                    ContentPart::Text(t) => json!({ "type": "text", "text": t }),
                    ContentPart::Image { media_type, .. } => {
                        // Image bytes are not exposed via the RPC listing; surface a marker only.
                        json!({ "type": "image", "media_type": media_type })
                    }
                    ContentPart::ImageRef { media_type, sha256 } => {
                        // Same shape as Image, with the cache key surfaced so
                        // a caller can later fetch the bytes out of band.
                        json!({ "type": "image", "media_type": media_type, "sha256": sha256 })
                    }
                    ContentPart::ToolUse { id, name, input } => {
                        json!({ "type": "tool_use", "id": id, "name": name, "input": input })
                    }
                    ContentPart::ToolUseRef { id, name, sha256 } => {
                        // Same shape as tool_use, with the cache key
                        // surfaced instead of arguments a caller cannot
                        // be handed from a listing anyway.
                        json!({ "type": "tool_use", "id": id, "name": name, "sha256": sha256 })
                    }
                    ContentPart::ToolResult { tool_use_id, content } => {
                        json!({ "type": "tool_result", "tool_use_id": tool_use_id, "content": content })
                    }
                    ContentPart::ToolResultRef { tool_use_id, sha256 } => {
                        // Same shape as tool_result, with the cache key
                        // surfaced instead of content a caller cannot be
                        // handed from a listing anyway.
                        json!({ "type": "tool_result", "tool_use_id": tool_use_id, "sha256": sha256 })
                    }
                })
                .collect();
            json!({ "role": role, "parts": parts })
        })
        .collect();

    let body = json!({
        "jsonrpc": "2.0",
        "id": req_id,
        "result": { "messages": items },
    });
    (StatusCode::OK, axum::Json(body)).into_response()
}

// ---------------------------------------------------------------------------
// voice/pipeline_run  — STT → LLM turn → TTS, streamed via SSE
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// voice/config — return the room_profile's wake-word + (future)
// per-session voice settings so satellites can self-configure
// ---------------------------------------------------------------------------

async fn handle_voice_config(
    state: Arc<ServeState>,
    req_id: Value,
    _params: Option<Value>,
) -> axum::response::Response {
    use base64::Engine as _;
    use sha2::{Digest, Sha256};

    // Wake-word config is global — the same Saphina (or whatever)
    // greets the user across every room_profile. `params` is reserved
    // for a future per-profile override but currently unused.
    let mut result = json!({});
    if let Some(path_str) = &state.config.voice.wake_word_model {
        let expanded = shellexpand::tilde(path_str).into_owned();
        match std::fs::read(&expanded) {
            Ok(bytes) => {
                let mut hasher = Sha256::new();
                hasher.update(&bytes);
                let hash = hasher.finalize();
                use std::fmt::Write;
                let mut sha = String::with_capacity(64);
                for b in hash.iter() {
                    let _ = write!(&mut sha, "{b:02x}");
                }
                let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
                let filename = std::path::Path::new(&expanded)
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("wake.onnx")
                    .to_string();
                result["wake_word_model"] = json!({
                    "format": "onnx_inline",
                    "filename": filename,
                    "sha256": sha,
                    "data_b64": b64,
                });
            }
            Err(e) => {
                error!("voice/config: failed to read openWakeWord model '{expanded}': {e}");
                let body = error_response(
                    req_id,
                    -32603,
                    &format!("voice.wake_word_model '{expanded}' could not be read: {e}"),
                );
                return body.into_response();
            }
        }
    }

    let body = json!({
        "jsonrpc": "2.0",
        "id": req_id,
        "result": result,
    });
    (StatusCode::OK, axum::Json(body)).into_response()
}

async fn handle_voice_pipeline_run(
    state: Arc<ServeState>,
    req_id: Value,
    params: Option<Value>,
    room_profile: String,
    authenticated_device_id: String,
) -> axum::response::Response {
    if state.voice.is_none() {
        let body = error_response(
            req_id,
            -32601,
            "voice/pipeline_run unavailable: no STT/TTS providers configured",
        );
        return body.into_response();
    }

    let params = params.unwrap_or(Value::Null);
    let audio_b64 = match params["audio"].as_str() {
        Some(s) => s.to_string(),
        None => {
            let body = error_response(req_id, -32602, "Missing params.audio (base64 PCM)");
            return body.into_response();
        }
    };
    let device_id = match params["device_id"].as_str() {
        Some(s) => s.to_string(),
        None => {
            let body = error_response(req_id, -32602, "Missing params.device_id");
            return body.into_response();
        }
    };
    // The client-supplied `device_id` is only ever used to key sessions and
    // the voice-push registry — it is never itself an auth check. Without
    // this comparison, an authenticated client could claim another device's
    // id and read/write that device's session or displace its push channel
    // (`voice_subscribers` is keyed on `device_id` alone). `DeviceAuth`
    // already established which device this bearer token belongs to; hold
    // it to that.
    if device_id != authenticated_device_id {
        let body = error_response(
            req_id,
            -32602,
            "params.device_id does not match the authenticated device",
        );
        return body.into_response();
    }
    // `room_profile` comes from the bearer token resolved in
    // `rpc_post`; clients no longer pass it as a param.
    let language = params["language"].as_str().map(|s| s.to_string());

    // Resolve / lazily-create the device-default session for this
    // `(device_id, room_profile)` pair under that profile's memory
    // namespace. Daily rotation falls out naturally: a satellite
    // reconnecting after the day boundary finds yesterday's file as
    // "not in today's window" and a fresh UUID file is opened. See #122.
    let namespace = state
        .config
        .namespace_for_room_profile(&room_profile)
        .to_string();
    let session_id = match state
        .device_default_session_store
        .find_or_create_for_device(
            &device_id,
            &room_profile,
            &namespace,
            state.config.day_boundary_hour,
        ) {
        Ok(id) => id,
        Err(e) => {
            let body = error_response(
                req_id,
                -32603,
                &format!("failed to resolve device-default session: {e}"),
            );
            return body.into_response();
        }
    };
    state
        .session_room_profiles
        .lock()
        .await
        .insert(session_id.clone(), room_profile.clone());

    // Same `device` block accepted by `initialize` — refreshed on every
    // pipeline_run so satellites can update their description without a
    // separate handshake. Treated as room metadata for the session.
    if let Some(device) = params.get("device") {
        let device_name = device
            .get("name")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let device_description = device
            .get("description")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        if let Some(name) = device_name {
            let room_info = RoomInfo {
                name: format!("voice channel with {name}"),
                description: device_description,
                kind: "voice".to_string(),
            };
            state
                .session_room_metadata
                .lock()
                .await
                .insert(session_id.clone(), room_info);
        }
    }

    let (tx, rx) = mpsc::channel::<Result<Event, Infallible>>(64);

    let device_id_for_timer = device_id.clone();
    tokio::spawn(async move {
        run_voice_turn(
            state,
            session_id,
            audio_b64,
            language,
            req_id,
            tx,
            Some(device_id_for_timer),
        )
        .await;
    });

    let stream = ReceiverStream::new(rx);
    Sse::new(stream)
        .keep_alive(KeepAlive::new().interval(std::time::Duration::from_secs(15)))
        .into_response()
}

// ---------------------------------------------------------------------------
// voice/subscribe — long-lived SSE for server→satellite voice pushes
// ---------------------------------------------------------------------------

async fn handle_voice_subscribe(
    state: Arc<ServeState>,
    req_id: Value,
    params: Option<Value>,
    room_profile: String,
    authenticated_device_id: String,
) -> axum::response::Response {
    let params = params.unwrap_or(Value::Null);
    let device_id = match params["device_id"].as_str() {
        Some(s) => s.to_string(),
        None => {
            let body = error_response(req_id, -32602, "Missing params.device_id");
            return body.into_response();
        }
    };
    // See the identical check in `handle_voice_pipeline_run`: without it, an
    // authenticated client could claim another device's id and displace its
    // push channel, since `voice_subscribers` is keyed on `device_id` alone.
    if device_id != authenticated_device_id {
        let body = error_response(
            req_id,
            -32602,
            "params.device_id does not match the authenticated device",
        );
        return body.into_response();
    }
    // `room_profile` comes from the bearer token resolved in
    // `rpc_post`; clients no longer pass it as a param.

    // Replace any prior subscription for this device (typical case:
    // the same satellite reconnects after a brief network blip). The
    // old sender is dropped; its writer task exits on the first
    // failed send. The room_profile may also have changed across
    // reconnect, so the freshest value wins — that's the satellite's
    // current binding.
    let (push_tx, push_rx) = mpsc::channel::<crate::voice::VoicePushItem>(32);
    {
        let mut subs = state.voice_subscribers.lock().await;
        subs.insert(device_id.clone(), (room_profile.clone(), push_tx));
    }
    info!("voice/subscribe: registered (device={device_id}, room_profile={room_profile})");

    let (sse_tx, sse_rx) = mpsc::channel::<Result<Event, Infallible>>(32);
    let cleanup_state = Arc::clone(&state);
    let cleanup_device = device_id.clone();
    tokio::spawn(async move {
        translate_voice_pushes(push_rx, sse_tx).await;
        // SSE writer exited (satellite disconnected or push channel
        // closed). Remove the subscriber entry — but only if it still
        // points at our (now-dropped) sender, since a subsequent
        // reconnect may have already replaced it.
        let mut subs = cleanup_state.voice_subscribers.lock().await;
        if subs
            .get(&cleanup_device)
            .map(|(_, tx)| tx.is_closed())
            .unwrap_or(false)
            && let Some((rp, _)) = subs.remove(&cleanup_device)
        {
            info!("voice/subscribe: unregistered (device={cleanup_device}, room_profile={rp})");
        }
    });

    let stream = ReceiverStream::new(sse_rx);
    Sse::new(stream)
        .keep_alive(KeepAlive::new().interval(std::time::Duration::from_secs(15)))
        .into_response()
}

/// Forward [`VoicePushItem`]s from the per-subscriber mpsc channel into
/// SSE notification events. Exits when either the push channel closes
/// (server cleanup) or the SSE channel closes (client disconnect).
async fn translate_voice_pushes(
    mut push_rx: mpsc::Receiver<crate::voice::VoicePushItem>,
    sse_tx: mpsc::Sender<Result<Event, Infallible>>,
) {
    use base64::Engine;
    while let Some(item) = push_rx.recv().await {
        let evt = match item {
            crate::voice::VoicePushItem::Start { task } => {
                let mut params = json!({"kind": "push_start"});
                if let Some(t) = task {
                    params["task"] = json!(t);
                }
                notification_event("notifications/voice_push", params)
            }
            crate::voice::VoicePushItem::AssistantText(text) => notification_event(
                "notifications/voice_push",
                json!({"kind": "assistant_text", "text": text}),
            ),
            crate::voice::VoicePushItem::AudioChunk(pcm) => {
                let bytes: Vec<u8> = pcm.iter().flat_map(|s| s.to_le_bytes()).collect();
                let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
                notification_event(
                    "notifications/voice_push",
                    json!({"kind": "audio_chunk", "data": b64}),
                )
            }
            crate::voice::VoicePushItem::Done => {
                notification_event("notifications/voice_push", json!({"kind": "push_done"}))
            }
            crate::voice::VoicePushItem::Error(message) => notification_event(
                "notifications/voice_push",
                json!({"kind": "error", "message": message}),
            ),
        };
        if sse_tx.send(Ok(evt)).await.is_err() {
            break;
        }
    }
}

async fn run_voice_turn(
    state: Arc<ServeState>,
    session_id: String,
    audio_b64: String,
    language: Option<String>,
    req_id: Value,
    tx: mpsc::Sender<Result<Event, Infallible>>,
    device_id: Option<String>,
) {
    use base64::Engine;

    let send = |evt: Event| {
        let tx = tx.clone();
        async move {
            let _ = tx.send(Ok(evt)).await;
        }
    };

    // Resolve voice pipeline (need STT here; from_text resolves TTS again).
    let pipeline = match resolve_voice_pipeline(&state, &session_id).await {
        Ok(p) => p,
        Err(VoicePipelineLookup::NoVoice) => {
            send(error_event(
                &req_id,
                -32601,
                "voice/pipeline_run unavailable: no STT/TTS providers configured",
            ))
            .await;
            return;
        }
        Err(VoicePipelineLookup::NotConfigured) => {
            send(error_event(
                &req_id,
                -32602,
                "Session's room_profile has no voice_pipeline configured",
            ))
            .await;
            return;
        }
    };
    let voice_registry = state.voice.as_ref().expect("checked above").clone();
    let stt = match voice_registry.stt(&pipeline.stt_provider) {
        Some(p) => p,
        None => {
            send(error_event(
                &req_id,
                -32603,
                &format!("stt_provider '{}' not instantiated", pipeline.stt_provider),
            ))
            .await;
            return;
        }
    };

    // Decode audio.
    let audio_bytes = match base64::engine::general_purpose::STANDARD.decode(audio_b64.as_bytes()) {
        Ok(b) => b,
        Err(e) => {
            send(error_event(
                &req_id,
                -32602,
                &format!("Invalid base64 audio: {e}"),
            ))
            .await;
            return;
        }
    };
    if audio_bytes.len() % 2 != 0 {
        send(error_event(
            &req_id,
            -32602,
            "Audio byte length is not a multiple of 2 (expected s16le)",
        ))
        .await;
        return;
    }
    let pcm: Vec<i16> = audio_bytes
        .chunks_exact(2)
        .map(|c| i16::from_le_bytes([c[0], c[1]]))
        .collect();

    // Stage: STT
    info!(
        "voice/pipeline_run: STT via '{}' ({} samples, lang={:?})",
        stt.name(),
        pcm.len(),
        language.as_deref().or(pipeline.language.as_deref()),
    );
    send(notification_event(
        "notifications/progress",
        json!({"kind": "stage", "stage": "stt", "status": "start"}),
    ))
    .await;
    let lang = language.as_deref().or(pipeline.language.as_deref());
    let transcript = match stt.transcribe(&pcm, lang).await {
        Ok(t) => t,
        Err(e) => {
            error!("STT failed: {e:#}");
            send(error_event(&req_id, -32603, &format!("STT failed: {e}"))).await;
            return;
        }
    };
    send(notification_event(
        "notifications/progress",
        json!({"kind": "stt_final", "text": transcript}),
    ))
    .await;
    send(notification_event(
        "notifications/progress",
        json!({"kind": "stage", "stage": "stt", "status": "end"}),
    ))
    .await;

    // Hand off to the from-text path for everything past STT.
    run_voice_turn_from_text_sse(state, session_id, transcript, req_id, tx, device_id).await;
}

/// Voice pipeline failure when looking up the per-session config.
enum VoicePipelineLookup {
    NoVoice,
    NotConfigured,
}

async fn resolve_voice_pipeline(
    state: &Arc<ServeState>,
    session_id: &str,
) -> Result<crate::config::VoicePipelineConfig, VoicePipelineLookup> {
    if state.voice.is_none() {
        return Err(VoicePipelineLookup::NoVoice);
    }
    let rp_name = state
        .session_room_profiles
        .lock()
        .await
        .get(session_id)
        .cloned();
    rp_name
        .as_deref()
        .and_then(|n| state.config.voice_pipeline_for_room_profile(n))
        .cloned()
        .ok_or(VoicePipelineLookup::NotConfigured)
}

/// LLM turn + TTS streaming, with progress emitted as SSE notifications
/// for the original `voice/pipeline_run` caller. The final JSON-RPC
/// result event ends the stream.
async fn run_voice_turn_from_text_sse(
    state: Arc<ServeState>,
    session_id: String,
    user_text: String,
    req_id: Value,
    tx: mpsc::Sender<Result<Event, Infallible>>,
    device_id: Option<String>,
) {
    use base64::Engine;

    let send = |evt: Event| {
        let tx = tx.clone();
        async move {
            let _ = tx.send(Ok(evt)).await;
        }
    };

    let pipeline = match resolve_voice_pipeline(&state, &session_id).await {
        Ok(p) => p,
        Err(VoicePipelineLookup::NoVoice) => {
            send(error_event(
                &req_id,
                -32601,
                "voice unavailable: no STT/TTS providers configured",
            ))
            .await;
            return;
        }
        Err(VoicePipelineLookup::NotConfigured) => {
            send(error_event(
                &req_id,
                -32602,
                "Session's room_profile has no voice_pipeline configured",
            ))
            .await;
            return;
        }
    };
    let voice_registry = state.voice.as_ref().expect("checked above").clone();
    let tts = match voice_registry.tts(&pipeline.tts_provider) {
        Some(p) => p,
        None => {
            send(error_event(
                &req_id,
                -32603,
                &format!("tts_provider '{}' not instantiated", pipeline.tts_provider),
            ))
            .await;
            return;
        }
    };

    // Stage: LLM (intent)
    send(notification_event(
        "notifications/progress",
        json!({"kind": "stage", "stage": "intent", "status": "start"}),
    ))
    .await;
    let outcome = run_llm_turn(
        Arc::clone(&state),
        session_id.clone(),
        ChatMessage::user_voice(&user_text),
        Arc::new(SseProgress::new(tx.clone(), req_id.clone())),
        device_id
            .clone()
            .map(|d| crate::timer::TimerOrigin::Voice { device_id: d }),
    )
    .await;
    send(notification_event(
        "notifications/progress",
        json!({"kind": "stage", "stage": "intent", "status": "end"}),
    ))
    .await;
    let reply_text = match outcome.text {
        Some(t) => t,
        None => {
            // run_llm_turn already emitted a provider error_event.
            return;
        }
    };
    send(notification_event(
        "notifications/progress",
        json!({"kind": "assistant_text", "text": reply_text}),
    ))
    .await;

    // Stage: TTS
    info!(
        "voice/pipeline_run: TTS via '{}' ({} chars)",
        tts.name(),
        reply_text.len(),
    );
    send(notification_event(
        "notifications/progress",
        json!({"kind": "stage", "stage": "tts", "status": "start"}),
    ))
    .await;
    let (pcm_tx, mut pcm_rx) = mpsc::channel::<Vec<i16>>(32);
    let reply_for_tts = reply_text.clone();
    let synth_handle =
        tokio::spawn(async move { tts.synthesize_stream(&reply_for_tts, pcm_tx).await });
    let mut chunks_emitted = 0usize;
    while let Some(chunk) = pcm_rx.recv().await {
        let bytes: Vec<u8> = chunk.iter().flat_map(|s| s.to_le_bytes()).collect();
        let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
        send(notification_event(
            "notifications/progress",
            json!({"kind": "audio_chunk", "data": b64}),
        ))
        .await;
        chunks_emitted += 1;
    }
    // Surface TTS failures to the client — without this the satellite
    // saw a silent "no audio_chunks" stream and assumed playback was
    // empty, which looked like a text-only reply.
    match synth_handle.await {
        Ok(Ok(())) => {
            if chunks_emitted == 0 {
                warn!(
                    "TTS returned no audio chunks (provider: {})",
                    pipeline.tts_provider
                );
                send(error_event(
                    &req_id,
                    -32603,
                    &format!(
                        "TTS provider '{}' produced no audio (check fn_name / payload / audio_field)",
                        pipeline.tts_provider
                    ),
                ))
                .await;
                return;
            }
        }
        Ok(Err(e)) => {
            error!("TTS synthesis error: {e:#}");
            send(error_event(
                &req_id,
                -32603,
                &format!("TTS synthesis failed: {e:#}"),
            ))
            .await;
            return;
        }
        Err(join_err) => {
            error!("TTS task panicked: {join_err}");
            send(error_event(
                &req_id,
                -32603,
                &format!("TTS task panicked: {join_err}"),
            ))
            .await;
            return;
        }
    }
    send(notification_event(
        "notifications/progress",
        json!({"kind": "stage", "stage": "tts", "status": "end"}),
    ))
    .await;

    // Final result: transcript + reply text. Audio was streamed via
    // progress events; no need to duplicate it here.
    send(result_event(
        &req_id,
        json!({
            "transcript": user_text,
            "assistant_text": reply_text,
        }),
    ))
    .await;

    // Title generation on first turn — same as run_turn.
    if outcome.was_first_turn {
        let state2 = Arc::clone(&state);
        let sid = session_id.clone();
        let reply = reply_text.clone();
        tokio::spawn(async move {
            let p = state2.provider_for_session(&sid).await;
            if let Some(title) = generate_session_title(&*p, &user_text, &reply).await {
                let result = if state2.is_acp(&sid).await {
                    state2.acp_session_store.append_title(&sid, &title)
                } else {
                    state2.store_for_session(&sid).set_title(&sid, &title)
                };
                if let Err(e) = result {
                    warn!("Failed to store session title: {e}");
                }
            }
        });
    }
}

/// Failure modes for [`push_voice_text_to_subscriber`]. `Offline` lets
/// the heartbeat caller decide whether to fall back to a chat room.
pub enum VoicePushError {
    /// The server has no `[stt_provider.*]` / `[tts_provider.*]` blocks
    /// configured at all — voice push is fundamentally unavailable.
    NoVoice,
    /// The room_profile is unknown or has no `voice_pipeline` set.
    NotConfigured,
    /// No satellite is currently subscribed for this `(device_id,
    /// room_profile)` pair. Caller should fall back to chat if the
    /// heartbeat task has a `room_id`, or log and skip otherwise.
    Offline,
    /// Any other failure (TTS, LLM, etc.) surfaced for logging.
    Other(String),
}

/// Server-initiated voice push: run the LLM turn against the voice
/// session bound to `device_id` and stream the TTS audio to the
/// satellite subscribed via `voice/subscribe`.
///
/// The satellite supplied its current `room_profile` when it
/// subscribed — that value is the authoritative reverse index, so the
/// caller never has to duplicate it (a satellite's room_profile can
/// only change via a fresh subscription, which atomically replaces
/// the binding).
///
/// `task_name` becomes the heartbeat task identifier echoed in the
/// `PushStart` event so the satellite can label notifications.
pub(crate) async fn push_voice_text_to_subscriber(
    state: Arc<ServeState>,
    device_id: String,
    task_name: Option<String>,
    user_text: String,
) -> Result<(), VoicePushError> {
    // Look up the active subscription up front — if the satellite is
    // offline, surface that without burning an LLM call. The map also
    // tells us which room_profile the satellite is bound to.
    let (room_profile, push_tx) = {
        let subs = state.voice_subscribers.lock().await;
        match subs.get(&device_id) {
            Some((rp, tx)) => (rp.clone(), tx.clone()),
            None => return Err(VoicePushError::Offline),
        }
    };
    if state.config.room_profile(&room_profile).is_none() {
        return Err(VoicePushError::NotConfigured);
    }

    // Resolve / lazily-create the device-default session for this
    // `(device_id, room_profile)` pair under that profile's memory
    // namespace, then pin the room_profile so `resolve_voice_pipeline`
    // and `run_llm_turn` find the right config. See #122.
    let namespace = state
        .config
        .namespace_for_room_profile(&room_profile)
        .to_string();
    let session_id = state
        .device_default_session_store
        .find_or_create_for_device(
            &device_id,
            &room_profile,
            &namespace,
            state.config.day_boundary_hour,
        )
        .map_err(|e| VoicePushError::Other(format!("device-default lookup: {e}")))?;
    state
        .session_room_profiles
        .lock()
        .await
        .insert(session_id.clone(), room_profile.clone());

    let pipeline = match resolve_voice_pipeline(&state, &session_id).await {
        Ok(p) => p,
        Err(VoicePipelineLookup::NoVoice) => return Err(VoicePushError::NoVoice),
        Err(VoicePipelineLookup::NotConfigured) => return Err(VoicePushError::NotConfigured),
    };
    let voice_registry = state.voice.as_ref().ok_or(VoicePushError::NoVoice)?.clone();
    let tts = voice_registry.tts(&pipeline.tts_provider).ok_or_else(|| {
        VoicePushError::Other(format!(
            "tts_provider '{}' not instantiated",
            pipeline.tts_provider
        ))
    })?;

    // Notify the satellite that a push is starting so it can mute the
    // mic before the first audio chunk lands.
    let _ = push_tx
        .send(crate::voice::VoicePushItem::Start {
            task: task_name.clone(),
        })
        .await;

    // LLM turn (no SSE response channel — nobody watches this
    // heartbeat-injected turn's tool_start/tool_end progress).
    // Heartbeat-injected user line — synthesised by the timer pipeline,
    // not authored by a human, so no input modality applies.
    let injected_msg = ChatMessage {
        role: crate::provider::Role::User,
        parts: vec![crate::provider::ContentPart::Text(user_text.clone())],
        input_kind: None,
        user_id: None,
    };
    let outcome = run_llm_turn(
        Arc::clone(&state),
        session_id.clone(),
        injected_msg,
        Arc::new(NullProgress),
        Some(crate::timer::TimerOrigin::Voice {
            device_id: device_id.clone(),
        }),
    )
    .await;
    let reply_text = match outcome.text {
        Some(t) => t,
        None => {
            let msg = "LLM turn produced no text".to_string();
            let _ = push_tx
                .send(crate::voice::VoicePushItem::Error(msg.clone()))
                .await;
            let _ = push_tx.send(crate::voice::VoicePushItem::Done).await;
            return Err(VoicePushError::Other(msg));
        }
    };
    let _ = push_tx
        .send(crate::voice::VoicePushItem::AssistantText(
            reply_text.clone(),
        ))
        .await;

    // TTS: stream chunks to the subscriber as soon as they're synthesised.
    let (pcm_tx, mut pcm_rx) = mpsc::channel::<Vec<i16>>(32);
    let reply_for_tts = reply_text.clone();
    let synth_handle =
        tokio::spawn(async move { tts.synthesize_stream(&reply_for_tts, pcm_tx).await });
    let mut chunks_emitted = 0usize;
    while let Some(chunk) = pcm_rx.recv().await {
        if push_tx
            .send(crate::voice::VoicePushItem::AudioChunk(chunk))
            .await
            .is_err()
        {
            // Satellite disconnected mid-stream; abort the synth task.
            synth_handle.abort();
            return Err(VoicePushError::Offline);
        }
        chunks_emitted += 1;
    }
    match synth_handle.await {
        Ok(Ok(())) if chunks_emitted == 0 => {
            let msg = format!("TTS provider '{}' produced no audio", pipeline.tts_provider);
            warn!("{msg}");
            let _ = push_tx
                .send(crate::voice::VoicePushItem::Error(msg.clone()))
                .await;
            let _ = push_tx.send(crate::voice::VoicePushItem::Done).await;
            return Err(VoicePushError::Other(msg));
        }
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            let msg = format!("TTS synthesis failed: {e:#}");
            error!("{msg}");
            let _ = push_tx
                .send(crate::voice::VoicePushItem::Error(msg.clone()))
                .await;
            let _ = push_tx.send(crate::voice::VoicePushItem::Done).await;
            return Err(VoicePushError::Other(msg));
        }
        Err(join_err) => {
            let msg = format!("TTS task panicked: {join_err}");
            error!("{msg}");
            let _ = push_tx
                .send(crate::voice::VoicePushItem::Error(msg.clone()))
                .await;
            let _ = push_tx.send(crate::voice::VoicePushItem::Done).await;
            return Err(VoicePushError::Other(msg));
        }
    }

    let _ = push_tx.send(crate::voice::VoicePushItem::Done).await;
    Ok(())
}

// ---------------------------------------------------------------------------
// Turn processing (tool-calling loop)
// ---------------------------------------------------------------------------

/// Rewrite a user-role message so the model sees the input modality.
/// Voice transcripts are prefixed with `[voice input]` (English, since
/// the model is more reliable with English meta-tags) so the assistant
/// can treat the body as STT output rather than typed text. Text and
/// modality-less messages pass through unchanged.
fn apply_input_kind_label(mut msg: ChatMessage) -> ChatMessage {
    let prefix = match &msg.input_kind {
        Some(UserInputKind::Voice) => "[voice input]\n",
        _ => return msg,
    };
    for part in msg.parts.iter_mut() {
        if let ContentPart::Text(s) = part {
            *s = format!("{prefix}{s}");
            return msg;
        }
    }
    msg.parts
        .insert(0, ContentPart::Text(prefix.trim_end().to_string()));
    msg
}

/// Where a turn reports its per-tool progress — and who it asks before
/// running a tool the policy wants asked about.
///
/// `run_llm_turn` is the shared executor behind `/rpc`, the voice pipeline,
/// A2A and the ACP endpoint — but each caller wants its
/// `tool_start`/`tool_end`/error notifications shaped differently: `/rpc`
/// and voice relay them as JSON-RPC notifications over SSE, ACP sends them
/// as `session/update` notifications instead, and some callers (voice
/// heartbeats, A2A) don't surface intermediate progress at all. Putting
/// reporting behind this trait keeps the turn executor itself agnostic to
/// which of those shapes (if any) is listening.
///
/// Named `TurnHost` rather than `TurnProgress` because of the second
/// job: `origin` and `approve` ask rather than report. Both carry
/// defaults, so a transport with no way to reach a human implements
/// neither and keeps behaving exactly as it did.
#[async_trait::async_trait]
pub(crate) trait TurnHost: Send + Sync {
    /// A tool call is about to run, carrying the arguments the model
    /// asked for.
    ///
    /// `input` is the provider's own [`crate::provider::ToolCall::input`],
    /// threaded through so a transport that can show details has them at
    /// hand — ACP reports it as the tool call's `rawInput`. Transports
    /// with no such field (the SSE `{id, name}` shape) ignore it.
    async fn tool_start(&self, id: &str, name: &str, input: &Value);
    async fn tool_end(&self, id: &str, name: &str);
    async fn turn_error(&self, message: &str);

    /// One piece of the model's prose, as soon as the round that produced
    /// it is done.
    ///
    /// Default no-op, and that default is load-bearing: `/rpc`, `/a2a` and
    /// the voice pipeline read the turn's whole reply from
    /// `LlmTurnOutcome::text` and are correct as they are. TTS in
    /// particular must not be handed a turn in pieces — it would speak the
    /// narration between tool calls as if it were the answer.
    ///
    /// Called for every non-empty text a round produces, including the
    /// final tool-less one, so a host that implements this sees the
    /// turn's prose in order and needs nothing from `outcome.text`.
    /// `SubagentHost` deliberately does *not* forward it — see
    /// its doc.
    async fn message_chunk(&self, _text: &str) {}

    /// Which round budget this turn is judged by. See [`RoundBudget`].
    ///
    /// `Unattended` by default, which is the safe direction: a host that
    /// forgets to answer gets the bounded budget, not the unbounded one.
    fn round_budget(&self) -> RoundBudget {
        RoundBudget::Unattended
    }

    /// Which row of the permission table this turn is judged by.
    ///
    /// `Trusted` by default: `/rpc`, `/a2a` and the voice pipeline were
    /// authenticated before the turn started and have no UI to ask
    /// through, so they must keep running everything.
    ///
    /// The heartbeat only reaches this default on its *voice* leg. Its
    /// chat leg goes through `Agent::handle_message`, which does not use
    /// `TurnHost` at all and judges itself `Origin::Channel`.
    fn origin(&self) -> crate::tools::policy::Origin {
        crate::tools::policy::Origin::Trusted
    }

    /// The editor on the other end, when there is one.
    ///
    /// `None` by default: `/rpc`, `/a2a`, Matrix, Discord and the voice
    /// pipeline have no client machine to reach. The client-side tools
    /// read this through a task-local and refuse when it is absent, so
    /// a default of `None` is what keeps them ACP-only.
    fn acp_client(&self) -> Option<Arc<dyn crate::tools::acp_client::AcpClient>> {
        None
    }

    /// The session's working directory, when this turn has one: ACP
    /// sessions carry the `cwd` the editor sent in `session/new` (or
    /// `load`/`resume`). `run_llm_turn` passes it to
    /// `Workspace::build_system_prompt`, which injects it into the system
    /// prompt verbatim — verbatim on purpose: the path names a location on
    /// the *client's* machine, so this server can neither resolve nor
    /// validate it.
    ///
    /// `None` by default: `/rpc`, `/a2a`, Matrix, Discord and the voice
    /// pipeline have no client working directory to inject.
    fn cwd(&self) -> Option<String> {
        None
    }

    /// The editor's declared file-system capabilities for this turn, as
    /// `(read, write)`. A client can implement `fs/read_text_file`
    /// without `fs/write_text_file` or vice versa, so the two are read
    /// independently rather than folded into one "fs" bit — see
    /// `visible_tool_predicate`, which gates the shared `file_read`/
    /// `file_write`/`file_append` names on them inside an ACP turn.
    ///
    /// `(false, false)` by default: every host with no editor on the
    /// other end (`/rpc`, `/a2a`, Matrix, Discord, voice) has nothing
    /// to ask this of.
    fn client_fs_caps(&self) -> (bool, bool) {
        (false, false)
    }

    /// Whether this turn's editor implements `terminal/*` — the
    /// capability the shell and delete names need to be worth offering at
    /// all. Same convention as `client_fs_caps`:
    /// read off `AcpSession::client_capabilities` and exposed here so
    /// `visible_tool_predicate` doesn't reach into ACP-specific state.
    ///
    /// `false` by default: every host with no editor on the other end
    /// has nothing to ask this of.
    fn client_terminal_cap(&self) -> bool {
        false
    }

    /// This call cleared the gate and is about to run.
    ///
    /// Separate from `tool_start`, which fires *before* the gate and so
    /// cannot say whether the call will run, wait on a human, or be
    /// refused. A host that draws a status needs both edges; one that
    /// does not implements neither.
    async fn tool_allowed(&self, id: &str) {
        let _ = id;
    }

    /// Called only when `decide` returned `Ask`.
    ///
    /// The default answers `AllowOnce` — a host that never returns an
    /// asking `origin()` can never reach this, so the default exists
    /// for safety rather than for use.
    async fn approve(
        &self,
        call: &crate::provider::ToolCall,
        kind: crate::tools::ToolKind,
    ) -> crate::tools::policy::Approval {
        let _ = (call, kind);
        crate::tools::policy::Approval::AllowOnce
    }
}

/// Builds the `{id, name}` params shared by the `tool_start`/`tool_end`
/// wire notifications. Pulled out so tests can pin the field names
/// directly without inspecting an opaque SSE `Event`.
fn tool_event_params(id: &str, name: &str) -> Value {
    json!({ "id": id, "name": name })
}

/// The `/rpc` and voice shape: JSON-RPC notifications (and, on provider
/// failure, a JSON-RPC error) delivered over the SSE channel — exactly
/// what `run_llm_turn` used to emit inline before progress reporting moved
/// behind [`TurnHost`].
pub(crate) struct SseProgress {
    tx: mpsc::Sender<Result<Event, Infallible>>,
    req_id: Value,
}

impl SseProgress {
    pub(crate) fn new(tx: mpsc::Sender<Result<Event, Infallible>>, req_id: Value) -> Self {
        Self { tx, req_id }
    }
}

#[async_trait::async_trait]
impl TurnHost for SseProgress {
    /// `_input` is ignored: the SSE payload is pinned to `{id, name}` and
    /// predates the argument being available here.
    async fn tool_start(&self, id: &str, name: &str, _input: &Value) {
        let _ = self
            .tx
            .send(Ok(notification_event(
                "tool_start",
                tool_event_params(id, name),
            )))
            .await;
    }

    async fn tool_end(&self, id: &str, name: &str) {
        let _ = self
            .tx
            .send(Ok(notification_event(
                "tool_end",
                tool_event_params(id, name),
            )))
            .await;
    }

    async fn turn_error(&self, message: &str) {
        let _ = self
            .tx
            .send(Ok(error_event(&self.req_id, -32603, message)))
            .await;
    }
}

/// Discard progress. Used by callers that drive a turn to completion with
/// nobody watching intermediate events (voice heartbeats, A2A v1).
pub(crate) struct NullProgress;

#[async_trait::async_trait]
impl TurnHost for NullProgress {
    async fn tool_start(&self, _id: &str, _name: &str, _input: &Value) {}
    async fn tool_end(&self, _id: &str, _name: &str) {}
    async fn turn_error(&self, _message: &str) {}
}

/// The host for an autonomous turn.
///
/// Deliberately tiny: it carries the permission-table row and nothing
/// else. Everything an autonomous session *is* — the system prompt, the
/// tools, the persistence — is decided by `run_llm_turn` and the
/// configuration, exactly as it is for any other session. This type
/// exists so that the day the workspace-access policy lands (design doc,
/// decision 10) there is one place to teach about a directory scope
/// rather than four.
///
/// `round_budget` is *not* implemented: `Unattended` is the correct
/// answer, because nobody can cancel an autonomous turn in flight, and
/// the default is already that.
pub(crate) struct AutonomousHost {
    pub(crate) origin: crate::tools::policy::Origin,
}

#[async_trait::async_trait]
impl TurnHost for AutonomousHost {
    async fn tool_start(&self, _id: &str, _name: &str, _input: &Value) {}
    async fn tool_end(&self, _id: &str, _name: &str) {}
    async fn turn_error(&self, message: &str) {
        warn!("Autonomous turn error: {message}");
    }

    fn origin(&self) -> crate::tools::policy::Origin {
        self.origin
    }
}

/// Which round budget a turn is judged by.
///
/// A property of the *route*, not of the request: what separates the two
/// is whether a human can stop a turn that has gone wrong. Only ACP can
/// (`session/cancel`), so only `AcpProgress` returns `Interactive`.
///
/// Deliberately not a number. A host knows which kind of route it is; it
/// does not know what the operator configured, and threading the config
/// through every implementor to let each read the same field would put
/// the same decision in four places. `TurnLoop::run` resolves this
/// against `[tools.tool_rounds]` in one place instead — the same shape
/// `TurnHost::origin` uses for the permission table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RoundBudget {
    /// The turn can be cancelled in flight. ACP only.
    Interactive,
    /// It cannot. Everything else, and every subagent.
    Unattended,
}

/// Why a turn stopped.
///
/// Exists because `text: None` conflates two materially different endings:
/// the provider broke, and the model was still working when its tool-round
/// budget ran out. A transport that has a way to say "budget" — ACP's
/// `StopReason::MaxTurnRequests` — cannot tell them apart from the text
/// alone, and answering "internal error" for a turn that was merely long is
/// wrong in a way the user sees.
///
/// Callers that don't care may keep reading `text` alone: this is an extra
/// field on the outcome, not a replacement, and every existing caller
/// ignores it.
pub(crate) enum TurnStop {
    /// The model produced its final message; `text` is `Some`.
    Replied,
    /// A `Provider::chat` call failed; `text` is `None` and `message` is
    /// the error's `Display` (`e.to_string()`, the same text logged right
    /// before this is returned and handed to `TurnHost::turn_error`).
    /// Carried on the variant — not just logged — because
    /// `SubagentTool::execute` (`src/tools/subagent.rs`) runs a subagent's
    /// nested turn under `SubagentHost`, which deliberately swallows
    /// `turn_error` (see that type's doc), so the log line used to be the
    /// only place this reached: a subagent's `OpenRouter insufficient
    /// credit` or similar came back to the parent model as an
    /// indistinguishable-from-a-quiet-failure "[the subagent produced no
    /// answer]". `SubagentTool::run_and_store` reads `message` off this
    /// variant to surface it as the tool's own error instead.
    ProviderError { message: String },
    /// The turn's `[tools.tool_rounds]` budget was reached with the model
    /// still calling tools. `text` is `None` — deliberately, because every
    /// caller that predates ACP treats this as a failed turn and must
    /// keep doing so — but the prose the model emitted alongside its tool
    /// calls is real work, and is carried here rather than discarded. It
    /// may be empty.
    BudgetExhausted { partial_text: String },
}

/// Outcome of [`run_llm_turn`].
pub(crate) struct LlmTurnOutcome {
    /// Final assistant text, when the turn completed successfully. `None`
    /// on provider error or when the `[tools.tool_rounds]` budget was hit
    /// without resolving.
    pub(crate) text: Option<String>,
    /// True iff the session had no prior turns before this one. Used by
    /// callers to decide whether to spawn a title-generation task.
    pub(crate) was_first_turn: bool,
    /// Which of those endings this was. See [`TurnStop`].
    pub(crate) stop: TurnStop,
}

/// Which tools a turn's model may see.
///
/// The one function that decides this — `run_llm_turn` and its
/// filtering tests both call it, rather than each building their own
/// version of the same rule (see `ToolSet::specs_filtered`'s doc for
/// why an unusable tool is worse than an absent one). Also called from
/// `Agent::handle_message` (`src/agent.rs`) for Matrix/Discord turns,
/// with every client-capability flag forced `false` — there is no ACP
/// client on that path, so the client-side tools must never appear
/// there either.
///
/// Every name is matched directly rather than by prefix or `ToolKind`,
/// because after #270 the names are shared: `file_read` on an ACP turn and
/// `file_read` on a Matrix turn are the same registered tool, and only the
/// session decides which machine it reaches (see `builtin_tools`). The
/// three client flags apply only to the routed turn, where they are the
/// capabilities this turn's editor recorded at `initialize`
/// (`AcpSession::client_capabilities`, `src/serve/acp.rs`); a client can
/// implement `fs/read_text_file` without `fs/write_text_file` (or vice
/// versa), so those two are read independently.
///
/// The one gate that is not about capabilities is
/// `policy::host_tool_denied`, and #270 is what moved it inside: it now
/// receives `has_client` as its `routed_to_client`, so on an ACP turn the
/// seven unified names are decided by capability alone — the same seven
/// names on a non-ACP turn are decided by `host_access` alone, which is
/// the deployment switch's entire domain. `Agent::handle_message` calls
/// this with every client flag false for exactly that reason.
pub(crate) fn visible_tool_predicate(
    host_access_enabled: bool,
    has_client: bool,
    client_fs_read: bool,
    client_fs_write: bool,
    client_terminal: bool,
) -> impl Fn(&str) -> bool {
    move |name: &str| {
        // The host gate, and the one place #270 changed its meaning:
        // `has_client` is the `routed_to_client` argument. On an ACP turn
        // these calls are going to the *editor's* machine, which this
        // deployment's `[tools.host_access]` switch has no business
        // speaking for, so the gate declines to fire and the list below
        // is the whole answer. On a turn with no client (Matrix, Discord,
        // `/rpc`, voice, the heartbeat) the gate is exactly the old rule
        // and every name it lets through is the agent's own.
        if crate::tools::policy::host_tool_denied(name, host_access_enabled, has_client) {
            return false;
        }

        // The names with no agent-side body at all, which are therefore
        // never offered without a client no matter what the host switch
        // says: there is nothing on this machine for them to mean. The
        // three lifecycle tools have no agent-side handle store (see
        // `client_tools`), and every skill tool resolves its directory by
        // running a script on the editor (`src/tools/skill_tools.rs`) —
        // ACP has no list, glob or stat, so even the read-only `skill`
        // needs that terminal.
        if matches!(
            name,
            "shell_start"
                | "shell_output"
                | "shell_kill"
                | "skill"
                | "skill_install"
                | "skill_update"
                | "skill_uninstall"
        ) {
            return has_client && client_terminal;
        }

        // Everything else survived the gate. With no client that is the
        // whole rule — `host_access` is the only gate there is, and the
        // seven unified names it let through (`file_read`, `file_write`,
        // `file_append`, `file_delete`, `dir_list`, `dir_walk`, `shell`)
        // are the agent's own tools here; `dir_list`/`dir_walk` included,
        // since #270 gave them an agent-side body rather than leaving them
        // client-only.
        if !has_client {
            return true;
        }

        // With a client, one more narrowing: the same seven names reach
        // the editor, so a routable name may still be unusable — and the
        // editor said at `initialize` which of these it implements. A
        // client can declare `fs/read_text_file` without
        // `fs/write_text_file` (or vice versa), so those two flags are
        // read independently rather than folded into one "fs" bit.
        // Nothing falls back to the agent's own disk when a capability is
        // missing; the name is simply absent from this round's list.
        match name {
            // The unified set: one name each, whose *machine* this turn
            // decides (see `builtin_tools`).
            "file_read" => client_fs_read,
            "file_write" => client_fs_write,
            // Append is a read and a write on the client's side (read ->
            // concatenate -> write), so it needs both halves.
            "file_append" => client_fs_read && client_fs_write,
            // No ACP request exists for delete, list or walk, so all three
            // — and `shell` — are spelled as a command on the terminal.
            "file_delete" | "dir_list" | "dir_walk" | "shell" => client_terminal,
            _ => true,
        }
    }
}

/// Where a turn's messages go — or that they go nowhere.
///
/// `Option<TurnPersistence>` rather than a `bool` on the loop: a
/// subagent has no session, so there is no id to write to and no
/// half-persisted state to reason about. Making the absence a shape
/// rather than a flag means the loop cannot accidentally write to a
/// session that does not exist.
pub(crate) struct TurnPersistence {
    store: Arc<SessionStore>,
    acp_store: Arc<AcpSessionStore>,
    session_id: String,
    is_acp: bool,
}

impl TurnPersistence {
    /// Append one message. There is nothing here for a caller to gate a
    /// paired append on — that bookkeeping belongs to
    /// `append_message_paired`, whose doc explains it; this method's own
    /// caller (the final assistant message, which pairs with nothing)
    /// discards any return value, so there is none.
    fn append_message(&self, msg: &ChatMessage) {
        if self.is_acp {
            if let Err(e) = self.acp_store.append_message(&self.session_id, msg) {
                warn!("Failed to persist a message: {e}");
            }
        } else if let Err(e) = self.store.append(&self.session_id, msg) {
            warn!("Failed to persist a message: {e}");
        }
    }

    /// Append a `tool_use` or `tool_result` message. Returns whether the
    /// caller may go on to persist a message that must be paired with
    /// this one.
    ///
    /// `false` only when the append was attempted and failed. Every
    /// store persists tool traffic now (#194), so there is no longer a
    /// transport that skips this — what used to be the `is_acp` branch
    /// here always returned `true` for the four `SessionStore` kinds
    /// precisely because they wrote nothing at all.
    fn append_message_paired(&self, msg: &ChatMessage) -> bool {
        let result = if self.is_acp {
            self.acp_store.append_message(&self.session_id, msg)
        } else {
            self.store.append(&self.session_id, msg)
        };
        match result {
            Ok(()) => true,
            Err(e) => {
                warn!("Failed to persist a message: {e}");
                false
            }
        }
    }

    /// Append a compaction summary and the checkpoint it establishes.
    ///
    /// Unconditional now. ACP used to skip this on the grounds that its
    /// events already answer the question — they do, but with the whole
    /// session, so every reload replayed everything and re-paid for the
    /// same compaction on the first turn back.
    fn append_summary(&self, summary: &str, keep_recent: usize) {
        let result = if self.is_acp {
            self.acp_store
                .append_summary(&self.session_id, summary, keep_recent)
        } else {
            self.store
                .append_summary(&self.session_id, summary, keep_recent)
        };
        if let Err(e) = result {
            warn!("Failed to persist compaction summary: {e}");
        }
    }
}

/// What a delegating tool needs to run a nested conversation.
///
/// Carried the same way the ACP client handle is (`tools::acp_client`),
/// and for the same reason: `Tool::execute` receives only its JSON
/// input, and threading a turn through the `Tool` trait would touch
/// every tool for the benefit of one.
pub(crate) struct TurnContext {
    pub state: Arc<ServeState>,
    pub provider: Arc<dyn Provider>,
    pub progress: Arc<dyn TurnHost>,
    /// `Arc<[ToolSpec]>` rather than `Vec<ToolSpec>`: this round's own
    /// `tool_specs` doesn't change between rounds of the same turn, so
    /// `TurnLoop::run` builds this once per turn and `Arc::clone`s it
    /// per round rather than deep-cloning every `ToolSpec` — schemas
    /// included — on every round of every turn, whether or not any
    /// subagent is even defined.
    pub visible_specs: Arc<[ToolSpec]>,
    pub timer_origin: Option<crate::timer::TimerOrigin>,
    /// The session this round's turn is persisting to, or `None` for a
    /// subagent's nested turn (`persistence` is `None` there — see
    /// `TurnPersistence`'s doc). `tools::skill_tools::SkillTool` keys
    /// its per-editor resolved-index cache on this: the tool is
    /// registered once into the `ToolSet` shared by every connection
    /// through `ServeState`, so without a session key two different
    /// editors' `skill()` calls would collide on one cache entry.
    pub session_id: Option<String>,
    /// This turn's own subagent nesting depth. A main turn is `0`; a
    /// nested subagent turn is its delegator's depth + 1 (set by
    /// `SubagentTool::run_and_store`, which builds the nested `TurnLoop`
    /// with `subagent_depth: ctx.subagent_depth + 1`). Paired with
    /// `tools.subagent.max_depth`, this is what `subagent_tool_specs`
    /// uses to decide whether this turn may delegate further.
    pub subagent_depth: u32,
    /// The room profile this turn's admin-tool grant hangs on, or `None` on a
    /// transport that names no profile an operator could have allowed
    /// (`/rpc`, A2A, voice, the unattended loops). Set by `run_llm_turn` from
    /// the profile an ACP session pinned, and read through
    /// `config_tools::current_admin_room_profile_for` while a tool call runs.
    ///
    /// Copied straight into the nested `TurnLoop` a delegated subagent builds,
    /// so a delegation keeps the grant its parent ran under — the grant is the
    /// session's, not the caller's, the same way `timer_origin` is carried.
    /// A definition that must not hold it narrows its own `tools:` instead.
    pub admin_room_profile: Option<String>,
}

tokio::task_local! {
    static TURN_CONTEXT_TL: Arc<TurnContext>;
}

/// Run `fut` with a [`TurnContext`] reachable from `current_turn_context`.
pub(crate) fn scope_turn_context<F: std::future::Future>(
    ctx: Arc<TurnContext>,
    fut: F,
) -> impl std::future::Future<Output = F::Output> {
    TURN_CONTEXT_TL.scope(ctx, fut)
}

/// The context for the turn currently executing a tool call, if there is
/// one.
///
/// `None` outside `scope_turn_context` — which is what makes
/// `tools::subagent::SubagentTool` refuse on any path that is not a live
/// turn, rather than reaching for a model and a host that do not exist.
pub(crate) fn current_turn_context() -> Option<Arc<TurnContext>> {
    TURN_CONTEXT_TL.try_with(Arc::clone).ok()
}

/// One model conversation run to completion: call the model, run the
/// tools it asks for, repeat until it stops asking.
///
/// Extracted from `run_llm_turn` so a subagent can run the same loop
/// without a session behind it. Everything session-shaped lives in
/// `persistence`, which is `None` for a subagent — see
/// `TurnPersistence`.
///
/// `namespace` is not session state either, but the loop needs it to
/// scope the memory tool (`scope_memory_namespace`) for every tool call
/// it executes, so it travels alongside the other per-turn inputs
/// rather than through `persistence`.
pub(crate) struct TurnLoop<'a> {
    pub state: &'a Arc<ServeState>,
    pub provider: &'a Arc<dyn Provider>,
    pub system: Option<&'a str>,
    pub tool_specs: &'a [ToolSpec],
    pub progress: &'a Arc<dyn TurnHost>,
    pub timer_origin: Option<crate::timer::TimerOrigin>,
    pub namespace: String,
    pub persistence: Option<&'a TurnPersistence>,
    /// This loop's own nesting depth, copied straight into each round's
    /// `TurnContext` (see `TurnContext::subagent_depth`). A main turn
    /// builds its `TurnLoop` with `0`; a nested subagent turn with its
    /// delegator's depth + 1.
    pub subagent_depth: u32,
    /// The admin-tool grant this loop's tool calls are judged by. Copied into
    /// every round's `TurnContext` and scoped around tool execution the same
    /// way `timer_origin` is. See `TurnContext::admin_room_profile`.
    pub admin_room_profile: Option<String>,
}

impl TurnLoop<'_> {
    /// Run until the model stops calling tools, the round budget runs
    /// out, or the provider fails. `history` is both the input and
    /// where the conversation accumulates.
    pub(crate) async fn run(self, history: &mut Vec<ChatMessage>) -> (Option<String>, TurnStop) {
        let state = self.state;
        let provider: &dyn Provider = &**self.provider;
        let system = self.system;
        let tool_specs = self.tool_specs;
        let progress = self.progress;
        let namespace = self.namespace;
        // Read once per turn: `None` on every non-ACP transport, which
        // is what keeps the client-side tools refusing there rather
        // than reaching for a connection that does not exist.
        let host_access_enabled = state.config.tools.host_access.enabled;
        // Read once per turn, beside the switch and for the same reason it
        // is: this is what decides whether that switch speaks for this
        // turn at all. #270's core meaning change — an ACP turn's calls go
        // to the editor's machine, so the host gate must not fire for
        // them; a turn with no client gets exactly the old behaviour.
        // `visible_tool_predicate` applies the same rule to the *offered*
        // list, and the two must move together or a name is advertised and
        // then refused on every call.
        let routed_to_client = progress.acp_client().is_some();
        let compression_config = &state.config.compression;
        let mut accumulated_text: Vec<String> = Vec::new();
        // Built once per turn, not once per round: `tool_specs` is fixed
        // for the whole call to `run`, so cloning it into an
        // `Arc<[ToolSpec]>` here and `Arc::clone`-ing it into each
        // round's `TurnContext` below is a pointer copy instead of a
        // deep clone of every `ToolSpec` — schemas included — on every
        // round of every turn.
        let visible_specs: Arc<[ToolSpec]> = Arc::from(tool_specs);
        // Rounds this turn, not tool calls in `history`. `history` can
        // arrive already seeded with a restored session's prior tool
        // traffic, so counting the whole thing would spend a session's
        // entire budget on what it did before this turn started — and the
        // check runs before compaction, so nothing could ever trim it
        // back.
        // Resolved once per turn, not per round: the config cannot change
        // mid-turn, and reading it here keeps `RoundBudget` a routing
        // question rather than a numeric one. `None` is unbounded — the
        // check below simply never fires.
        let round_limit = state
            .config
            .tools
            .tool_rounds
            .limit(progress.round_budget());
        let mut round = 0usize;
        let (final_text, stop) = loop {
            if round_limit.is_some_and(|max| round >= max) {
                warn!("Reached max tool rounds ({round})");
                // `None` for the text, as before — callers that predate ACP
                // report this as a failed turn and must keep doing so — but the
                // prose accumulated so far rides along on the stop reason for
                // transports that can show partial work.
                break (
                    None,
                    TurnStop::BudgetExhausted {
                        partial_text: accumulated_text.join("\n\n"),
                    },
                );
            }

            // Check if context compression is needed
            match maybe_compress(
                provider,
                system,
                history,
                Some(tool_specs),
                compression_config,
            )
            .await
            {
                Ok(Some(result)) => {
                    // Persist the checkpoint so a reload starts the model's
                    // history from here instead of replaying the whole
                    // session and re-paying for this compaction. A trim-only
                    // pass carries no summary and moves no checkpoint: the
                    // log keeps the full text, and the trim is re-derived
                    // whenever it is needed again.
                    if let (Some(p), Some(summary)) = (self.persistence, result.summary.as_deref())
                    {
                        p.append_summary(summary, result.keep_recent);
                    }
                    *history = result.compressed;
                }
                Ok(None) => {}
                Err(e) => {
                    warn!("Context compression failed, continuing with full history: {e}");
                }
            }

            // Hydrate `ImageRef` parts from the image cache into full
            // `Image` parts for the provider call. `Image` parts (just
            // arrived this turn) and Text/Tool parts pass through;
            // `ImageRef` parts are intentionally degraded to text markers
            // so historical images aren't re-billed every turn (the cache
            // still retains the bytes for an on-demand recall tool).
            // After hydration, fold each user message's input modality
            // into a textual prefix so the model knows when a body is a
            // voice transcript (likely to contain STT errors).
            let history_for_provider: Vec<ChatMessage> =
                crate::image_cache::hydrate_history(history)
                    .into_iter()
                    .map(apply_input_kind_label)
                    .collect();
            // Measured against exactly what goes on the wire, hydration and
            // input-kind labels included — the response's own token count is
            // what teaches the estimate how wrong it is.
            let estimated =
                estimate_request_tokens(system, &history_for_provider, Some(tool_specs));
            let response = provider
                .chat(system, &history_for_provider, Some(tool_specs))
                .await;
            if let Ok(resp) = &response {
                record_prompt_usage(estimated, resp.prompt_usage.as_ref());
            }

            match response {
                Err(e) => {
                    error!("Provider error: {e:#}");
                    let message = e.to_string();
                    progress.turn_error(&message).await;
                    break (None, TurnStop::ProviderError { message });
                }
                Ok(resp) if !resp.has_tool_calls() => {
                    let text = resp.text.unwrap_or_default();
                    let msg = ChatMessage::assistant(&text);
                    history.push(msg.clone());
                    if let Some(p) = self.persistence {
                        p.append_message(&msg);
                    }
                    if !text.is_empty() {
                        progress.message_chunk(&text).await;
                        accumulated_text.push(text);
                    }
                    break (Some(accumulated_text.join("\n\n")), TurnStop::Replied);
                }
                Ok(resp) => {
                    round += 1;
                    let tool_calls = resp.tool_calls.clone();
                    if let Some(t) = resp.text.as_ref().filter(|s| !s.is_empty()) {
                        progress.message_chunk(t).await;
                        accumulated_text.push(t.clone());
                    }
                    let msg =
                        ChatMessage::assistant_with_tools(resp.text.clone(), tool_calls.clone());
                    history.push(msg.clone());
                    // Whether this append landed is captured and carried
                    // down to the `tool_result` append below: the two
                    // must not be skipped independently of each other
                    // (see the comment there for why).
                    let tool_use_persisted = self
                        .persistence
                        .is_none_or(|p| p.append_message_paired(&msg));

                    // Notify client of each tool starting
                    for call in &tool_calls {
                        progress.tool_start(&call.id, &call.name, &call.input).await;
                    }

                    // Permission gate.
                    //
                    // Serial on purpose. `decide` is a cheap pure call, but
                    // `approve` puts a dialog in front of a human, and
                    // firing several at once would stack them on the poor
                    // soul in the editor. Execution below stays concurrent.
                    //
                    // `offered` is this round's own advertised list —
                    // `tool_specs`, by name. `ToolSet::execute` dispatches
                    // on name across *every* tool registered on the shared
                    // `ToolSet`, not just the ones this round offered, so
                    // without checking membership here first, a call
                    // naming a tool this round never advertised would
                    // still run. That is what would let a subagent recurse
                    // by calling `subagent` itself — removed from its own
                    // `tool_specs` by `subagent_tool_specs`, but not from
                    // the underlying `ToolSet` — and what would let a
                    // definition's `tools:` restriction be bypassed by
                    // simply naming a tool outside it. One check here
                    // closes both, and the same hallucinated-name gap on
                    // the ordinary (non-subagent) path.
                    let offered: std::collections::HashSet<&str> =
                        tool_specs.iter().map(|s| s.name.as_ref()).collect();
                    let kinds = self.state.tools.kinds().await;
                    let origin = progress.origin();
                    let mut permitted: Vec<crate::provider::ToolCall> = Vec::new();
                    let mut refused: Vec<(String, String)> = Vec::new();
                    for call in &tool_calls {
                        use crate::tools::policy::{
                            Decision, Refusal, host_tool_denied, kind_of, refusal_message,
                        };

                        // Three gates, checked in order, only the last of
                        // which is the origin/kind policy table itself.
                        //
                        // `NotOffered` goes first: whether a name is even
                        // in this round's own `tool_specs` is a fact about
                        // what *this call* is allowed to see (a subagent's
                        // `tools:` restriction, `subagent` removed from its
                        // own nested round, a hallucinated name on an
                        // ordinary turn) — see `Refusal::NotOffered`'s doc
                        // — and it has to come before the deployment-wide
                        // host-machine gate below it, or a name outside
                        // this round's list but happening to match a host
                        // tool would be reported as merely unavailable on
                        // this transport rather than as what it actually
                        // is: not offered here at all.
                        //
                        // The host-machine gate itself sits in front of
                        // `decide`: it is a fact about the deployment ("may
                        // this agent touch its own disk at all"), not a
                        // row in the origin/kind policy table — so it is
                        // checked, and can refuse, before `decide` is even
                        // consulted. `routed_to_client` is what scopes that
                        // fact to the machine it is about: on this turn an
                        // ACP client means the call lands elsewhere, and
                        // the editor's own `session/request_permission`
                        // answer is what governs it instead.
                        let refusal = if !offered.contains(call.name.as_str()) {
                            Some(refusal_message(&call.name, Refusal::NotOffered))
                        } else if host_tool_denied(
                            &call.name,
                            host_access_enabled,
                            routed_to_client,
                        ) {
                            Some(refusal_message(&call.name, Refusal::Unavailable))
                        } else {
                            let kind = kind_of(&call.name, &kinds);
                            let verdict = crate::tools::policy::decide(origin, kind);
                            match verdict {
                                Decision::Allow => None,
                                Decision::Deny => {
                                    Some(refusal_message(&call.name, Refusal::Unavailable))
                                }
                                Decision::Ask => {
                                    if progress.approve(call, kind).await.allows() {
                                        None
                                    } else {
                                        Some(refusal_message(&call.name, Refusal::UserDeclined))
                                    }
                                }
                            }
                        };

                        match refusal {
                            None => {
                                // Every permitted call, not just an asked
                                // one: a client that saw `tool_start` needs
                                // to know this one is running rather than
                                // still waiting on it.
                                progress.tool_allowed(&call.id).await;
                                permitted.push(call.clone());
                            }
                            Some(reason) => {
                                info!("Refused tool {} (id={}): {reason}", call.name, call.id);
                                refused.push((call.id.clone(), reason));
                            }
                        }
                    }

                    // Execute all tools concurrently — each call wrapped in
                    // the session's memory namespace (task_local) so the
                    // memory tool writes under `memory/<namespace>/...`.
                    let tools = Arc::clone(&self.state.tools);
                    let ns = namespace.clone();
                    let timer_origin = self.timer_origin.clone();
                    let admin_room_profile = self.admin_room_profile.clone();
                    // Read once per turn, same as `timer_origin`: `None` on
                    // every non-ACP transport, which is what keeps the
                    // client-side tools refusing there rather than reaching
                    // for a connection that does not exist.
                    let acp_client = progress.acp_client();
                    // What `tools::subagent::SubagentTool` needs to run a
                    // nested conversation, scoped around every call this
                    // round the same way the timer/ACP task-locals are —
                    // see `TurnContext`. Built once per round rather than
                    // per call: it is the same for every permitted call in
                    // this round. `visible_specs` is an `Arc::clone` of
                    // the `Arc<[ToolSpec]>` built once above `loop`, not a
                    // fresh deep clone of `tool_specs` — see that binding's
                    // comment.
                    let turn_ctx = Arc::new(TurnContext {
                        state: Arc::clone(self.state),
                        provider: Arc::clone(self.provider),
                        progress: Arc::clone(progress),
                        visible_specs: Arc::clone(&visible_specs),
                        timer_origin: timer_origin.clone(),
                        session_id: self.persistence.map(|p| p.session_id.clone()),
                        subagent_depth: self.subagent_depth,
                        admin_room_profile: self.admin_room_profile.clone(),
                    });
                    let mut results: Vec<(String, crate::tools::ToolOutput)> =
                        futures_util::future::join_all(permitted.into_iter().map(|c| {
                            let tools = Arc::clone(&tools);
                            let ns = ns.clone();
                            let origin = timer_origin.clone();
                            let client = acp_client.clone();
                            let turn_ctx = Arc::clone(&turn_ctx);
                            let admin_room_profile = admin_room_profile.clone();
                            async move {
                                let fut = crate::tools::workspace_tools::scope_memory_namespace(
                                    ns,
                                    async move {
                                        info!("Executing tool: {} (id={})", c.name, c.id);
                                        let output = tools.execute(&c).await;
                                        info!("Tool {} done", c.name);
                                        (c.id, output)
                                    },
                                );
                                // A subagent's own tool calls need the turn
                                // context too — nested here rather than
                                // constructed fresh inside `SubagentTool`,
                                // so `current_turn_context()` is available
                                // to any tool this round, not just the one
                                // named `subagent`.
                                // The admin tools read this through
                                // `config_tools::current_admin_room_profile()`.
                                // Scoped here rather than inside the match
                                // below so the four (origin, client) arms do
                                // not each need their own copy; `None` is a
                                // meaningful value (no profile to allow), not
                                // "unset".
                                let fut = crate::tools::config_tools::scope_admin_room_profile(
                                    admin_room_profile.clone(),
                                    fut,
                                );
                                let fut = scope_turn_context(turn_ctx, fut);
                                // Both remaining scopes have to wrap
                                // execution too: the timer tool reads one
                                // task-local and the client-side tools read
                                // the other, and either missing breaks that
                                // set of tools. Each arm awaits in place
                                // rather than boxing a common future type,
                                // the same way the single-scope version of
                                // this match did before the client scope
                                // was added.
                                match (origin, client) {
                                    (Some(o), Some(c)) => {
                                        crate::timer::scope_timer_origin(
                                            o,
                                            crate::tools::acp_client::scope_acp_client(c, fut),
                                        )
                                        .await
                                    }
                                    (Some(o), None) => {
                                        crate::timer::scope_timer_origin(o, fut).await
                                    }
                                    (None, Some(c)) => {
                                        crate::tools::acp_client::scope_acp_client(c, fut).await
                                    }
                                    (None, None) => fut.await,
                                }
                            }
                        }))
                        .await;

                    // A refused call still owes the model a tool_result:
                    // every tool_use in the assistant message must be
                    // answered, and the reason is more useful to the model
                    // than silence. The turn is NOT ended — the model may
                    // have another route, and ACP's `Refusal` stop reason
                    // means "the agent declined", which is a different
                    // thing from "the user declined".
                    for (id, reason) in refused {
                        results.push((id, crate::tools::ToolOutput::from(reason)));
                    }

                    // Notify client of each tool completing
                    for call in &tool_calls {
                        progress.tool_end(&call.id, &call.name).await;
                    }

                    let mut text_results = Vec::with_capacity(results.len());
                    let mut images = Vec::new();
                    for (id, output) in results {
                        text_results.push((id, output.text));
                        images.extend(output.images);
                    }
                    let result_msg = ChatMessage::tool_results_with_images(text_results, images);
                    history.push(result_msg.clone());
                    // Gated on `tool_use_persisted`: this append must not run
                    // independently of the `tool_use` append above. If that
                    // one failed, the tip on disk is still the user message,
                    // so appending the result here would chain it directly
                    // onto that user message — a `tool_result` with no
                    // `tool_use` anywhere before it, which is just as
                    // rejected by the API as the reverse gap, and unlike the
                    // in-memory `history` (which is correct either way and
                    // gets scrubbed/reloaded next turn) would brick the
                    // on-disk session forever. A half-persisted pair is worse
                    // than neither, so skip this append too rather than
                    // leaving only the result on disk.
                    if tool_use_persisted && let Some(p) = self.persistence {
                        p.append_message_paired(&result_msg);
                    }
                }
            }
        };
        (final_text, stop)
    }
}

/// Execute one full LLM turn for an established session: hydrate history,
/// run the tool-calling loop, persist user + assistant messages to JSONL,
/// and report per-tool `tool_start` / `tool_end` progress through
/// `progress`. Does NOT send the final JSON-RPC result event — the caller
/// is responsible for shaping the final payload (text reply, voice audio,
/// etc.) and emitting the appropriate result event.
///
/// # This future may be dropped mid-turn
///
/// `/rpc` and the voice/heartbeat paths run this inside a detached
/// `tokio::spawn`, so it finishes whether or not the client is still there.
/// Two callers can drop it instead: the ACP endpoint does so deliberately,
/// on `session/cancel` and on a vanished client (`src/serve/acp.rs`, the
/// `session/prompt` handler's `tokio::select!`), and `/a2a` awaits it
/// directly in an axum handler, whose future hyper may drop when an HTTP
/// client disconnects mid-request. Either way it is dropped at whatever
/// await point it happens to be sitting on.
///
/// Nothing here unwinds, so a dropped turn leaves a *split*:
///
/// - The user message and any compaction summary are already on disk — they
///   are appended to JSONL as they happen (steps 4 and 5). The `state.sessions`
///   write-back at the end never runs, but step 1 no longer minds: it
///   re-reads storage every turn rather than trusting a cache that this
///   drop just left behind stale, so a cancelled prompt's already-saved
///   progress is what the *next* turn hydrates from, not the previous
///   completed turn's write-back. A compaction summary can still be
///   persisted and then thrown away by the drop, and will be produced
///   again next turn — that part is unchanged, it is simply not lost
///   information, only repeated work. For an ACP session, the same
///   re-read applies to `tool_use` and `tool_result`: each is appended as
///   it happens, so a drop between the two leaves exactly the gap
///   `AcpSessionStore::history`'s positional repair exists to close, and
///   step 1 hits that repair on every turn now, not only after a process
///   restart.
/// - Tool futures in flight are dropped too. `ShellTool` therefore sets
///   `kill_on_drop(true)` (`src/tools/builtin_tools.rs`) — without it a
///   cancelled turn left a shell command running against the workspace.
///   the client halves of `ShellTool` and `ClientShellStart`
///   own a process on a different machine, which a drop cannot kill for
///   them the way `kill_on_drop` kills a local child — so they are made
///   drop-safe the other way: the terminal handle is written into
///   `ServeState.acp_terminals` (a registry owned by `ServeState`, not
///   by the tool future) immediately after `create_terminal` succeeds,
///   before any later `.await` that this future's drop could land on.
///   The command keeps running on the editor's machine either way — as
///   it must, per this module's `AcpClient::release_terminal` doc — and
///   the handle survives the drop for the model to find and act on next
///   turn. Any tool added later that owns an external process, a lock
///   or a partially-written file must be drop-safe for the same reason.
///
/// Anything added to this function that must happen exactly once per turn
/// needs to be written with that in mind: reaching the end of the body is
/// not guaranteed.
pub(crate) async fn run_llm_turn(
    state: Arc<ServeState>,
    session_id: String,
    user_msg: ChatMessage,
    progress: Arc<dyn TurnHost>,
    timer_origin: Option<crate::timer::TimerOrigin>,
) -> LlmTurnOutcome {
    // Pick the right store up front so every persistence call in this
    // turn lands in the same place (device-default vs cross-device).
    let store = Arc::clone(state.store_for_session(&session_id));
    let is_acp = state.is_acp(&session_id).await;
    // The room profile this turn's admin-tool grant hangs on. An ACP session
    // carries the one its bearer token pinned at `session/new` (or
    // `load`/`resume`); `/rpc`, A2A, voice and the unattended loops name no
    // profile an operator declared, so they get `None` and the admin tools
    // refuse there. Channel rooms never reach this function — `Agent::
    // handle_message` runs its own loop, where the room travels as
    // `TimerOrigin::Chat` instead.
    let admin_room_profile = if is_acp {
        state
            .session_room_profiles
            .lock()
            .await
            .get(&session_id)
            .cloned()
    } else {
        None
    };

    // 1. Hydrate history fresh from storage every turn, rather than only
    //    the first time this process touches the session.
    //
    //    This function's own doc (above) already names the reason: a
    //    dropped turn — `session/cancel`, ACP's "stop and immediately
    //    send the next message" interrupt pattern chief among them —
    //    never reaches step 8's `state.sessions` write-back, but the user
    //    message and any completed `tool_use`/`tool_result` pairs from
    //    that same turn are already on disk (steps 4/5, appended as they
    //    happen). With `HashMap::entry(...).or_insert_with(...)`, once
    //    *any* turn had populated this session's entry — including an
    //    empty one inserted by a first turn that then got cancelled
    //    before writing anything back — every later turn cloned that
    //    stale in-memory copy and never looked at storage again: the
    //    interrupted turn's own progress, though durably saved, became
    //    invisible to the very next turn for the rest of the process's
    //    life. Loading fresh here costs a JSONL parse every turn instead
    //    of once per session, but keeps this cache honest against a drop
    //    landing anywhere in the turn above it. `history_for_model`/
    //    `load_session` also run `repair_tool_pairing`, so an orphaned
    //    `tool_use` left by a mid-round cancellation gets its synthesised
    //    placeholder result spliced back in here rather than staying a
    //    gap only a fresh process restart used to close.
    let mut history: Vec<ChatMessage> = {
        let loaded = if is_acp {
            state
                .acp_session_store
                .history_for_model(&session_id)
                .unwrap_or_default()
        } else {
            store.load_session(&session_id).unwrap_or_default()
        };
        state
            .sessions
            .lock()
            .await
            .insert(session_id.clone(), loaded.clone());
        loaded
    };
    let was_first_turn = history.is_empty();

    // 2. Resolve provider once per turn — sessions can pin a profile at
    //    initialize-time; absent that, the background provider is used.
    let provider = state.provider_for_session(&session_id).await;

    // 2a. Namespace chain follows the session's pinned room_profile when
    //     set; otherwise the implicit default namespace. Resolved here
    //     so it can be recorded in the session metadata on first chat
    //     (used by the daily-log builder to route NSFW sessions away
    //     from default-namespace rooms).
    let namespace = match state.session_room_profiles.lock().await.get(&session_id) {
        Some(rp_name) => state.config.namespace_for_room_profile(rp_name).to_string(),
        None => crate::config::DEFAULT_NAMESPACE_NAME.to_string(),
    };
    let namespace_chain = state.config.resolve_namespace_chain(&namespace);

    // 2b. Ensure JSONL file exists. If this session was deferred at initialize
    //     time, commit it now using the reserved public_id.
    //
    // Device-default sessions are always already-on-disk by the time
    // run_llm_turn runs (find_or_create_for_device writes the meta
    // line at create time) so `ensure_session` against them is a
    // no-op. Skip it to avoid synthesising a spurious grain-id
    // public_id that device-default sessions don't need.
    let key: ConversationKey = (session_id.clone(), None);
    if !is_acp && Arc::ptr_eq(&store, &state.cross_device_session_store) {
        let pending_pub_id = state.pending_sessions.lock().await.remove(&session_id);
        if let Err(e) = store
            .ensure_session(&session_id, &key, "rpc", pending_pub_id, &namespace)
            .map(|_| ())
        {
            warn!("Failed to ensure session file: {e}");
        }
    }

    // 3a. System prompt (rebuilt fresh per request).
    let room_info = state
        .session_room_metadata
        .lock()
        .await
        .get(&session_id)
        .cloned();
    let system = {
        let sp = state
            .workspace
            .build_system_prompt(
                state.config.anthropic.system_prompt.as_deref(),
                state.config.day_boundary_hour,
                &namespace_chain,
                room_info.as_ref(),
                progress.cwd().as_deref(),
            )
            .await;
        if sp.is_empty() { None } else { Some(sp) }
    };

    // 4. Append user message. Image scrubbing for storage is handled inside
    //    `SessionStore::append` so the in-memory history keeps full image
    //    bytes for the provider call while JSONL gets a hash marker.
    history.push(user_msg.clone());
    if is_acp {
        if let Err(e) = state
            .acp_session_store
            .append_message(&session_id, &user_msg)
        {
            warn!("Failed to persist user message: {e}");
        }
    } else if let Err(e) = store.append(&session_id, &user_msg) {
        warn!("Failed to persist user message: {e}");
    }

    // 5. Tool-calling loop — refresh MCP tools if any server signalled a change.
    state.tools.refresh_if_needed().await;
    // A tool the caller cannot use is worse than absent — see
    // `ToolSet::specs_filtered`. Host tools (the agent's own filesystem
    // and shell) are hidden from the list whenever the operator has not
    // opted this deployment into host access; the two client-side file
    // tools are hidden whenever there is no editor on the other end, or
    // it did not declare the matching `fs` capability.
    let host_access_enabled = state.config.tools.host_access.enabled;
    let has_client = progress.acp_client().is_some();
    let (client_fs_read, client_fs_write) = progress.client_fs_caps();
    let client_terminal = progress.client_terminal_cap();
    // Skills are additionally gated on the turn's namespace. This is
    // composed here rather than added as a sixth parameter to
    // `visible_tool_predicate`, because that function is also called
    // from `src/agent.rs`, which this branch may not edit. The channel
    // path needs no namespace check anyway: it passes every client flag
    // as false, so the arm above already hides all four tools there.
    let skills_enabled = state
        .config
        .memory_namespaces
        .get(&namespace)
        .map(|ns| ns.skills)
        .unwrap_or(false);
    let base = visible_tool_predicate(
        host_access_enabled,
        has_client,
        client_fs_read,
        client_fs_write,
        client_terminal,
    );
    let tool_specs = state
        .tools
        .specs_filtered(move |name: &str| {
            if !base(name) {
                return false;
            }
            if !skills_enabled
                && matches!(
                    name,
                    "skill" | "skill_install" | "skill_update" | "skill_uninstall"
                )
            {
                return false;
            }
            true
        })
        .await;
    let persistence = TurnPersistence {
        store: Arc::clone(&store),
        acp_store: Arc::clone(&state.acp_session_store),
        session_id: session_id.clone(),
        is_acp,
    };
    let (final_text, stop) = TurnLoop {
        state: &state,
        provider: &provider,
        system: system.as_deref(),
        tool_specs: &tool_specs,
        progress: &progress,
        timer_origin,
        namespace: namespace.clone(),
        persistence: Some(&persistence),
        subagent_depth: 0,
        admin_room_profile: admin_room_profile.clone(),
    }
    .run(&mut history)
    .await;

    // Scrub `Image` parts in the just-completed history into compact
    // `ImageRef` references backed by the workspace-external image
    // cache. After this, long-lived in-memory storage is hash-only;
    // the next turn re-hydrates from cache for the provider call.
    crate::image_cache::scrub_history_inplace(&mut history, state.image_cache.as_deref());

    // Update in-memory sessions map
    state
        .sessions
        .lock()
        .await
        .insert(session_id.clone(), history);

    LlmTurnOutcome {
        text: final_text,
        was_first_turn,
        stop,
    }
}

async fn run_turn(
    state: Arc<ServeState>,
    session_id: String,
    user_message: String,
    want_audio: bool,
    req_id: Value,
    tx: mpsc::Sender<Result<Event, Infallible>>,
) {
    let send = |evt: Event| {
        let tx = tx.clone();
        async move {
            let _ = tx.send(Ok(evt)).await;
        }
    };

    let outcome = run_llm_turn(
        Arc::clone(&state),
        session_id.clone(),
        ChatMessage::user(&user_message),
        Arc::new(SseProgress::new(tx.clone(), req_id.clone())),
        None,
    )
    .await;

    // GUI clients that asked for `audio` get the text-then-audio
    // sequence over progress notifications before the final result.
    // TTS failures here are non-fatal: text is still useful, so we
    // surface a `tts_error` notification and continue to the result.
    if want_audio && let Some(text) = outcome.text.as_deref() {
        send(notification_event(
            "notifications/progress",
            json!({"kind": "assistant_text", "text": text}),
        ))
        .await;
        stream_chat_tts(&state, &session_id, text, &tx).await;
    }

    // Send final result
    match &outcome.text {
        Some(text) => {
            send(result_event(&req_id, json!({ "content": text }))).await;
        }
        None => {
            send(error_event(&req_id, -32603, "No response generated")).await;
        }
    }

    // Generate and store session title after the first successful turn.
    if outcome.was_first_turn
        && let Some(text) = outcome.text
    {
        let state2 = Arc::clone(&state);
        let sid = session_id.clone();
        let user_msg = user_message.clone();
        tokio::spawn(async move {
            let p = state2.provider_for_session(&sid).await;
            if let Some(title) = generate_session_title(&*p, &user_msg, &text).await {
                let result = if state2.is_acp(&sid).await {
                    state2.acp_session_store.append_title(&sid, &title)
                } else {
                    state2.store_for_session(&sid).set_title(&sid, &title)
                };
                if let Err(e) = result {
                    warn!("Failed to store session title: {e}");
                }
            }
        });
    }
}

// ---------------------------------------------------------------------------
// Chat → TTS bridge (modalities=["text","audio"])
// ---------------------------------------------------------------------------

/// Synthesize `reply_text` via the session's voice_pipeline TTS provider
/// and stream the resulting PCM as `audio_chunk` progress notifications.
///
/// Best-effort: any failure (no voice configured, provider missing,
/// synth error, zero chunks) surfaces as a `tts_error` progress
/// notification and returns — the caller still emits the text result,
/// since GUI clients prefer "text without audio" over "no answer at
/// all" when TTS is misconfigured.
///
/// Intentionally separate from `run_voice_turn_from_text_sse`'s TTS
/// block: the voice path emits `stage` markers and treats TTS failure
/// as fatal (it aborts the JSON-RPC result). Sharing would risk
/// regressing that behaviour.
async fn stream_chat_tts(
    state: &Arc<ServeState>,
    session_id: &str,
    reply_text: &str,
    tx: &mpsc::Sender<Result<Event, Infallible>>,
) {
    use base64::Engine;

    let send = |evt: Event| {
        let tx = tx.clone();
        async move {
            let _ = tx.send(Ok(evt)).await;
        }
    };
    let emit_tts_error = |message: String| {
        let tx = tx.clone();
        async move {
            let _ = tx
                .send(Ok(notification_event(
                    "notifications/progress",
                    json!({"kind": "tts_error", "message": message}),
                )))
                .await;
        }
    };

    let pipeline = match resolve_voice_pipeline(state, session_id).await {
        Ok(p) => p,
        Err(VoicePipelineLookup::NoVoice) => {
            emit_tts_error(
                "voice unavailable: no STT/TTS providers configured server-side".to_string(),
            )
            .await;
            return;
        }
        Err(VoicePipelineLookup::NotConfigured) => {
            emit_tts_error("session's room_profile has no voice_pipeline configured".to_string())
                .await;
            return;
        }
    };
    let voice_registry = match state.voice.as_ref() {
        Some(v) => v.clone(),
        None => {
            emit_tts_error("voice registry unavailable".to_string()).await;
            return;
        }
    };
    let tts = match voice_registry.tts(&pipeline.tts_provider) {
        Some(p) => p,
        None => {
            emit_tts_error(format!(
                "tts_provider '{}' not instantiated",
                pipeline.tts_provider
            ))
            .await;
            return;
        }
    };

    let (pcm_tx, mut pcm_rx) = mpsc::channel::<Vec<i16>>(32);
    let reply_for_tts = reply_text.to_string();
    let synth_handle =
        tokio::spawn(async move { tts.synthesize_stream(&reply_for_tts, pcm_tx).await });
    let mut chunks_emitted = 0usize;
    while let Some(chunk) = pcm_rx.recv().await {
        let bytes: Vec<u8> = chunk.iter().flat_map(|s| s.to_le_bytes()).collect();
        let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
        send(notification_event(
            "notifications/progress",
            json!({"kind": "audio_chunk", "data": b64}),
        ))
        .await;
        chunks_emitted += 1;
    }
    match synth_handle.await {
        Ok(Ok(())) => {
            if chunks_emitted == 0 {
                emit_tts_error(format!(
                    "TTS provider '{}' produced no audio",
                    pipeline.tts_provider
                ))
                .await;
            }
        }
        Ok(Err(e)) => {
            error!("chat TTS synthesis error: {e:#}");
            emit_tts_error(format!("TTS synthesis failed: {e:#}")).await;
        }
        Err(join_err) => {
            error!("chat TTS task panicked: {join_err}");
            emit_tts_error(format!("TTS task panicked: {join_err}")).await;
        }
    }
}

// ---------------------------------------------------------------------------
// Title generation
// ---------------------------------------------------------------------------

async fn generate_session_title(
    provider: &dyn Provider,
    user_message: &str,
    assistant_response: &str,
) -> Option<String> {
    let user_snippet: String = user_message.chars().take(300).collect();
    let asst_snippet: String = assistant_response.chars().take(300).collect();
    let prompt = format!(
        "Generate a concise title (max 60 characters) for this conversation. \
        Respond with only the title text — no quotes, no punctuation at the end.\n\n\
        User: {user_snippet}\nAssistant: {asst_snippet}"
    );
    let messages = vec![ChatMessage::user(&prompt)];
    match provider.chat(None, &messages, None).await {
        Ok(resp) => resp.text.map(|t| {
            let t = t.trim().to_string();
            if t.chars().count() > 60 {
                t.chars().take(60).collect()
            } else {
                t
            }
        }),
        Err(e) => {
            warn!("Title generation failed: {e:#}");
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Test fixtures — ServeState backed by a scripted / hanging stub provider.
// Every endpoint/turn test elsewhere needs a ServeState; this is the one
// place that knows how to build one without a real Anthropic API key.
// ---------------------------------------------------------------------------

/// The one tool the fixture advertises, so a scripted turn has something
/// real to call: a turn whose tool call names a tool nobody registered
/// would exercise `ToolSet`'s "Unknown tool" path instead of an execution.
#[cfg(test)]
pub(crate) struct EchoTool {
    spec: crate::provider::ToolSpec,
}

#[cfg(test)]
impl EchoTool {
    pub(crate) fn new() -> Self {
        Self {
            spec: crate::provider::ToolSpec {
                name: "echo".into(),
                description: "Echo the given text back.".into(),
                input_schema: json!({
                    "type": "object",
                    "properties": { "text": { "type": "string" } },
                    "required": ["text"]
                }),
            },
        }
    }
}

#[cfg(test)]
#[async_trait::async_trait]
impl crate::tools::Tool for EchoTool {
    fn spec(&self) -> &crate::provider::ToolSpec {
        &self.spec
    }

    /// A test fixture standing in for an ordinary read-only tool.
    /// Deliberately NOT `Other`: that is the ask-me bucket, and the
    /// pre-existing ACP tests drive turns with a helper that answers no
    /// permission requests, so leaving this unclassified would make them
    /// hang rather than fail once the permission gate lands.
    fn kind(&self) -> crate::tools::ToolKind {
        crate::tools::ToolKind::Read
    }

    async fn execute(&self, input: &Value) -> anyhow::Result<String> {
        Ok(input["text"].as_str().unwrap_or_default().to_string())
    }
}

/// A stand-in for `shell`: `ToolKind::Execute`, so the policy asks or
/// refuses. Carries a per-instance "did I run" flag, which is how the
/// gate tests tell "refused" from "ran and returned an error".
///
/// Per instance, not a `static`: `cargo test` runs tests in parallel and
/// several tests construct one of these, so a process-global flag would
/// make the assertions depend on scheduling.
#[cfg(test)]
pub(crate) struct RiskyTool {
    spec: crate::provider::ToolSpec,
    ran: Arc<std::sync::atomic::AtomicBool>,
}

#[cfg(test)]
impl RiskyTool {
    pub(crate) fn new() -> Self {
        Self {
            spec: crate::provider::ToolSpec {
                name: "risky".into(),
                description: "Pretend to run a command.".into(),
                input_schema: json!({ "type": "object", "properties": {} }),
            },
            ran: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    /// A handle the test keeps after the tool is boxed into the ToolSet.
    pub(crate) fn ran_flag(&self) -> Arc<std::sync::atomic::AtomicBool> {
        Arc::clone(&self.ran)
    }
}

#[cfg(test)]
#[async_trait::async_trait]
impl crate::tools::Tool for RiskyTool {
    fn spec(&self) -> &crate::provider::ToolSpec {
        &self.spec
    }

    fn kind(&self) -> crate::tools::ToolKind {
        crate::tools::ToolKind::Execute
    }

    async fn execute(&self, _input: &Value) -> anyhow::Result<String> {
        self.ran.store(true, std::sync::atomic::Ordering::SeqCst);
        Ok("ran".to_string())
    }
}

/// A minimal named stub, for tests that only care whether a tool's name
/// survives `specs_filtered` — not what it does. Used to stand in for
/// the host tools (`crate::tools::policy::HOST_TOOLS`), which the
/// filtering tests below need present in the set so "not offered"
/// means the predicate hid them, not that they were never registered.
#[cfg(test)]
struct NamedStubTool(crate::provider::ToolSpec);

#[cfg(test)]
impl NamedStubTool {
    fn new(name: &str) -> Self {
        Self(crate::provider::ToolSpec {
            name: name.to_string().into(),
            description: String::new().into(),
            input_schema: json!({ "type": "object", "properties": {} }),
        })
    }
}

#[cfg(test)]
#[async_trait::async_trait]
impl crate::tools::Tool for NamedStubTool {
    fn spec(&self) -> &crate::provider::ToolSpec {
        &self.0
    }

    async fn execute(&self, _input: &Value) -> anyhow::Result<String> {
        Ok(String::new())
    }
}

/// A `ToolKind::Read` tool registered under a name the ACP turn's visibility
/// predicate passes through, whose only job is to record which room profile
/// the surrounding tool execution was scoped with.
///
/// The task-local is only observable from inside a tool, which is exactly
/// where the gate reads it, so a probe is the honest instrument: a test that
/// asserted on the scope directly would pin the plumbing rather than the
/// behaviour. Registered by name (`heartbeat_config`) that
/// `visible_tool_predicate` neither gates on `host_access` nor on a client
/// capability, so it is offered on both the ACP and the non-ACP path.
#[cfg(test)]
pub(crate) struct AdminProfileProbe {
    seen: std::sync::Mutex<Option<Option<String>>>,
    spec: crate::provider::ToolSpec,
}

#[cfg(test)]
impl AdminProfileProbe {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            seen: std::sync::Mutex::new(None),
            spec: crate::provider::ToolSpec {
                name: "heartbeat_config".into(),
                description: "Records the room profile this turn scoped.".into(),
                input_schema: serde_json::json!({"type": "object", "properties": {}}),
            },
        })
    }

    /// `Some(value)` once the tool ran, `None` before it did.
    pub(crate) fn seen(&self) -> Option<Option<String>> {
        self.seen.lock().unwrap().clone()
    }
}

#[cfg(test)]
#[async_trait::async_trait]
impl crate::tools::Tool for AdminProfileProbe {
    fn spec(&self) -> &crate::provider::ToolSpec {
        &self.spec
    }

    fn kind(&self) -> crate::tools::ToolKind {
        crate::tools::ToolKind::Read
    }

    async fn execute(&self, _input: &serde_json::Value) -> anyhow::Result<String> {
        *self.seen.lock().unwrap() = Some(crate::tools::config_tools::current_admin_room_profile());
        Ok("probed".to_string())
    }
}

/// The tool is registered as a `Box<dyn Tool>`, which needs a direct
/// `Tool` impl — but the test keeps its own `Arc` handle to read
/// `seen()` after registration. Forwarding through the `Arc` (the same
/// shape `SkillTool` and `SubagentTool` use) lets one instance back
/// both the registration and the assertion handle.
#[cfg(test)]
#[async_trait::async_trait]
impl crate::tools::Tool for Arc<AdminProfileProbe> {
    fn spec(&self) -> &crate::provider::ToolSpec {
        (**self).spec()
    }

    fn kind(&self) -> crate::tools::ToolKind {
        (**self).kind()
    }

    async fn execute(&self, input: &serde_json::Value) -> anyhow::Result<String> {
        (**self).execute(input).await
    }
}
/// Every `chat()` call a [`StubProvider`] has seen, in call order:
/// `(system, tool names offered)`. `system` is owned (not the borrowed
/// `Option<&str>` `chat()` receives) so the log outlives the call.
///
/// Exists because the scripted responses alone only prove a turn's
/// *outcome* was right — they say nothing about what the provider was
/// actually called *with*. Both of `subagent`'s headline properties
/// (its own system prompt, its own restricted tool list) are otherwise
/// asserted only where they are computed (`subagent_system_prompt`,
/// `subagent_tool_specs`), not where they are wired into the call
/// `TurnLoop::run` makes — this closes that gap. `Clone`, so the handle
/// obtained from [`StubProvider::call_log`] before the provider is
/// moved into a `ProviderRegistry` can still be read afterwards.
#[cfg(test)]
#[derive(Clone, Default)]
pub(crate) struct ChatLog(Arc<std::sync::Mutex<Vec<(Option<String>, Vec<String>)>>>);

#[cfg(test)]
impl ChatLog {
    /// A snapshot of every call recorded so far, in order.
    pub(crate) fn calls(&self) -> Vec<(Option<String>, Vec<String>)> {
        self.0.lock().unwrap().clone()
    }
}

/// Provider double for tests. In "scripted" mode it pops one
/// [`crate::provider::ChatResponse`] off a queue per `chat()` call. In
/// "hanging" mode `chat()` never resolves — used to keep a turn in
/// flight while a cancellation test races it.
#[cfg(test)]
pub(crate) struct StubProvider {
    /// `Err` entries let a test force one specific `chat()` call —
    /// identified by its position in the sequence, not by which
    /// conversation made it — to fail while the calls around it
    /// succeed (see `new_scripted`), the same way an exhausted script
    /// fails every call after it.
    script: Option<
        std::sync::Mutex<std::collections::VecDeque<Result<crate::provider::ChatResponse, String>>>,
    >,
    /// Only set in hanging mode. Counts entries into the parked `chat()` —
    /// see [`HangingChat::entered`].
    hang_entered: Option<Arc<std::sync::atomic::AtomicUsize>>,
    /// Only set in hanging mode. Counts in-flight `chat()` futures that were
    /// dropped (e.g. their task was aborted), so a cancellation test can
    /// assert the turn was actually torn down rather than merely observing
    /// that it never completed on its own.
    hang_dropped: Option<Arc<std::sync::atomic::AtomicUsize>>,
    /// See [`ChatLog`].
    calls: ChatLog,
}

/// The two observations a test can make about a hanging provider's
/// `chat()` calls.
///
/// `entered` exists so a test never has to *guess* that a turn has reached
/// the provider. Cancelling a turn that has not got that far yet drops a
/// future that never built the guard, so `dropped` would stay behind and the
/// assertion would be a coin toss; waiting for `entered` first makes it a
/// fact.
///
/// Both are counters rather than flags because prompts on one ACP connection
/// run concurrently: a test that opens two turns needs to wait for *both* to
/// reach the provider, and to see both of them torn down.
#[cfg(test)]
pub(crate) struct HangingChat {
    /// Incremented just before `chat()` parks forever, so a test can wait
    /// until N turns are genuinely inside the provider before taking their
    /// connection away.
    pub(crate) entered: Arc<std::sync::atomic::AtomicUsize>,
    /// Incremented when one of those parked futures is dropped: the proof
    /// that an abandoned turn actually stopped calling the provider.
    pub(crate) dropped: Arc<std::sync::atomic::AtomicUsize>,
}

#[cfg(test)]
impl HangingChat {
    /// Wait until one of the counters reaches `at_least`, or fail with
    /// `what` naming the thing that never happened.
    pub(crate) async fn wait_for(
        counter: &std::sync::atomic::AtomicUsize,
        at_least: usize,
        what: &str,
    ) {
        let waited = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while counter.load(std::sync::atomic::Ordering::SeqCst) < at_least {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await;
        assert!(
            waited.is_ok(),
            "timed out waiting for {what} (reached {}, wanted {at_least})",
            counter.load(std::sync::atomic::Ordering::SeqCst)
        );
    }
}

#[cfg(test)]
impl StubProvider {
    pub(crate) fn new(responses: Vec<crate::provider::ChatResponse>) -> Self {
        Self::new_scripted(responses.into_iter().map(Ok).collect())
    }

    /// Like `new`, but a script entry can be `Err` instead of a
    /// response, for a test that needs one particular `chat()` call to
    /// fail (e.g. a subagent's own provider call) while the calls
    /// before and after it succeed.
    pub(crate) fn new_scripted(items: Vec<Result<crate::provider::ChatResponse, String>>) -> Self {
        Self {
            script: Some(std::sync::Mutex::new(items.into())),
            hang_entered: None,
            hang_dropped: None,
            calls: ChatLog::default(),
        }
    }

    /// A provider whose `chat()` never resolves. Returns the provider plus
    /// the flags described on [`HangingChat`].
    pub(crate) fn new_hanging() -> (Self, HangingChat) {
        let entered = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let dropped = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        (
            Self {
                script: None,
                hang_entered: Some(Arc::clone(&entered)),
                hang_dropped: Some(Arc::clone(&dropped)),
                calls: ChatLog::default(),
            },
            HangingChat { entered, dropped },
        )
    }

    /// A handle onto this provider's [`ChatLog`], to keep after the
    /// provider is moved into a `ProviderRegistry`.
    pub(crate) fn call_log(&self) -> ChatLog {
        self.calls.clone()
    }
}

#[cfg(test)]
#[async_trait::async_trait]
impl Provider for StubProvider {
    fn name(&self) -> &str {
        "stub"
    }

    async fn chat(
        &self,
        system: Option<&str>,
        _messages: &[ChatMessage],
        tools: Option<&[crate::provider::ToolSpec]>,
    ) -> anyhow::Result<crate::provider::ChatResponse> {
        self.calls.0.lock().unwrap().push((
            system.map(|s| s.to_string()),
            tools
                .map(|specs| specs.iter().map(|s| s.name.to_string()).collect())
                .unwrap_or_default(),
        ));
        let Some(script) = &self.script else {
            // Hold a guard whose Drop flips `hang_dropped` before awaiting
            // forever, so a caller that aborts this future (rather than
            // waiting for it) leaves proof behind.
            struct DroppedFlag(Arc<std::sync::atomic::AtomicUsize>);
            impl Drop for DroppedFlag {
                fn drop(&mut self) {
                    self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                }
            }
            let flag = self
                .hang_dropped
                .clone()
                .expect("hanging StubProvider always carries a drop flag");
            let _guard = DroppedFlag(flag);
            // Announce the guard only once it exists: a test that waits for
            // this before cancelling knows the drop flag has something to
            // fire.
            self.hang_entered
                .as_ref()
                .expect("hanging StubProvider always carries an entry counter")
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            std::future::pending::<()>().await;
            unreachable!()
        };
        match script.lock().unwrap().pop_front() {
            Some(Ok(resp)) => Ok(resp),
            Some(Err(msg)) => Err(anyhow::anyhow!(msg)),
            None => Err(anyhow::anyhow!("StubProvider script exhausted")),
        }
    }
}

/// Keepalive for every test fixture but the one asserting on it. Off, so
/// no unrelated test carries a background ping timer.
#[cfg(test)]
const DEFAULT_TEST_ACP_KEEPALIVE_SECS: u64 = 0;

#[cfg(test)]
impl ServeState {
    /// State backed by temp directories and a stub provider that answers "ok".
    pub(crate) fn for_test(acp_enabled: bool) -> Arc<Self> {
        Self::for_test_scripted(
            acp_enabled,
            vec![crate::provider::ChatResponse {
                prompt_usage: None,
                text: Some("ok".to_string()),
                tool_calls: Vec::new(),
                stop_reason: None,
            }],
        )
    }

    pub(crate) fn for_test_scripted(
        acp_enabled: bool,
        responses: Vec<crate::provider::ChatResponse>,
    ) -> Arc<Self> {
        Self::build_for_test(acp_enabled, StubProvider::new(responses))
    }

    /// Same as [`Self::for_test_scripted`], with an explicit round budget
    /// so a test can pin the cap instead of depending on the shipped
    /// default. Tests that do not care keep calling `for_test_scripted`.
    pub(crate) fn for_test_scripted_with_rounds(
        acp_enabled: bool,
        responses: Vec<crate::provider::ChatResponse>,
        rounds: crate::config::ToolRounds,
    ) -> Arc<Self> {
        Self::build_for_test_with(
            acp_enabled,
            StubProvider::new(responses),
            rounds,
            DEFAULT_TEST_ACP_KEEPALIVE_SECS,
        )
    }

    /// State whose `/acp` keepalive fires at `keepalive_secs` instead of
    /// the shipped 30s, so a test can watch a ping arrive without waiting
    /// half a minute for it. Every other constructor pins the keepalive
    /// *off*, so no other test pays for a timer it does not assert on.
    pub(crate) fn for_test_acp_keepalive(keepalive_secs: u64) -> Arc<Self> {
        Self::build_for_test_with(
            true,
            StubProvider::new(vec![crate::provider::ChatResponse {
                prompt_usage: None,
                text: Some("ok".to_string()),
                tool_calls: Vec::new(),
                stop_reason: None,
            }]),
            crate::config::ToolRounds::default(),
            keepalive_secs,
        )
    }

    /// Same as [`Self::for_test_scripted`], plus a [`ChatLog`] handle so
    /// a test can assert on what each `chat()` call actually received —
    /// not just the scripted outcome, but the `system`/`tools` a
    /// property (e.g. a subagent's own prompt and tool list) claims
    /// were wired through.
    pub(crate) fn for_test_scripted_with_log(
        acp_enabled: bool,
        responses: Vec<crate::provider::ChatResponse>,
    ) -> (Arc<Self>, ChatLog) {
        let provider = StubProvider::new(responses);
        let log = provider.call_log();
        (Self::build_for_test(acp_enabled, provider), log)
    }

    /// State whose provider never returns, so a turn stays in flight.
    /// The returned [`HangingChat`] reports when that `chat()` call was
    /// entered and when it was dropped.
    pub(crate) fn for_test_hanging(acp_enabled: bool) -> (Arc<Self>, HangingChat) {
        let (provider, hanging) = StubProvider::new_hanging();
        (Self::build_for_test(acp_enabled, provider), hanging)
    }

    fn build_for_test(acp_enabled: bool, provider: StubProvider) -> Arc<Self> {
        Self::build_for_test_with(
            acp_enabled,
            provider,
            crate::config::ToolRounds::default(),
            DEFAULT_TEST_ACP_KEEPALIVE_SECS,
        )
    }

    fn build_for_test_with(
        acp_enabled: bool,
        provider: StubProvider,
        rounds: crate::config::ToolRounds,
        acp_keepalive_secs: u64,
    ) -> Arc<Self> {
        // Leak the TempDir guard on purpose: this is a test binary and the
        // OS reclaims the directory when it exits.
        let dir = Box::leak(Box::new(tempfile::tempdir().unwrap()));
        let base = dir.path().to_path_buf();
        let workspace_dir = base.join("workspace");

        // Device auth fixture: one device ("developer-device") routed to
        // room_profile "developer", with its bearer token fixed at
        // "sa-acp-token" so the many tests presenting that literal keep
        // working. Written by hand rather than through
        // `KeyStore::generate` (which always mints a random token) — the
        // key file format documents `token` as its only required field,
        // exactly like this. Mirrors the fixture shape in
        // `device_auth::tests`, minus the random token.
        let keys_file = base.join("keys.toml");
        let devices_file = crate::config::workspace_devices_path(&workspace_dir);
        std::fs::create_dir_all(devices_file.parent().unwrap()).unwrap();
        let mut devices = sapphire_framework::registry::Devices::load(&devices_file).unwrap();
        let device = devices.add("developer-device", None, None).unwrap();
        std::fs::write(
            &keys_file,
            format!(
                "[[key]]\ntoken = \"sa-acp-token\"\nlabel = \"developer-device\"\ndevice_id = \"{}\"\n",
                device.id
            ),
        )
        .unwrap();

        let mut config = Config::parse_for_test(
            r#"
[anthropic]
api_key = "test"

[profiles.dev]
provider = "stub"

[room_profile.developer]
profile  = "dev"
rooms    = []
"#,
        );
        config.acp = Some(crate::config::AcpConfig {
            enabled: acp_enabled,
            keepalive_secs: acp_keepalive_secs,
        });
        config.tools.tool_rounds = rounds;
        config.keys.file = Some(keys_file.clone());
        config.room_profiles.get_mut("developer").unwrap().devices = vec![device.id.to_string()];

        let device_auth = Arc::new(
            crate::device_auth::DeviceAuth::open(&keys_file, &devices_file, &config.room_profiles)
                .expect("test device_auth fixture should build"),
        );

        // Registered under both names: `provider_for_session` falls
        // through to the background provider when no room_profile is
        // pinned (as in the fixture_state_serves_the_scripted_provider
        // test above, which looks up an unpinned session), and the
        // background provider resolves the built-in "anthropic" key.
        let registry = ProviderRegistry::for_test(&["anthropic", "stub"], Arc::new(provider));

        // Same base directory as the cross-device store above: the ACP
        // store puts itself in an `acp/` subtree per namespace, so
        // sharing `base.join("sessions")` doesn't collide with `rpc/`.
        let tool_payload_cache =
            crate::tool_payload_cache::ToolPayloadCache::open(base.join("tool-payloads")).unwrap();
        let acp_session_store = Arc::new(AcpSessionStore::new(
            base.join("sessions"),
            Some(tool_payload_cache),
        ));
        let subagent_cache = Some(
            SubagentCache::open(base.join("subagents"), 8_388_608)
                .expect("test subagent_cache fixture should open"),
        );

        Arc::new(Self {
            config,
            registry: Arc::new(registry),
            workspace: Arc::new(Workspace::new(
                workspace_dir,
                crate::config::DigestConfig::default(),
            )),
            tools: Arc::new(ToolSet::new(vec![Box::new(EchoTool::new())], Vec::new())),
            permissions: Arc::new(acp_permissions::PermissionStore::open(
                base.join("acp-permissions.json"),
            )),
            cross_device_session_store: Arc::new(SessionStore::new(
                base.join("sessions"),
                "rpc",
                None,
            )),
            device_default_session_store: Arc::new(SessionStore::new(
                base.join("device-default"),
                "device-default",
                None,
            )),
            // Same layout production builds (`<sessions_base>/<ns>/autonomous/`,
            // here under the workspace dir) so a test asserting the
            // workspace-relative path a loop run writes actually exercises
            // the derived path, not a fallback string.
            autonomous_session_store: Arc::new(
                SessionStore::new(base.join("workspace").join("sessions"), "autonomous", None)
                    .with_dated_files(4),
            ),
            mcp_session_store: Arc::new(SessionStore::new(base.join("mcp"), "mcp", None)),
            mcp_project_index: Default::default(),
            sessions: Default::default(),
            pending_sessions: Default::default(),
            session_room_profiles: Default::default(),
            session_room_metadata: Default::default(),
            voice: None,
            image_cache: None,
            voice_subscribers: Default::default(),
            device_auth,
            open_acp_sessions: Default::default(),
            acp_session_store,
            acp_sessions: Default::default(),
            subagent_cache,
            acp_terminals: Default::default(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp_session::{EventBody, StoredPart};
    use crate::provider::{Role, UserInputKind};

    /// A refused tool still gets a `tool_result`, and the turn carries
    /// on. Refusing must not look to the model like the tool vanished,
    /// and must not end the turn — the model may have another route.
    #[tokio::test]
    async fn a_refused_tool_returns_a_result_and_the_turn_continues() {
        use crate::tools::policy::Origin;

        /// Records what it was told, so the test can assert a refused
        /// call is still reported as starting and ending. A client that
        /// hears `tool_start` and never `tool_end` leaves the entry
        /// spinning in its tool list forever.
        #[derive(Default)]
        struct ChannelHost {
            started: std::sync::Mutex<Vec<String>>,
            ended: std::sync::Mutex<Vec<String>>,
        }
        #[async_trait::async_trait]
        impl TurnHost for ChannelHost {
            async fn tool_start(&self, id: &str, _name: &str, _input: &Value) {
                self.started.lock().unwrap().push(id.to_string());
            }
            async fn tool_end(&self, id: &str, _name: &str) {
                self.ended.lock().unwrap().push(id.to_string());
            }
            async fn turn_error(&self, _message: &str) {}
            fn origin(&self) -> Origin {
                Origin::Channel
            }
        }

        // `risky` is Execute, so `Origin::Channel` refuses it. The
        // second scripted response is the model carrying on afterwards.
        let state = ServeState::for_test_scripted(
            true,
            vec![
                crate::provider::ChatResponse {
                    prompt_usage: None,
                    text: None,
                    tool_calls: vec![crate::provider::ToolCall {
                        id: "call-1".to_string(),
                        name: "risky".to_string(),
                        input: json!({}),
                    }],
                    stop_reason: None,
                },
                crate::provider::ChatResponse {
                    prompt_usage: None,
                    text: Some("could not run that".to_string()),
                    tool_calls: Vec::new(),
                    stop_reason: None,
                },
            ],
        );
        let risky = RiskyTool::new();
        let ran = risky.ran_flag();
        state.tools.register_tool(Box::new(risky)).await;

        let host = Arc::new(ChannelHost::default());
        let outcome = run_llm_turn(
            Arc::clone(&state),
            "s-refused".to_string(),
            ChatMessage::user("run it"),
            Arc::clone(&host) as Arc<dyn TurnHost>,
            None,
        )
        .await;

        assert_eq!(outcome.text.as_deref(), Some("could not run that"));
        assert!(
            !ran.load(std::sync::atomic::Ordering::SeqCst),
            "a refused tool must not have executed"
        );

        // The model must still be told what happened to the call it
        // made. Asserting on the turn's own text is not enough: the
        // scripted provider ignores the history it is handed, so an
        // implementation that refused the tool and then dropped its
        // result entirely would produce exactly the same reply — and
        // would fail the *next* real provider request with a 400,
        // because a tool_use without its tool_result is malformed.
        let history = state.sessions.lock().await;
        let messages = history.get("s-refused").expect("the session exists");
        let refusal = messages
            .iter()
            .flat_map(|m| &m.parts)
            .find_map(|part| match part {
                ContentPart::ToolResult {
                    tool_use_id,
                    content,
                } if tool_use_id == "call-1" => Some(content.clone()),
                _ => None,
            })
            .expect("the refused call still owes the model a tool_result");
        assert!(
            refusal.contains("risky"),
            "the refusal should name the tool, got {refusal}"
        );

        // Started and never ended leaves the entry spinning in a
        // client's tool list.
        assert_eq!(*host.started.lock().unwrap(), vec!["call-1".to_string()]);
        assert_eq!(*host.ended.lock().unwrap(), vec!["call-1".to_string()]);
    }

    /// The same call is allowed once the origin is a trusted one, so
    /// the refusal above is the policy talking and not a broken tool.
    #[tokio::test]
    async fn a_trusted_origin_runs_the_same_tool() {
        let state = ServeState::for_test_scripted(
            true,
            vec![
                crate::provider::ChatResponse {
                    prompt_usage: None,
                    text: None,
                    tool_calls: vec![crate::provider::ToolCall {
                        id: "call-1".to_string(),
                        name: "risky".to_string(),
                        input: json!({}),
                    }],
                    stop_reason: None,
                },
                crate::provider::ChatResponse {
                    prompt_usage: None,
                    text: Some("ran it".to_string()),
                    tool_calls: Vec::new(),
                    stop_reason: None,
                },
            ],
        );
        let risky = RiskyTool::new();
        let ran = risky.ran_flag();
        state.tools.register_tool(Box::new(risky)).await;

        let outcome = run_llm_turn(
            Arc::clone(&state),
            "s-allowed".to_string(),
            ChatMessage::user("run it"),
            Arc::new(NullProgress),
            None,
        )
        .await;

        assert_eq!(outcome.text.as_deref(), Some("ran it"));
        assert!(
            ran.load(std::sync::atomic::Ordering::SeqCst),
            "a trusted origin must have executed it"
        );
    }

    /// The control case for the ACP test in `serve::acp`: `/rpc` and A2A
    /// call `run_llm_turn` with no ACP session behind them, so no profile
    /// is scoped and the admin tools refuse. Asserting `None` — not merely
    /// "not 'developer'" — keeps the two paths distinguishable.
    #[tokio::test]
    async fn a_non_acp_turn_scopes_no_room_profile() {
        let state = ServeState::for_test_scripted(
            true,
            vec![
                crate::provider::ChatResponse {
                    prompt_usage: None,
                    text: None,
                    tool_calls: vec![crate::provider::ToolCall {
                        id: "call-1".to_string(),
                        name: "heartbeat_config".to_string(),
                        input: json!({"action": "list"}),
                    }],
                    stop_reason: None,
                },
                crate::provider::ChatResponse {
                    prompt_usage: None,
                    text: Some("done".to_string()),
                    tool_calls: Vec::new(),
                    stop_reason: None,
                },
            ],
        );
        let probe = AdminProfileProbe::new();
        state
            .tools
            .register_tool(Box::new(Arc::clone(&probe)))
            .await;

        run_llm_turn(
            Arc::clone(&state),
            "s-plain".to_string(),
            ChatMessage::user("go"),
            Arc::new(NullProgress),
            None,
        )
        .await;

        assert_eq!(probe.seen(), Some(None), "no profile may be scoped here");
    }

    /// A config tool must be reachable through the room profile an ACP
    /// session pinned — the whole grant this branch wires. Read
    /// end to end through `run_llm_turn`, not by inspecting fields: an
    /// implementation that computed the profile but never scoped it
    /// around tool execution (or scoped a value other than the session's
    /// pinned one) must fail here, the way Task 4's probe would.
    ///
    /// The tool is a real `ConfigTool` over a real workspace with
    /// `[tools.admin].room_profiles = ["ops"]`; the turn is an ACP turn
    /// only by `state.acp_sessions` plus the pin in
    /// `session_room_profiles`, exactly what `session/new` writes. The
    /// policy layer is left at the default `Origin::Trusted` so the
    /// profile gate is the only permission this call passes through —
    /// if the profile does not reach the tool, the call is refused.
    #[tokio::test]
    async fn an_acp_turn_carries_its_pinned_room_profile_to_its_tools() {
        let (state, _dir) = scripted_state_with_agent_config(vec!["ops".to_string()]).await;

        // The pin `session/new` writes: ACP session + a room profile name.
        let sid = "acp-profile-carry".to_string();
        state.acp_sessions.lock().await.insert(sid.clone());
        state
            .session_room_profiles
            .lock()
            .await
            .insert(sid.clone(), "ops".to_string());

        let outcome = run_llm_turn(
            Arc::clone(&state),
            sid.clone(),
            ChatMessage::user("list the agents"),
            Arc::new(NullProgress),
            None,
        )
        .await;

        assert_eq!(outcome.text.as_deref(), Some("listed"));
        let result = tool_result_for(&state, &sid, "call-1").await;
        assert!(
            result.contains("No agents definitions."),
            "the profile gate must have opened: got {result}"
        );
    }

    /// The fail-closed half: a turn on a transport that pins no profile
    /// (`/rpc` — not in `acp_sessions`) is refused by the profile gate
    /// even though a profile is on the allow list, and the turn still
    /// carries on with the refusal as the tool's result.
    #[tokio::test]
    async fn a_non_acp_turn_is_refused_by_the_profile_gate() {
        let (state, _dir) = scripted_state_with_agent_config(vec!["ops".to_string()]).await;

        // Not registered in `acp_sessions`, so this is an /rpc session —
        // `run_llm_turn` must hand its tools no profile at all, not the
        // one some *other* session pinned.
        let sid = "rpc-no-profile".to_string();
        state
            .session_room_profiles
            .lock()
            .await
            .insert("some-other-session".to_string(), "ops".to_string());

        run_llm_turn(
            Arc::clone(&state),
            sid.clone(),
            ChatMessage::user("list the agents"),
            Arc::new(NullProgress),
            None,
        )
        .await;

        let result = tool_result_for(&state, &sid, "call-1").await;
        assert!(
            result.contains("Permission denied") && result.contains("room_profiles"),
            "a /rpc turn must be refused by the profile gate: got {result}"
        );
    }

    /// The state + real `agent_config` tool the two wiring tests above
    /// share: a scripted one-round-then-answer provider, and a
    /// `ConfigTool` over `state.workspace` itself with
    /// `[tools.admin].room_profiles` set to `profiles`. The framework
    /// workspace the tool writes through is opened over the same
    /// directory (`Workspace::from_root` needs the `.sapphire-agent`
    /// marker), with a cache dir under the leaked test TempDir so the
    /// redb files it creates are reclaimed with the process.
    ///
    /// The returned `TempDir` must be held: it is the only thing keeping
    /// the workspace directory (and everything pointing into it) alive.
    #[cfg(test)]
    async fn scripted_state_with_agent_config(
        profiles: Vec<String>,
    ) -> (Arc<ServeState>, Box<tempfile::TempDir>) {
        static TEST_CTX: std::sync::OnceLock<sapphire_framework::workspace::AppContext> =
            std::sync::OnceLock::new();
        let ctx = TEST_CTX.get_or_init(|| {
            sapphire_framework::workspace::AppContext::new("sapphire-agent").allow_external_paths()
        });

        let state = ServeState::for_test_scripted(
            true,
            vec![
                crate::provider::ChatResponse {
                    prompt_usage: None,
                    text: None,
                    tool_calls: vec![crate::provider::ToolCall {
                        id: "call-1".to_string(),
                        name: "agent_config".to_string(),
                        input: json!({"action": "list"}),
                    }],
                    stop_reason: None,
                },
                crate::provider::ChatResponse {
                    prompt_usage: None,
                    text: Some("listed".to_string()),
                    tool_calls: Vec::new(),
                    stop_reason: None,
                },
            ],
        );
        let dir = Box::new(tempfile::tempdir().unwrap());
        let root = dir.path().to_path_buf();
        // `state.workspace`'s own dir is inside the leaked fixture base,
        // not this one — the tool's `workspace_root` is what matters, so
        // point both at the same root.
        std::fs::create_dir_all(root.join(".sapphire-agent")).unwrap();
        ctx.set_cache_dir(root.join(".sapphire-agent-cache"));

        let mut config = state.config.clone();
        config.tools.admin.room_profiles = profiles;
        let sw = sapphire_framework::workspace::Workspace::from_root(ctx, &root).unwrap();
        let ws = Arc::new(std::sync::Mutex::new(
            sapphire_framework::workspace::WorkspaceState::open(sw).unwrap(),
        ));

        state
            .tools
            .register_tool(Box::new(crate::tools::config_tools::ConfigTool::new(
                crate::tools::config_tools::ConfigDir::Agents,
                root,
                config,
                ws,
                None,
                std::sync::Weak::new(),
            )))
            .await;
        (state, dir)
    }

    /// The text of the `tool_result` answering `tool_use_id` in `sid`'s
    /// history — the way the model itself would see it.
    async fn tool_result_for(state: &Arc<ServeState>, sid: &str, tool_use_id: &str) -> String {
        let history = state.sessions.lock().await;
        let messages = history.get(sid).expect("the session exists");
        messages
            .iter()
            .flat_map(|m| &m.parts)
            .find_map(|part| match part {
                ContentPart::ToolResult {
                    tool_use_id: id,
                    content,
                } if id == tool_use_id => Some(content.clone()),
                _ => None,
            })
            .expect("the call still owes the model a tool_result")
    }

    /// A subagent runs under the parent's `Origin`, so it cannot do
    /// what the parent was refused. Anything else would make "ask a
    /// subagent" a way around the permission gate.
    ///
    /// `subagent` itself is `ToolKind::Other`, which the policy table
    /// groups with `Execute` in the same "risky" bucket in every origin
    /// (see `crate::tools::policy::decide`) — so an origin that
    /// auto-denies `Execute` (`Origin::Channel`) would auto-deny the
    /// top-level `subagent` call too, before delegation ever ran, which
    /// would make the test pass without exercising `SubagentTool` at
    /// all. `Origin::Acp(SessionMode::Default)` sends every risky call
    /// through `approve` instead of denying it outright, so this test
    /// can let the top-level `subagent` call through while having the
    /// very same `approve` reject the nested `risky` call — proving the
    /// nested call is judged by the *parent's* host, not a laxer one
    /// standing in for it.
    #[tokio::test]
    async fn a_subagent_is_judged_by_the_parents_origin() {
        use crate::tools::policy::{Approval, Origin, SessionMode};

        /// Approves everything except a tool named `risky` — modeling a
        /// human who would say yes to delegating but no to the risky
        /// command itself, however it is asked. The subagent's nested
        /// call reaches this exact same `approve`, so if the risky call
        /// were judged by anything other than the parent's own host it
        /// would not be rejected here.
        struct AskExceptRiskyHost;
        #[async_trait::async_trait]
        impl TurnHost for AskExceptRiskyHost {
            async fn tool_start(&self, _id: &str, _name: &str, _input: &Value) {}
            async fn tool_end(&self, _id: &str, _name: &str) {}
            async fn turn_error(&self, _message: &str) {}
            fn origin(&self) -> Origin {
                Origin::Acp(SessionMode::Default)
            }
            async fn approve(
                &self,
                call: &crate::provider::ToolCall,
                _kind: crate::tools::ToolKind,
            ) -> Approval {
                if call.name == "risky" {
                    Approval::RejectOnce
                } else {
                    Approval::AllowOnce
                }
            }
        }

        let state = ServeState::for_test_scripted(
            true,
            vec![
                // Parent round 1: delegate to the subagent.
                crate::provider::ChatResponse {
                    prompt_usage: None,
                    text: None,
                    tool_calls: vec![crate::provider::ToolCall {
                        id: "call-1".to_string(),
                        name: "subagent".to_string(),
                        input: json!({"agent": "delegator", "prompt": "run it"}),
                    }],
                    stop_reason: None,
                },
                // Subagent round 1: try the Execute-kind tool the
                // parent itself could not run under this host.
                crate::provider::ChatResponse {
                    prompt_usage: None,
                    text: None,
                    tool_calls: vec![crate::provider::ToolCall {
                        id: "sub-call-1".to_string(),
                        name: "risky".to_string(),
                        input: json!({}),
                    }],
                    stop_reason: None,
                },
                // Subagent round 2: give up after the refusal.
                crate::provider::ChatResponse {
                    prompt_usage: None,
                    text: Some("could not run it".to_string()),
                    tool_calls: Vec::new(),
                    stop_reason: None,
                },
                // Parent round 2.
                crate::provider::ChatResponse {
                    prompt_usage: None,
                    text: Some("done".to_string()),
                    tool_calls: Vec::new(),
                    stop_reason: None,
                },
            ],
        );

        let risky = RiskyTool::new();
        let ran = risky.ran_flag();
        state.tools.register_tool(Box::new(risky)).await;
        state
            .tools
            .register_tool(Box::new(crate::tools::subagent::SubagentTool::new(vec![
                crate::agents::AgentDef {
                    name: "delegator".to_string(),
                    description: "Delegates a risky call.".to_string(),
                    tools: Some(vec!["risky".to_string()]),
                    subagents: None,
                    prompt: "You are a delegator.".to_string(),
                    profile: None,
                },
            ])))
            .await;

        let outcome = run_llm_turn(
            Arc::clone(&state),
            "s-subagent-origin".to_string(),
            ChatMessage::user("delegate it"),
            Arc::new(AskExceptRiskyHost) as Arc<dyn TurnHost>,
            None,
        )
        .await;

        assert_eq!(outcome.text.as_deref(), Some("done"));
        assert!(
            !ran.load(std::sync::atomic::Ordering::SeqCst),
            "the subagent must be judged by the parent's own host — the \
             same `approve` that let delegation through must also be the \
             one the nested 'risky' call is judged by, and it rejects \
             that call by name"
        );
    }

    /// The depth cap is enforced by the *gate*, not just by the list.
    /// With `max_depth = 1` — the pre-recursion default, one delegation
    /// deep, no further — the nested turn's own `tool_specs` carries no
    /// `subagent`, and `ToolSet::execute` dispatches by name across every
    /// tool the shared `ToolSet` has registered, `subagent` included, so
    /// it is the gate (which refuses any call outside the round's own
    /// `tool_specs`) that actually stops the nested turn from calling
    /// `subagent` by name and recursing anyway. This asserts the gate,
    /// not just the list. See `a_subagent_can_nest_up_to_max_depth_two`
    /// for the same script where the cap permits one more level.
    #[tokio::test]
    async fn a_subagent_cannot_nest_past_max_depth_one() {
        let (mut state, chat_log) = ServeState::for_test_scripted_with_log(
            true,
            vec![
                // Parent round 1: delegate.
                crate::provider::ChatResponse {
                    prompt_usage: None,
                    text: None,
                    tool_calls: vec![crate::provider::ToolCall {
                        id: "call-1".to_string(),
                        name: "subagent".to_string(),
                        input: json!({"agent": "delegator", "prompt": "go"}),
                    }],
                    stop_reason: None,
                },
                // Subagent round 1: try to recurse by naming `subagent`
                // itself — never in this round's own `tool_specs` (the
                // cap is 1 here), but still a name the shared `ToolSet`
                // has registered.
                crate::provider::ChatResponse {
                    prompt_usage: None,
                    text: None,
                    tool_calls: vec![crate::provider::ToolCall {
                        id: "sub-call-1".to_string(),
                        name: "subagent".to_string(),
                        input: json!({"agent": "delegator", "prompt": "recurse"}),
                    }],
                    stop_reason: None,
                },
                // Subagent round 2: give up after the refusal.
                crate::provider::ChatResponse {
                    prompt_usage: None,
                    text: Some("gave up".to_string()),
                    tool_calls: Vec::new(),
                    stop_reason: None,
                },
                // Parent round 2.
                crate::provider::ChatResponse {
                    prompt_usage: None,
                    text: Some("done".to_string()),
                    tool_calls: Vec::new(),
                    stop_reason: None,
                },
            ],
        );

        // Pin the cap to 1 (one level only) rather than rely on the
        // shipped default of 2 — this test is specifically about the
        // no-nesting boundary.
        Arc::get_mut(&mut state)
            .unwrap()
            .config
            .tools
            .subagent
            .max_depth = 1;

        state
            .tools
            .register_tool(Box::new(crate::tools::subagent::SubagentTool::new(vec![
                crate::agents::AgentDef {
                    name: "delegator".to_string(),
                    description: "Delegates.".to_string(),
                    tools: None,
                    subagents: None,
                    prompt: "You are a delegator.".to_string(),
                    profile: None,
                },
            ])))
            .await;

        let outcome = run_llm_turn(
            Arc::clone(&state),
            "s-subagent-no-recursion".to_string(),
            ChatMessage::user("delegate it"),
            Arc::new(NullProgress),
            None,
        )
        .await;

        // If the self-call had actually recursed, satisfying it would
        // need a 5th `chat()` call (a nested-nested turn), leaving the
        // 4-entry script exhausted and the turn ending in a provider
        // error rather than "done" — so this one assertion catches
        // unbounded recursion by construction, without needing to
        // instrument `SubagentTool` itself.
        assert_eq!(outcome.text.as_deref(), Some("done"));
        assert_eq!(
            chat_log.calls().len(),
            4,
            "exactly parent-round-1, subagent-round-1, subagent-round-2, \
             parent-round-2 — a 5th call would mean the self-recursion \
             attempt actually ran"
        );
    }

    /// The counterpart of `a_subagent_cannot_nest_past_max_depth_one`:
    /// the same script, the same single `delegator` definition, but the
    /// shipped default cap (`max_depth = 2`). Now the nested turn *is*
    /// offered `subagent`, its recursive call runs, and the turn needs a
    /// fifth `chat()` call — depth-2's answer, then depth-1's, then the
    /// parent's. If the cap were misread as "levels below the delegator
    /// minus one", this script would exhaust at four calls and the fifth
    /// would error instead of completing.
    #[tokio::test]
    async fn a_subagent_can_nest_up_to_max_depth_two() {
        let (state, chat_log) = ServeState::for_test_scripted_with_log(
            true,
            vec![
                // Parent round 1: delegate to the depth-1 agent.
                crate::provider::ChatResponse {
                    prompt_usage: None,
                    text: None,
                    tool_calls: vec![crate::provider::ToolCall {
                        id: "call-1".to_string(),
                        name: "subagent".to_string(),
                        input: json!({"agent": "delegator", "prompt": "go"}),
                    }],
                    stop_reason: None,
                },
                // Depth-1 round 1: recurse — now offered, at depth 1 with
                // the default cap of 2.
                crate::provider::ChatResponse {
                    prompt_usage: None,
                    text: None,
                    tool_calls: vec![crate::provider::ToolCall {
                        id: "sub-call-1".to_string(),
                        name: "subagent".to_string(),
                        input: json!({"agent": "delegator", "prompt": "recurse"}),
                    }],
                    stop_reason: None,
                },
                // Depth-2 round 1: at the cap — no further delegation,
                // answers straight away.
                crate::provider::ChatResponse {
                    prompt_usage: None,
                    text: Some("leaf answer".to_string()),
                    tool_calls: Vec::new(),
                    stop_reason: None,
                },
                // Depth-1 round 2: receives the leaf's answer, answers.
                crate::provider::ChatResponse {
                    prompt_usage: None,
                    text: Some("delegator done".to_string()),
                    tool_calls: Vec::new(),
                    stop_reason: None,
                },
                // Parent round 2: receives it, finishes the turn.
                crate::provider::ChatResponse {
                    prompt_usage: None,
                    text: Some("done".to_string()),
                    tool_calls: Vec::new(),
                    stop_reason: None,
                },
            ],
        );

        state
            .tools
            .register_tool(Box::new(crate::tools::subagent::SubagentTool::new(vec![
                crate::agents::AgentDef {
                    name: "delegator".to_string(),
                    description: "Delegates.".to_string(),
                    tools: None,
                    subagents: None,
                    prompt: "You are a delegator.".to_string(),
                    profile: None,
                },
            ])))
            .await;

        let outcome = run_llm_turn(
            Arc::clone(&state),
            "s-subagent-nesting".to_string(),
            ChatMessage::user("delegate it"),
            Arc::new(NullProgress),
            None,
        )
        .await;

        assert_eq!(outcome.text.as_deref(), Some("done"));
        assert_eq!(
            chat_log.calls().len(),
            5,
            "the recursion actually ran: parent(2) + depth-1(2) + depth-2(1) \
             = 5 calls; four would mean the nested call was still refused"
        );
    }

    /// A definition's `tools:` restriction is also a promise about what
    /// is *offered*, and needs the same gate: without it, a subagent
    /// could call any tool registered on the shared `ToolSet`, whether
    /// or not its own definition named it. `Origin::Trusted` is used
    /// here specifically because it allows every kind unconditionally —
    /// isolating this from `a_subagent_is_judged_by_the_parents_origin`,
    /// which is about the origin/host gate, not this one.
    #[tokio::test]
    async fn a_subagent_restricted_to_one_tool_is_refused_another_that_is_registered() {
        let state = ServeState::for_test_scripted(
            true,
            vec![
                // Parent round 1: delegate.
                crate::provider::ChatResponse {
                    prompt_usage: None,
                    text: None,
                    tool_calls: vec![crate::provider::ToolCall {
                        id: "call-1".to_string(),
                        name: "subagent".to_string(),
                        input: json!({"agent": "delegator", "prompt": "go"}),
                    }],
                    stop_reason: None,
                },
                // Subagent round 1: try a tool outside its own `tools:`
                // list — registered on the `ToolSet`, and `Origin::Trusted`
                // would allow it unconditionally, but the definition
                // never named it.
                crate::provider::ChatResponse {
                    prompt_usage: None,
                    text: None,
                    tool_calls: vec![crate::provider::ToolCall {
                        id: "sub-call-1".to_string(),
                        name: "risky".to_string(),
                        input: json!({}),
                    }],
                    stop_reason: None,
                },
                // Subagent round 2: give up after the refusal.
                crate::provider::ChatResponse {
                    prompt_usage: None,
                    text: Some("could not run it".to_string()),
                    tool_calls: Vec::new(),
                    stop_reason: None,
                },
                // Parent round 2.
                crate::provider::ChatResponse {
                    prompt_usage: None,
                    text: Some("done".to_string()),
                    tool_calls: Vec::new(),
                    stop_reason: None,
                },
            ],
        );

        let risky = RiskyTool::new();
        let ran = risky.ran_flag();
        state.tools.register_tool(Box::new(risky)).await;
        state
            .tools
            .register_tool(Box::new(crate::tools::subagent::SubagentTool::new(vec![
                crate::agents::AgentDef {
                    name: "delegator".to_string(),
                    description: "Delegates, but only echoes.".to_string(),
                    tools: Some(vec!["echo".to_string()]),
                    subagents: None,
                    prompt: "You are a delegator.".to_string(),
                    profile: None,
                },
            ])))
            .await;

        let outcome = run_llm_turn(
            Arc::clone(&state),
            "s-subagent-restricted-tools".to_string(),
            ChatMessage::user("delegate it"),
            Arc::new(NullProgress),
            None,
        )
        .await;

        assert_eq!(outcome.text.as_deref(), Some("done"));
        assert!(
            !ran.load(std::sync::atomic::Ordering::SeqCst),
            "`risky` was registered and `Origin::Trusted` allows every \
             kind, so only the `tools:` restriction itself explains a \
             refusal here"
        );
    }

    /// `Refusal::NotOffered` closes the same hole on the *ordinary*
    /// (non-subagent) path — every test above exercises it only through
    /// `subagent`. A name the model invents (or gets right, but this
    /// round happens not to offer) that still matches something
    /// registered on the shared `ToolSet` must be refused before
    /// `ToolSet::execute` ever dispatches on it — see `TurnLoop::run`'s
    /// permission gate, which checks `offered` first, ahead of the
    /// host-machine gate and `decide` both.
    ///
    /// `shell_start` is the tool named: it isn't in `HOST_TOOLS`, so
    /// the host-machine gate — the *other* thing that could explain a
    /// refusal here — never fires for it, and `NullProgress`'s default
    /// `origin()` is `Origin::Trusted`, which `decide` allows
    /// unconditionally for every kind. The only thing left standing
    /// between the call and `ran` flipping `true` is the offered check
    /// itself: `visible_tool_predicate` excludes `shell_start` from
    /// this round's own `tool_specs` because `NullProgress` reports no
    /// ACP client (`has_client` is `false`), even though the tool stays
    /// registered and visible to `state.tools.kinds()`.
    #[tokio::test]
    async fn a_registered_but_unoffered_tool_is_refused_on_an_ordinary_turn() {
        struct FakeClientShellStart {
            spec: crate::provider::ToolSpec,
            ran: Arc<std::sync::atomic::AtomicBool>,
        }
        #[async_trait::async_trait]
        impl crate::tools::Tool for FakeClientShellStart {
            fn spec(&self) -> &crate::provider::ToolSpec {
                &self.spec
            }
            fn kind(&self) -> crate::tools::ToolKind {
                crate::tools::ToolKind::Execute
            }
            async fn execute(&self, _input: &Value) -> anyhow::Result<String> {
                self.ran.store(true, std::sync::atomic::Ordering::SeqCst);
                Ok("ran".to_string())
            }
        }

        let state = ServeState::for_test_scripted(
            true,
            vec![
                // Round 1: the model names a tool that is registered on
                // the shared `ToolSet` but excluded from this round's
                // own advertised list.
                crate::provider::ChatResponse {
                    prompt_usage: None,
                    text: None,
                    tool_calls: vec![crate::provider::ToolCall {
                        id: "call-1".to_string(),
                        name: "shell_start".to_string(),
                        input: json!({"command": "echo hi"}),
                    }],
                    stop_reason: None,
                },
                // Round 2: give up after the refusal.
                crate::provider::ChatResponse {
                    prompt_usage: None,
                    text: Some("done".to_string()),
                    tool_calls: Vec::new(),
                    stop_reason: None,
                },
            ],
        );

        let ran = Arc::new(std::sync::atomic::AtomicBool::new(false));
        state
            .tools
            .register_tool(Box::new(FakeClientShellStart {
                spec: crate::provider::ToolSpec {
                    name: "shell_start".into(),
                    description: "Pretend to run a command on the client.".into(),
                    input_schema: json!({ "type": "object", "properties": {} }),
                },
                ran: Arc::clone(&ran),
            }))
            .await;

        let outcome = run_llm_turn(
            Arc::clone(&state),
            "s-notoffered-parent-path".to_string(),
            ChatMessage::user("run something"),
            Arc::new(NullProgress),
            None,
        )
        .await;

        assert_eq!(outcome.text.as_deref(), Some("done"));
        assert!(
            !ran.load(std::sync::atomic::Ordering::SeqCst),
            "`shell_start` is registered, isn't a `HOST_TOOLS` name (so the \
             host gate never fires), and `Origin::Trusted` \
             allows every kind unconditionally — only the offered check \
             itself explains a refusal here"
        );
    }

    /// The isolation, asserted rather than assumed: what the subagent
    /// said to itself must not reach the parent's history or its store.
    /// Only the final answer comes back, as the tool's result.
    ///
    /// Also asserts the two headline properties where they are actually
    /// *wired* (the `system`/`tools` a `chat()` call was made with), not
    /// just where they are computed: `subagent_system_prompt` and
    /// `subagent_tool_specs` are pure functions with their own unit
    /// tests, but nothing short of inspecting `StubProvider`'s call log
    /// would catch `TurnLoop { system: Some(&system), ... }` being wired
    /// to the wrong string, or `tool_specs: &specs` to the parent's
    /// unfiltered list — every other test here would stay green.
    #[tokio::test]
    async fn a_subagents_conversation_does_not_reach_the_parent() {
        let (state, chat_log) = ServeState::for_test_scripted_with_log(
            true,
            vec![
                // Parent round 1: delegate.
                crate::provider::ChatResponse {
                    prompt_usage: None,
                    text: None,
                    tool_calls: vec![crate::provider::ToolCall {
                        id: "call-1".to_string(),
                        name: "subagent".to_string(),
                        input: json!({"agent": "investigator", "prompt": "go investigate"}),
                    }],
                    stop_reason: None,
                },
                // Subagent round 1: an intermediate tool call whose
                // result carries a string unique to this test. Deliberately
                // no accompanying text — `TurnLoop::run` folds a round's
                // narration text into the final answer it returns
                // (`accumulated_text`), so any text here would legitimately
                // reach the parent as part of "the final answer" and would
                // not test isolation at all. A tool call's own result,
                // by contrast, lives only in the subagent's local,
                // never-persisted `history`.
                crate::provider::ChatResponse {
                    prompt_usage: None,
                    text: None,
                    tool_calls: vec![crate::provider::ToolCall {
                        id: "sub-call-1".to_string(),
                        name: "echo".to_string(),
                        input: json!({"text": "SUBAGENT_SECRET_MUSING"}),
                    }],
                    stop_reason: None,
                },
                // Subagent round 2: its final answer.
                crate::provider::ChatResponse {
                    prompt_usage: None,
                    text: Some("FINAL ANSWER: 42".to_string()),
                    tool_calls: Vec::new(),
                    stop_reason: None,
                },
                // Parent round 2.
                crate::provider::ChatResponse {
                    prompt_usage: None,
                    text: Some("relayed".to_string()),
                    tool_calls: Vec::new(),
                    stop_reason: None,
                },
            ],
        );

        state
            .tools
            .register_tool(Box::new(crate::tools::subagent::SubagentTool::new(vec![
                crate::agents::AgentDef {
                    name: "investigator".to_string(),
                    description: "Investigates something.".to_string(),
                    tools: Some(vec!["echo".to_string()]),
                    subagents: None,
                    prompt: "You are an investigator.".to_string(),
                    profile: None,
                },
            ])))
            .await;

        // A workspace file the *parent's* system prompt is built from
        // (`Workspace::build_system_prompt`, `# Soul`) but the subagent's
        // must not be — its whole system prompt is the definition's
        // `prompt` plus the date, nothing read from the workspace.
        std::fs::write(
            state.workspace.dir().join("SOUL.md"),
            "PARENTS_SOUL_MARKER: I am the parent's own soul file.",
        )
        .unwrap();

        let sid = "acp-subagent-isolation".to_string();
        state.acp_sessions.lock().await.insert(sid.clone());
        state
            .acp_session_store
            .create(&sid, "default", "/work")
            .unwrap();

        let outcome = run_llm_turn(
            Arc::clone(&state),
            sid.clone(),
            ChatMessage::user("delegate it"),
            Arc::new(NullProgress),
            None,
        )
        .await;

        assert_eq!(outcome.text.as_deref(), Some("relayed"));

        // Whether any message in `history` mentions `needle` anywhere in
        // its parts — text, a tool call's input, or a tool result.
        fn history_mentions(history: &[ChatMessage], needle: &str) -> bool {
            history.iter().any(|m| {
                m.parts.iter().any(|p| match p {
                    ContentPart::Text(t) => t.contains(needle),
                    ContentPart::ToolUse { input, .. } => input.to_string().contains(needle),
                    ContentPart::ToolResult { content, .. } => content.contains(needle),
                    _ => false,
                })
            })
        }

        let mem_guard = state.sessions.lock().await;
        let mem_history = mem_guard.get(&sid).expect("the parent session exists");
        assert!(
            !history_mentions(mem_history, "SUBAGENT_SECRET_MUSING"),
            "the subagent's own intermediate tool traffic must not reach \
             the parent's in-memory history"
        );

        let acp_history = state
            .acp_session_store
            .history(&sid)
            .expect("the ACP store has the session");
        assert!(
            !history_mentions(&acp_history, "SUBAGENT_SECRET_MUSING"),
            "the subagent's own intermediate tool traffic must not reach \
             the parent's ACP store"
        );
        assert!(
            history_mentions(&acp_history, "FINAL ANSWER: 42"),
            "only the subagent's final answer should reach the parent, as \
             the tool's own result"
        );

        // Now the two headline properties, asserted at the actual wire:
        // what each `chat()` call was made with. Call order matches the
        // script order above, since the parent and the subagent share
        // one provider and one script queue: [0] parent round 1
        // (delegates), [1]/[2] the subagent's own two rounds, [3] parent
        // round 2.
        let calls = chat_log.calls();
        assert_eq!(calls.len(), 4, "unexpected call count: {calls:?}");

        let parent_system = calls[0].0.as_deref().unwrap_or_default();
        assert!(
            parent_system.contains("PARENTS_SOUL_MARKER"),
            "sanity: the parent's own turn must see its workspace's \
             SOUL.md, or this test cannot tell a wired-through workspace \
             prompt apart from one that was never read: {parent_system}"
        );
        assert!(
            calls[0].1.iter().any(|n| n == "subagent"),
            "sanity: the parent must be offered `subagent` itself: {:?}",
            calls[0].1
        );

        for (i, (system, tools)) in [&calls[1], &calls[2]].into_iter().enumerate() {
            let system = system.as_deref().unwrap_or_default();
            assert!(
                system.contains("You are an investigator."),
                "the subagent's round {i} must see its own definition's \
                 prompt: {system}"
            );
            assert!(
                !system.contains("PARENTS_SOUL_MARKER"),
                "the subagent's round {i} must not see the parent \
                 workspace's SOUL.md — its system prompt is the \
                 definition and nothing else: {system}"
            );
            assert_eq!(
                tools,
                &vec!["echo".to_string(), "subagent".to_string()],
                "the subagent's round {i} must see exactly its \
                 definition's own tool list plus the `subagent` tool (the \
                 default cap of 2 offers it at depth 1), nothing else"
            );
        }

        let parent_system_2 = calls[3].0.as_deref().unwrap_or_default();
        assert!(
            parent_system_2.contains("PARENTS_SOUL_MARKER"),
            "the parent's second round is still the parent's own turn: {parent_system_2}"
        );
    }

    /// An ACP session's messages go to the ACP store, and the shared
    /// `/rpc` store never sees them. This is the whole point of the
    /// branch: an editor's thread list should not be showing Matrix
    /// conversations, and the two formats must be free to drift.
    #[tokio::test]
    async fn an_acp_session_persists_to_the_acp_store() {
        let state = ServeState::for_test(true);
        let sid = "acp-1".to_string();
        state.acp_sessions.lock().await.insert(sid.clone());
        state
            .acp_session_store
            .create(&sid, "default", "/work")
            .unwrap();

        run_llm_turn(
            Arc::clone(&state),
            sid.clone(),
            ChatMessage::user("hello"),
            Arc::new(NullProgress),
            None,
        )
        .await;

        let history = state
            .acp_session_store
            .history(&sid)
            .expect("the ACP store has the session");
        assert!(
            history.iter().any(|m| matches!(
                m.parts.first(),
                Some(ContentPart::Text(t)) if t == "hello"
            )),
            "the user message is in the ACP store"
        );
        assert!(
            state
                .cross_device_session_store
                .load_session(&sid)
                .is_none(),
            "the /rpc store must not have been touched"
        );
    }

    /// A session nobody registered as ACP still behaves exactly as it
    /// did — the `/rpc` path is unchanged by this branch.
    #[tokio::test]
    async fn an_rpc_session_still_persists_to_the_rpc_store() {
        let state = ServeState::for_test(true);
        let sid = "rpc-1".to_string();

        run_llm_turn(
            Arc::clone(&state),
            sid.clone(),
            ChatMessage::user("hello"),
            Arc::new(NullProgress),
            None,
        )
        .await;

        assert!(
            state
                .cross_device_session_store
                .load_session(&sid)
                .is_some(),
            "the /rpc store still receives non-ACP sessions"
        );
    }

    /// The pre-existing transports keep today's behaviour. `Trusted` is
    /// what makes that true: `decide` allows everything for it, so no
    /// `/rpc`, voice or heartbeat turn can start asking for permission.
    #[test]
    fn existing_transports_are_trusted() {
        use crate::tools::policy::Origin;

        let (tx, _rx) = mpsc::channel(4);
        let sse = SseProgress::new(tx, json!(1));
        assert_eq!(sse.origin(), Origin::Trusted);
        assert_eq!(NullProgress.origin(), Origin::Trusted);
    }

    /// A host that cannot ask must not block the call. The default lets
    /// it through, which is what keeps the existing transports behaving
    /// exactly as before.
    #[tokio::test]
    async fn the_default_approval_allows_once() {
        use crate::tools::{ToolKind, policy::Approval};

        let call = crate::provider::ToolCall {
            id: "c1".to_string(),
            name: "shell".to_string(),
            input: json!({}),
        };
        assert_eq!(
            NullProgress.approve(&call, ToolKind::Execute).await,
            Approval::AllowOnce
        );
    }

    #[test]
    fn apply_label_passes_text_through_unchanged() {
        let msg = ChatMessage::user("hello");
        let labeled = apply_input_kind_label(msg.clone());
        assert_eq!(labeled.parts.len(), 1);
        match &labeled.parts[0] {
            ContentPart::Text(s) => assert_eq!(s, "hello"),
            _ => panic!("expected Text part"),
        }
    }

    #[test]
    fn apply_label_passes_none_input_kind_through_unchanged() {
        let msg = ChatMessage {
            role: Role::User,
            parts: vec![ContentPart::Text("hi".to_string())],
            input_kind: None,
            user_id: None,
        };
        let labeled = apply_input_kind_label(msg);
        match &labeled.parts[0] {
            ContentPart::Text(s) => assert_eq!(s, "hi"),
            _ => panic!("expected Text part"),
        }
    }

    #[test]
    fn apply_label_voice_prefixes_first_text_part() {
        let msg = ChatMessage::user_voice("what's the weather");
        let labeled = apply_input_kind_label(msg);
        match &labeled.parts[0] {
            ContentPart::Text(s) => {
                assert!(s.starts_with("[voice input]\n"), "got: {s}");
                assert!(s.ends_with("what's the weather"));
            }
            _ => panic!("expected Text part"),
        }
    }

    #[test]
    fn apply_label_voice_inserts_label_when_no_text_part_present() {
        let msg = ChatMessage {
            role: Role::User,
            parts: vec![ContentPart::Image {
                media_type: "image/png".to_string(),
                data_base64: "AAAA".to_string(),
            }],
            input_kind: Some(UserInputKind::Voice),
            user_id: None,
        };
        let labeled = apply_input_kind_label(msg);
        assert_eq!(labeled.parts.len(), 2);
        match &labeled.parts[0] {
            ContentPart::Text(s) => assert_eq!(s, "[voice input]"),
            _ => panic!("expected inserted Text part"),
        }
        assert!(matches!(labeled.parts[1], ContentPart::Image { .. }));
    }

    #[test]
    fn extract_bearer_accepts_both_cases_and_trims() {
        let mut h = HeaderMap::new();
        h.insert("authorization", HeaderValue::from_static("Bearer  tok-1 "));
        assert_eq!(extract_bearer(&h), Some("tok-1".to_string()));

        let mut h = HeaderMap::new();
        h.insert("authorization", HeaderValue::from_static("bearer tok-2"));
        assert_eq!(extract_bearer(&h), Some("tok-2".to_string()));
    }

    #[test]
    fn extract_bearer_rejects_missing_wrong_scheme_and_empty() {
        assert_eq!(extract_bearer(&HeaderMap::new()), None);

        let mut h = HeaderMap::new();
        h.insert("authorization", HeaderValue::from_static("Basic tok"));
        assert_eq!(extract_bearer(&h), None);

        let mut h = HeaderMap::new();
        h.insert("authorization", HeaderValue::from_static("Bearer   "));
        assert_eq!(extract_bearer(&h), None);
    }

    #[tokio::test]
    async fn fixture_state_serves_the_scripted_provider() {
        let state = ServeState::for_test_scripted(
            true,
            vec![crate::provider::ChatResponse {
                prompt_usage: None,
                text: Some("scripted reply".to_string()),
                tool_calls: Vec::new(),
                stop_reason: None,
            }],
        );
        let provider = state.provider_for_session("no-such-session").await;
        let resp = provider
            .chat(None, &[ChatMessage::user("hi")], None)
            .await
            .unwrap();
        assert_eq!(resp.text.as_deref(), Some("scripted reply"));
    }

    #[tokio::test]
    async fn fixture_state_resolves_the_test_token() {
        let state = ServeState::for_test(true);
        let resolved = state
            .device_auth
            .resolve("sa-acp-token")
            .expect("fixture token should resolve");
        assert_eq!(resolved.room_profile, "developer");
        assert!(state.device_auth.resolve("sa-wrong").is_none());
    }

    /// Without this check, an authenticated client could pass someone else's
    /// `device_id` in `voice/subscribe` params and displace that device's
    /// push channel — `voice_subscribers` is keyed on `device_id` alone, and
    /// the value came from client-supplied JSON, not from the token.
    #[tokio::test]
    async fn voice_subscribe_rejects_a_device_id_that_disagrees_with_the_token() {
        let state = ServeState::for_test(true);
        let authenticated_device_id = state
            .device_auth
            .resolve("sa-acp-token")
            .expect("fixture token should resolve")
            .device
            .id
            .to_string();

        let response = handle_voice_subscribe(
            Arc::clone(&state),
            Value::Null,
            Some(json!({ "device_id": "not-the-authenticated-device" })),
            "developer".to_string(),
            authenticated_device_id,
        )
        .await;

        let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["error"]["code"], -32602);
        assert!(
            v["error"]["message"]
                .as_str()
                .unwrap()
                .contains("does not match"),
            "{v}"
        );
        // Nothing should have been registered under the impersonated id.
        assert!(
            !state
                .voice_subscribers
                .lock()
                .await
                .contains_key("not-the-authenticated-device")
        );
    }

    // `handle_voice_pipeline_run` carries the identical guard (added right
    // after its own `device_id` extraction), but exercising it directly
    // needs a configured `VoiceProviders`, which `ServeState::for_test`
    // deliberately leaves `None` (see its `voice/pipeline_run unavailable`
    // early return) — outside what's worth building a real STT/TTS fixture
    // for here.

    #[tokio::test]
    async fn hanging_provider_sets_the_drop_flag_when_its_chat_future_is_aborted() {
        let (state, hanging) = ServeState::for_test_hanging(true);
        let handle = tokio::spawn(async move {
            let provider = state.provider_for_session("no-such-session").await;
            let _ = provider.chat(None, &[ChatMessage::user("hi")], None).await;
        });
        // Give the spawned task a chance to actually enter `chat()` and
        // start awaiting `pending::<()>()` before we abort it — otherwise
        // we might cancel it before the guard is ever constructed.
        tokio::task::yield_now().await;
        handle.abort();
        let _ = handle.await;
        assert_eq!(
            hanging.entered.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "expected the chat() call to have parked before it was aborted"
        );
        assert_eq!(
            hanging.dropped.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "expected the in-flight chat() future's drop guard to fire on abort"
        );
    }

    /// Regression test for `run_llm_turn`'s step 1: a turn must hydrate
    /// from storage every time, not only the first time this process
    /// touches the session — see that function's own doc for why.
    ///
    /// This reproduces the *result* of a `session/cancel`-then-reprompt
    /// without needing to actually abort a real turn: it seeds
    /// `state.sessions` with exactly what the old `entry().or_insert_with(
    /// ...)` hydration would have left behind after a first turn was
    /// dropped before its write-back ever ran — an empty cached entry —
    /// while the ACP session store (what step 4 durably appends the user
    /// message to, *before* the turn can even reach a provider call that
    /// might hang or be cancelled) already has real content. A second
    /// turn on the same session must not come back having "forgotten"
    /// that content.
    #[tokio::test]
    async fn a_stale_in_memory_cache_does_not_hide_a_sessions_persisted_progress() {
        let state = ServeState::for_test(true);
        let sid = "acp-interrupt-then-reprompt".to_string();
        state.acp_sessions.lock().await.insert(sid.clone());
        state
            .acp_session_store
            .create(&sid, "default", "/work")
            .unwrap();
        state
            .acp_session_store
            .append_message(&sid, &ChatMessage::user("FIRST_TURN_MARKER"))
            .unwrap();
        // What the pre-fix hydration left behind: an empty entry, inserted
        // by the cancelled first turn's own hydration and never
        // overwritten because that turn's future was dropped before
        // reaching its `state.sessions` write-back.
        state.sessions.lock().await.insert(sid.clone(), Vec::new());

        let outcome = run_llm_turn(
            Arc::clone(&state),
            sid.clone(),
            ChatMessage::user("SECOND_TURN_MARKER"),
            Arc::new(NullProgress),
            None,
        )
        .await;

        assert_eq!(outcome.text.as_deref(), Some("ok"));
        assert!(
            !outcome.was_first_turn,
            "storage was not empty, so this must not be reported as the \
             session's first turn"
        );

        let history = state.sessions.lock().await;
        let messages = history.get(&sid).expect("the session exists");
        let texts: Vec<&str> = messages
            .iter()
            .flat_map(|m| &m.parts)
            .filter_map(|p| match p {
                ContentPart::Text(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert!(
            texts.iter().any(|t| t.contains("FIRST_TURN_MARKER")),
            "the interrupted turn's own already-persisted progress must \
             survive into the next turn's history: {texts:?}"
        );
        assert!(
            texts.iter().any(|t| t.contains("SECOND_TURN_MARKER")),
            "and the new turn's own message must still be there too: {texts:?}"
        );
    }

    #[test]
    fn tool_event_params_pins_the_wire_field_names() {
        assert_eq!(
            tool_event_params("call-1", "recall"),
            json!({ "id": "call-1", "name": "recall" })
        );
    }

    #[tokio::test]
    async fn sse_progress_emits_one_event_per_call() {
        let (tx, mut rx) = mpsc::channel(8);
        let progress = SseProgress::new(tx, json!(7));

        progress
            .tool_start("call-1", "recall", &json!({ "query": "x" }))
            .await;
        progress.tool_end("call-1", "recall").await;
        drop(progress);

        let mut seen = Vec::new();
        while let Some(item) = rx.recv().await {
            seen.push(item);
        }
        assert_eq!(seen.len(), 2, "one event per call");
        assert!(seen[0].is_ok());
        assert!(seen[1].is_ok());
    }

    #[tokio::test]
    async fn sse_progress_turn_error_emits_one_event() {
        let (tx, mut rx) = mpsc::channel(8);
        let progress = SseProgress::new(tx, json!(7));

        progress.turn_error("provider exploded").await;
        drop(progress);

        let mut seen = Vec::new();
        while let Some(item) = rx.recv().await {
            seen.push(item);
        }
        assert_eq!(seen.len(), 1, "one event for the error");
        assert!(seen[0].is_ok());
    }

    /// The bug Fix 2 closes: a subagent's own provider failure must not
    /// report itself as *this request's* terminal outcome. `progress`
    /// for a subagent's nested `TurnLoop` is the parent's own host by
    /// design — here, `SseProgress` — and unwrapped, `turn_error` sends
    /// a terminal JSON-RPC error carrying the *parent* request's id the
    /// instant it is called. If `SubagentTool::execute` did not wrap
    /// `ctx.progress`, the subagent's own `chat()` failure below would
    /// fire that mid-turn, and the parent would still go on to finish
    /// normally afterwards — two terminal responses for one request.
    ///
    /// A successful delegation emits exactly two events on this
    /// `SseProgress` (`tool_start`/`tool_end` for the `subagent` call
    /// itself); a spurious `turn_error` from the subagent's own failure
    /// would add a third. Counting is enough — no need to decode
    /// `Event`'s payload — because nothing else on this path emits.
    #[tokio::test]
    async fn a_subagents_provider_failure_does_not_leak_a_stale_terminal_error() {
        let state = ServeState::build_for_test(
            true,
            StubProvider::new_scripted(vec![
                // Parent round 1: delegate.
                Ok(crate::provider::ChatResponse {
                    prompt_usage: None,
                    text: None,
                    tool_calls: vec![crate::provider::ToolCall {
                        id: "call-1".to_string(),
                        name: "subagent".to_string(),
                        input: json!({"agent": "delegator", "prompt": "go"}),
                    }],
                    stop_reason: None,
                }),
                // Subagent round 1: the provider call itself fails.
                Err("simulated subagent provider failure".to_string()),
                // Parent round 2, after the subagent's tool result
                // reports its own failure back as ordinary tool output.
                Ok(crate::provider::ChatResponse {
                    prompt_usage: None,
                    text: Some("done".to_string()),
                    tool_calls: Vec::new(),
                    stop_reason: None,
                }),
            ]),
        );
        state
            .tools
            .register_tool(Box::new(crate::tools::subagent::SubagentTool::new(vec![
                crate::agents::AgentDef {
                    name: "delegator".to_string(),
                    description: "Delegates.".to_string(),
                    tools: None,
                    subagents: None,
                    prompt: "You are a delegator.".to_string(),
                    profile: None,
                },
            ])))
            .await;

        let (tx, mut rx) = mpsc::channel(16);
        let progress = Arc::new(SseProgress::new(tx, json!(42)));

        let outcome = run_llm_turn(
            Arc::clone(&state),
            "s-subagent-sse-provider-failure".to_string(),
            ChatMessage::user("delegate it"),
            progress,
            None,
        )
        .await;

        assert_eq!(
            outcome.text.as_deref(),
            Some("done"),
            "the parent's own turn must complete normally even though \
             the subagent's own provider call failed"
        );

        rx.close();
        let mut seen = Vec::new();
        while let Some(item) = rx.recv().await {
            seen.push(item);
        }
        assert_eq!(
            seen.len(),
            2,
            "expected exactly tool_start + tool_end for the `subagent` \
             call; a 3rd event would be the subagent's own failure \
             leaking out as this request's terminal error"
        );
    }

    #[tokio::test]
    async fn null_progress_discards_without_panicking_and_sends_nothing() {
        // NullProgress holds no channel to observe directly, so the
        // absence of a panic across all three methods is the assertion.
        let progress = NullProgress;
        progress.tool_start("recall", "call-1", &json!({})).await;
        progress.tool_end("recall", "call-1").await;
        progress.turn_error("ignored").await;
    }

    /// The point of the task: after a turn that called a tool, reopening
    /// the session shows what the agent did, not just what was said.
    #[tokio::test]
    async fn an_acp_turn_persists_its_tool_calls() {
        let state = ServeState::for_test_scripted(
            true,
            vec![
                crate::provider::ChatResponse {
                    prompt_usage: None,
                    text: None,
                    tool_calls: vec![crate::provider::ToolCall {
                        id: "call-1".to_string(),
                        name: "risky".to_string(),
                        input: json!({}),
                    }],
                    stop_reason: None,
                },
                crate::provider::ChatResponse {
                    prompt_usage: None,
                    text: Some("done".to_string()),
                    tool_calls: Vec::new(),
                    stop_reason: None,
                },
            ],
        );
        state.tools.register_tool(Box::new(RiskyTool::new())).await;

        let sid = "acp-tools".to_string();
        state.acp_sessions.lock().await.insert(sid.clone());
        state
            .acp_session_store
            .create(&sid, "default", "/work")
            .unwrap();

        run_llm_turn(
            Arc::clone(&state),
            sid.clone(),
            ChatMessage::user("run it"),
            Arc::new(NullProgress),
            None,
        )
        .await;

        let history = state.acp_session_store.history(&sid).expect("the session");

        // Filtering into two id lists and comparing them would pass
        // even if the two messages landed in the wrong order (or if the
        // append sites were swapped) — with one call, both lists come
        // out as `["call-1"]` regardless of which side wrote first.
        // Locate each by its *position* in the message sequence instead,
        // so the assertion actually pins the order on disk.
        let use_at = history
            .iter()
            .position(|m| {
                m.parts
                    .iter()
                    .any(|p| matches!(p, ContentPart::ToolUse { id, .. } if id == "call-1"))
            })
            .expect("the tool_use was persisted");
        let result_at = history
            .iter()
            .position(|m| {
                m.parts.iter().any(|p| {
                    matches!(p, ContentPart::ToolResult { tool_use_id, .. } if tool_use_id == "call-1")
                })
            })
            .expect("the matching tool_result was persisted");

        assert!(
            use_at < result_at,
            "the tool_use (message {use_at}) must land before its tool_result \
             (message {result_at}) on disk — a tool_use with no matching \
             tool_result, or one in the wrong order, is rejected by the API \
             on reload"
        );
    }

    /// A tool that puts the session's JSONL back on the way an
    /// interrupted write would find it, timed to land between the two
    /// appends `run_llm_turn` makes around a tool call. Exists only to
    /// make the `tool_use` append fail while leaving the `tool_result`
    /// append able to *succeed if attempted* — the one way to tell a
    /// gated implementation from an ungated one that merely fails twice
    /// for the same reason.
    struct RestoringTool {
        spec: crate::provider::ToolSpec,
        moved_away: std::path::PathBuf,
        real_path: std::path::PathBuf,
    }

    impl RestoringTool {
        fn new(moved_away: std::path::PathBuf, real_path: std::path::PathBuf) -> Self {
            Self {
                spec: crate::provider::ToolSpec {
                    name: "risky".into(),
                    description: "Pretend to run a command.".into(),
                    input_schema: json!({ "type": "object", "properties": {} }),
                },
                moved_away,
                real_path,
            }
        }
    }

    #[async_trait::async_trait]
    impl crate::tools::Tool for RestoringTool {
        fn spec(&self) -> &crate::provider::ToolSpec {
            &self.spec
        }

        fn kind(&self) -> crate::tools::ToolKind {
            crate::tools::ToolKind::Execute
        }

        async fn execute(&self, _input: &Value) -> anyhow::Result<String> {
            std::fs::rename(&self.moved_away, &self.real_path)
                .expect("restoring the session file mid-tool-call");
            Ok("ran".to_string())
        }
    }

    /// Fix 1: the write side's two `tool_use`/`tool_result` appends
    /// "must not be skipped independently" of each other. This pins
    /// that the code actually enforces it, not just says it in a
    /// comment.
    ///
    /// The session's file is moved out of the way before the turn
    /// starts, so the `tool_use` append fails (the store can't find the
    /// session). The tool itself — `RestoringTool` — puts the file back
    /// mid-turn, between the `tool_use` append (already failed) and the
    /// `tool_result` append (yet to come). An ungated implementation
    /// would find the file present again and happily append the
    /// `tool_result` on its own — landing exactly the bricking bug this
    /// fix exists for: a `tool_result` chained straight onto the user
    /// message, with no `tool_use` anywhere before it. The fix must
    /// skip that second append too.
    #[tokio::test]
    async fn a_failed_tool_use_append_suppresses_the_tool_result_append_too() {
        let state = ServeState::for_test_scripted(
            true,
            vec![
                crate::provider::ChatResponse {
                    prompt_usage: None,
                    text: None,
                    tool_calls: vec![crate::provider::ToolCall {
                        id: "call-1".to_string(),
                        name: "risky".to_string(),
                        input: json!({}),
                    }],
                    stop_reason: None,
                },
                crate::provider::ChatResponse {
                    prompt_usage: None,
                    text: Some("done".to_string()),
                    tool_calls: Vec::new(),
                    stop_reason: None,
                },
            ],
        );

        let sid = "acp-tool-use-failure".to_string();
        state.acp_sessions.lock().await.insert(sid.clone());
        state
            .acp_session_store
            .create(&sid, "default", "/work")
            .unwrap();

        let real_path = state.acp_session_store.path_for_test(&sid);
        let moved_away = real_path.with_extension("jsonl.moved");
        std::fs::rename(&real_path, &moved_away).unwrap();

        state
            .tools
            .register_tool(Box::new(RestoringTool::new(
                moved_away.clone(),
                real_path.clone(),
            )))
            .await;

        run_llm_turn(
            Arc::clone(&state),
            sid.clone(),
            ChatMessage::user("run it"),
            Arc::new(NullProgress),
            None,
        )
        .await;

        // `events()` reads the raw, un-repaired file — unlike
        // `history()`, which would (correctly, per Fix 2) drop a stray
        // `tool_result` with nothing before it on the way out and so
        // could pass here even without the write-side gate this test
        // is actually about. Reading raw is what makes this a test of
        // Fix 1 rather than an accidental re-test of Fix 2's cleanup.
        let events = state
            .acp_session_store
            .events(&sid)
            .expect("the file exists again — the tool put it back");

        let has_use = events.iter().any(|e| match &e.body {
            EventBody::Message { parts, .. } => parts
                .iter()
                .any(|p| matches!(p, StoredPart::ToolUseRef { id, .. } if id == "call-1")),
            _ => false,
        });
        let has_result = events.iter().any(|e| match &e.body {
            EventBody::Message { parts, .. } => parts
                .iter()
                .any(|p| matches!(p, StoredPart::ToolResultRef { tool_use_id, .. } if tool_use_id == "call-1")),
            _ => false,
        });
        assert!(!has_use, "the tool_use append failed and must stay absent");
        assert!(
            !has_result,
            "a tool_result with no tool_use before it must never reach disk — \
             the second append should have been gated on the first one's \
             success, even though the file was available again by the time \
             it would have run"
        );
    }

    /// `/rpc` used to skip tool traffic entirely (its store had no
    /// reference form for a tool result, so writing one raw would put
    /// the content in the workspace and the retrieve index). The four
    /// `SessionStore` kinds now have the same workspace-external cache
    /// ACP does, so `/rpc` persists both halves too. Tracked as #194.
    #[tokio::test]
    async fn an_rpc_turn_now_persists_its_tool_calls_too() {
        let state = ServeState::for_test_scripted(
            true,
            vec![
                crate::provider::ChatResponse {
                    prompt_usage: None,
                    text: None,
                    tool_calls: vec![crate::provider::ToolCall {
                        id: "call-1".to_string(),
                        name: "risky".to_string(),
                        input: json!({}),
                    }],
                    stop_reason: None,
                },
                crate::provider::ChatResponse {
                    prompt_usage: None,
                    text: Some("done".to_string()),
                    tool_calls: Vec::new(),
                    stop_reason: None,
                },
            ],
        );
        state.tools.register_tool(Box::new(RiskyTool::new())).await;

        // Not registered in `acp_sessions`, so this is an /rpc session.
        let sid = "rpc-tools".to_string();

        run_llm_turn(
            Arc::clone(&state),
            sid.clone(),
            ChatMessage::user("run it"),
            Arc::new(NullProgress),
            None,
        )
        .await;

        // Read the raw JSONL rather than `load_session`: `load_session`
        // runs `repair_tool_pairing`, which synthesises a MISSING_RESULT
        // stand-in for an orphaned `tool_use` on the way out. That would
        // make this test pass even if the `tool_result` append were
        // silently dropped — it would prove only "the tool_use reached
        // disk and something answers it after loading", not "both
        // halves reached disk". Reading raw is the same technique
        // `a_failed_tool_use_append_suppresses_the_tool_result_append_too`
        // uses against the ACP store, and for the same reason (see its
        // own comment).
        let path = state
            .cross_device_session_store
            .absolute_path_for(&sid)
            .expect("the session file exists");
        let raw = std::fs::read_to_string(&path).unwrap();
        let use_at = raw
            .find("ToolUse")
            .expect("the tool_use never reached disk");
        let result_at = raw.find("ToolResultRef").expect(
            "the tool_result never reached disk — repair_tool_pairing \
             would hide this on a load",
        );
        assert!(
            use_at < result_at,
            "the pair is out of order on disk:\n{raw}"
        );
    }

    /// The four SessionStore kinds persist tool traffic now, so a /rpc
    /// session's tool_use and tool_result both reach disk (#194).
    #[test]
    fn a_non_acp_session_persists_both_halves_of_a_tool_call() {
        let base = tempfile::TempDir::new().unwrap();
        let cache_dir = tempfile::TempDir::new().unwrap();
        let cache =
            crate::tool_payload_cache::ToolPayloadCache::open(cache_dir.path().to_path_buf())
                .unwrap();
        let store = SessionStore::new(base.path().join("sessions"), "rpc", Some(cache));
        let key = ("s1".to_string(), None);
        store
            .ensure_session("s1", &key, "rpc", None, "default")
            .unwrap();

        store
            .append(
                "s1",
                &ChatMessage::assistant_with_tools(
                    None,
                    vec![crate::provider::ToolCall {
                        id: "c1".to_string(),
                        name: "file_read".to_string(),
                        input: serde_json::json!({}),
                    }],
                ),
            )
            .unwrap();
        store
            .append(
                "s1",
                &ChatMessage::tool_results_with_images(
                    vec![("c1".to_string(), "contents".to_string())],
                    Vec::new(),
                ),
            )
            .unwrap();

        let loaded = store.load_session("s1").expect("the session loads");
        assert!(
            loaded
                .iter()
                .flat_map(|m| &m.parts)
                .any(|p| matches!(p, ContentPart::ToolUse { id, .. } if id == "c1")),
            "tool_use missing: {loaded:?}"
        );
        assert!(
            loaded.iter().flat_map(|m| &m.parts).any(|p| matches!(p, ContentPart::ToolResult { tool_use_id, content } if tool_use_id == "c1" && content == "contents")),
            "tool_result missing: {loaded:?}"
        );
    }

    /// The measurement exists to distinguish "many sessions accumulate"
    /// from "one session grows", because only the first is fixed by
    /// dropping idle sessions. A total alone cannot tell them apart, so
    /// the largest single session is reported too.
    #[tokio::test]
    async fn residency_separates_the_total_from_the_largest_session() {
        let state = ServeState::for_test(true);
        {
            let mut sessions = state.sessions.lock().await;
            sessions.insert("small".to_string(), vec![ChatMessage::user("hi")]);
            sessions.insert(
                "big".to_string(),
                vec![ChatMessage {
                    role: Role::User,
                    parts: vec![ContentPart::ToolResult {
                        tool_use_id: "c1".to_string(),
                        content: "x".repeat(5_000),
                    }],
                    input_kind: None,
                    user_id: None,
                }],
            );
        }

        let r = state.session_residency().await;
        assert_eq!(r.sessions, 2);
        assert_eq!(r.messages, 2);
        assert_eq!(r.text_bytes, 2, "only the 'hi'");
        assert_eq!(r.tool_result_bytes, 5_000);
        assert_eq!(
            r.largest.as_ref().map(|(id, _)| id.as_str()),
            Some("big"),
            "the largest session is named so one long thread is \
             distinguishable from many short ones"
        );
    }

    #[tokio::test]
    async fn residency_of_an_empty_map_is_all_zero() {
        let state = ServeState::for_test(true);
        state.sessions.lock().await.clear();
        let r = state.session_residency().await;
        assert_eq!(r.sessions, 0);
        assert_eq!(r.messages, 0);
        assert_eq!(r.largest, None);
    }

    // -----------------------------------------------------------------
    // visible_tool_predicate — which tools a turn's model may see
    // -----------------------------------------------------------------

    /// What a hypothetical ACP turn's editor declared, for
    /// `tool_names_for_turn`. Deliberately not `ClientCapabilities`
    /// itself: these tests exercise `visible_tool_predicate`, the same
    /// function `run_llm_turn` calls, not the wire-level plumbing that
    /// fills `AcpSession::client_capabilities` in from `initialize` —
    /// that plumbing has no test-visible seam of its own (see the
    /// comment at the end of `src/serve/acp.rs`'s test module).
    struct TestCaps {
        fs_read: bool,
        fs_write: bool,
        terminal: bool,
    }

    /// A `ToolSet` carrying a stand-in for every name the predicate has an
    /// opinion about, so "not offered" in the assertions below means the
    /// predicate hid the name — not that it was never registered in the
    /// first place.
    ///
    /// Stand-ins rather than the real tools for the file and shell names:
    /// after #270 those names are *one* set of tools whose machine the turn
    /// decides (`file_read` is the agent's or the editor's depending on the
    /// session), and `visible_tool_predicate` is a pure function of the name
    /// and the five flags. Which machine a name reaches is `builtin_tools`'
    /// business and is tested there; this fixture is about the predicate's
    /// own table. The three lifecycle tools are the real ones, since they
    /// have no agent-side body at all — still under their pre-#270
    /// `shell_*` names, which Task 4 renames.
    fn client_filtering_test_set() -> ToolSet {
        let names = [
            // The unified set: one name each, routed by session type.
            "file_read",
            "file_write",
            "file_append",
            "file_delete",
            "dir_list",
            "dir_walk",
            "shell",
            // Skill tools, gated on the same terminal capability.
            "skill",
            "skill_install",
            "skill_update",
            "skill_uninstall",
        ];
        let mut tools: Vec<Box<dyn crate::tools::Tool>> = names
            .iter()
            .map(|name| Box::new(NamedStubTool::new(name)) as Box<dyn crate::tools::Tool>)
            .collect();
        tools.push(Box::new(crate::tools::client_tools::ClientShellStart::new()));
        tools.push(Box::new(
            crate::tools::client_tools::ClientShellOutput::new(),
        ));
        tools.push(Box::new(crate::tools::client_tools::ClientShellKill::new()));
        ToolSet::new(tools, Vec::new())
    }

    /// The tool names an ACP turn with an editor declaring `caps` would
    /// see, at the `host_access` setting given.
    ///
    /// `host_access` is a parameter rather than the `true` this helper
    /// used to hard-code: on an ACP turn it must make no difference at
    /// all, since the unified names reach the editor's machine and the
    /// switch speaks for the agent's own. Every ACP test below passes
    /// `false` — the shipped default, and the value that used to hide all
    /// seven — so "still offered" is the sharper statement.
    async fn tool_names_for_turn(caps: TestCaps, host_access: bool) -> Vec<String> {
        client_filtering_test_set()
            .specs_filtered(visible_tool_predicate(
                host_access,
                true,
                caps.fs_read,
                caps.fs_write,
                caps.terminal,
            ))
            .await
            .into_iter()
            .map(|s| s.name.to_string())
            .collect()
    }

    /// The tool names a turn with no editor on the other end (`/rpc`,
    /// Matrix, Discord, voice) would see: every client-capability flag
    /// `false`, exactly as `Agent::handle_message` passes them, with
    /// `host_access` varied.
    async fn tool_names_for_turn_without_a_client(host_access: bool) -> Vec<String> {
        client_filtering_test_set()
            .specs_filtered(visible_tool_predicate(
                host_access,
                false,
                false,
                false,
                false,
            ))
            .await
            .into_iter()
            .map(|s| s.name.to_string())
            .collect()
    }

    /// A client that says it can read but not write gets exactly one of
    /// the two file tools. Clients implement these independently, so
    /// the two flags are read separately rather than as one "fs" bit.
    ///
    /// `host_access` is off in the helper: the seven names are
    /// `HOST_TOOLS`, and an ACP turn must offer them regardless — that
    /// the deployment-wide switch has no say here is the change #270
    /// makes.
    #[tokio::test]
    async fn the_two_fs_tools_follow_their_own_capability_flags() {
        let names = tool_names_for_turn(
            TestCaps {
                fs_read: true,
                fs_write: false,
                terminal: false,
            },
            false,
        )
        .await;
        assert!(names.contains(&"file_read".to_string()));
        assert!(!names.contains(&"file_write".to_string()));
        // `append` needs both halves: it is a read and a write.
        assert!(!names.contains(&"file_append".to_string()));

        let names = tool_names_for_turn(
            TestCaps {
                fs_read: false,
                fs_write: true,
                terminal: false,
            },
            false,
        )
        .await;
        assert!(!names.contains(&"file_read".to_string()));
        assert!(names.contains(&"file_write".to_string()));
        // Still not: it would have to read the file back first.
        assert!(!names.contains(&"file_append".to_string()));
    }

    /// Every name that reaches the editor's shell is offered only when the
    /// editor declared `terminal/*` support — the same independence the two
    /// file tools get, just with one flag instead of two since ACP's
    /// `terminal` capability isn't split into finer-grained bits. ACP has no
    /// request for delete, so it names `rm` on that terminal and rides the
    /// same flag, as do `dir_list`/`dir_walk`.
    #[tokio::test]
    async fn the_terminal_tool_follows_its_own_capability_flag() {
        let terminal_tools = [
            // The unified names that reach the editor's shell...
            "shell",
            "file_delete",
            "dir_list",
            "dir_walk",
            // ...plus the three with no agent-side body at all, which keep
            // their original `shell_*` names until Task 4.
            "shell_start",
            "shell_output",
            "shell_kill",
        ];

        let names = tool_names_for_turn(
            TestCaps {
                fs_read: false,
                fs_write: false,
                terminal: true,
            },
            false,
        )
        .await;
        for tool in terminal_tools {
            assert!(
                names.contains(&tool.to_string()),
                "missing {tool}: {names:?}"
            );
        }

        let names = tool_names_for_turn(
            TestCaps {
                fs_read: false,
                fs_write: false,
                terminal: false,
            },
            false,
        )
        .await;
        for tool in terminal_tools {
            assert!(
                !names.contains(&tool.to_string()),
                "unexpected {tool}: {names:?}"
            );
        }
    }

    /// Inside an ACP session the seven names are gated on capabilities
    /// alone — `host_access` is not consulted at all, because every one of
    /// them reaches the editor. Here it is left at its off-by-default
    /// setting, which before #270 hid all seven.
    #[tokio::test]
    async fn an_acp_turn_sees_the_seven_names_by_capability_only() {
        let names = tool_names_for_turn(
            TestCaps {
                fs_read: true,
                fs_write: true,
                terminal: true,
            },
            false,
        )
        .await;
        for n in crate::tools::policy::HOST_TOOLS {
            assert!(names.contains(&n.to_string()), "missing {n}: {names:?}");
        }

        // `fs.read` only: the read half survives, the write half and the
        // terminal-dependent names do not. No fallback to the agent's
        // disk — that is the point of the capability gate, and `dir_list`
        // /`dir_walk` ride the terminal flag rather than being exempt.
        let names = tool_names_for_turn(
            TestCaps {
                fs_read: true,
                fs_write: false,
                terminal: false,
            },
            false,
        )
        .await;
        assert!(names.contains(&"file_read".to_string()));
        for n in [
            "file_write",
            "file_append",
            "file_delete",
            "dir_list",
            "dir_walk",
            "shell",
        ] {
            assert!(!names.contains(&n.to_string()), "unexpected {n}: {names:?}");
        }
    }

    /// Outside an ACP session the same seven names answer to the
    /// deployment switch and nothing else.
    ///
    /// `dir_list` and `dir_walk` are pinned on their own here because
    /// they were client-only before #270 and are now ordinary
    /// `HOST_TOOLS` with an agent-side body: a Matrix/Discord or `/rpc`
    /// turn must see them exactly when host access is on, and never on a
    /// capability flag it does not have. The three lifecycle tools go the
    /// other way — no agent-side body exists, so they are absent at both
    /// settings.
    #[tokio::test]
    async fn a_non_acp_turn_is_governed_by_host_access_alone() {
        let off = tool_names_for_turn_without_a_client(false).await;
        for n in crate::tools::policy::HOST_TOOLS {
            assert!(!off.contains(&n.to_string()), "{n} should be hidden");
        }

        let on = tool_names_for_turn_without_a_client(true).await;
        for n in crate::tools::policy::HOST_TOOLS {
            assert!(on.contains(&n.to_string()), "{n} should be present");
        }

        for n in ["shell_start", "shell_output", "shell_kill"] {
            assert!(!on.contains(&n.to_string()), "{n} has no agent-side body");
            assert!(!off.contains(&n.to_string()), "{n} has no agent-side body");
        }
    }

    /// Matrix, Discord, `/rpc` and voice have no editor. Offering them a
    /// tool that can only fail wastes a round trip and invites the model
    /// to pick the wrong machine.
    #[tokio::test]
    async fn a_non_acp_turn_is_offered_no_lifecycle_tools() {
        for host_access in [false, true] {
            let names = tool_names_for_turn_without_a_client(host_access).await;
            for name in ["shell_start", "shell_output", "shell_kill"] {
                assert!(
                    !names.contains(&name.to_string()),
                    "{name} has no agent-side body, got: {names:?}"
                );
            }
        }
    }

    /// Channels reach `visible_tool_predicate` with every client flag
    /// false (see `src/agent.rs`), so gating on the terminal capability
    /// is also what keeps skills off Matrix and Discord — the same rule
    /// as before #270, and it has to stay true now that the seven unified
    /// names *are* offered there when host access is on. A skill tool has
    /// no agent-side body either: it resolves a directory on the editor's
    /// machine.
    #[test]
    fn skill_tools_need_a_client_with_a_terminal() {
        let none = visible_tool_predicate(false, false, false, false, false);
        for t in ["skill", "skill_install", "skill_update", "skill_uninstall"] {
            assert!(!none(t), "{t} offered with no client");
        }
        // And the host switch does not change that: it speaks for the
        // agent's own disk, which is not where these run.
        let none_host_on = visible_tool_predicate(true, false, false, false, false);
        for t in ["skill", "skill_install", "skill_update", "skill_uninstall"] {
            assert!(!none_host_on(t), "{t} offered with no client");
        }
        let full = visible_tool_predicate(false, true, true, true, true);
        for t in ["skill", "skill_install", "skill_update", "skill_uninstall"] {
            assert!(full(t), "{t} hidden from a fully capable editor");
        }
        let no_term = visible_tool_predicate(false, true, true, true, false);
        assert!(!no_term("skill"), "skill offered without a terminal");
    }

    /// A turn with an editor on the other end, for the two tests below.
    /// `origin` is `Acp(Default)` — where an `Execute` name would be asked
    /// about, but a `Read` one is allowed outright — and `fs_read` is the
    /// only capability declared, so `file_read` is the one unified name
    /// this turn advertises.
    struct AcpHostForGateTests {
        client: Option<Arc<dyn crate::tools::acp_client::AcpClient>>,
    }

    #[async_trait::async_trait]
    impl TurnHost for AcpHostForGateTests {
        async fn tool_start(&self, _id: &str, _name: &str, _input: &Value) {}
        async fn tool_end(&self, _id: &str, _name: &str) {}
        async fn turn_error(&self, _message: &str) {}
        fn origin(&self) -> crate::tools::policy::Origin {
            crate::tools::policy::Origin::Acp(crate::tools::policy::SessionMode::Default)
        }
        fn acp_client(&self) -> Option<Arc<dyn crate::tools::acp_client::AcpClient>> {
            self.client.clone()
        }
        fn client_fs_caps(&self) -> (bool, bool) {
            (true, false)
        }
    }

    /// A registered `file_read` that records having run, so a turn can be
    /// judged on whether the call reached execution rather than only on
    /// what the model was told.
    struct FakeFileRead {
        spec: crate::provider::ToolSpec,
        ran: Arc<std::sync::atomic::AtomicBool>,
    }

    impl FakeFileRead {
        fn new(ran: Arc<std::sync::atomic::AtomicBool>) -> Self {
            Self {
                spec: crate::provider::ToolSpec {
                    name: "file_read".into(),
                    description: "Pretend to read a file.".into(),
                    input_schema: json!({"type": "object", "properties": {}}),
                },
                ran,
            }
        }
    }

    #[async_trait::async_trait]
    impl crate::tools::Tool for FakeFileRead {
        fn spec(&self) -> &crate::provider::ToolSpec {
            &self.spec
        }
        fn kind(&self) -> crate::tools::ToolKind {
            crate::tools::ToolKind::Read
        }
        async fn execute(&self, _input: &Value) -> anyhow::Result<String> {
            self.ran.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok("ran".to_string())
        }
    }

    /// The gate's meaning change, where the gate is actually consulted:
    /// an ACP turn on a deployment with `host_access` off — the shipped
    /// default, and the case that switch was protecting — runs a call
    /// named `file_read` anyway, because that call is going to the
    /// editor's machine. Before Task 3 this refused it: the gate had no
    /// `routed_to_client` to tell the two apart, so a deployment-wide
    /// switch would have been deciding for a machine it does not own.
    #[tokio::test]
    async fn an_acp_turn_runs_a_host_named_tool_with_host_access_off() {
        let state = ServeState::for_test_scripted(
            true,
            vec![
                crate::provider::ChatResponse {
                    prompt_usage: None,
                    text: None,
                    tool_calls: vec![crate::provider::ToolCall {
                        id: "call-1".to_string(),
                        name: "file_read".to_string(),
                        input: json!({"path": "note.txt"}),
                    }],
                    stop_reason: None,
                },
                crate::provider::ChatResponse {
                    prompt_usage: None,
                    text: Some("done".to_string()),
                    tool_calls: Vec::new(),
                    stop_reason: None,
                },
            ],
        );
        assert!(
            !state.config.tools.host_access.enabled,
            "the shipped default is the case under test"
        );

        let ran = Arc::new(std::sync::atomic::AtomicBool::new(false));
        state
            .tools
            .register_tool(Box::new(FakeFileRead::new(Arc::clone(&ran))))
            .await;

        let client: Arc<dyn crate::tools::acp_client::AcpClient> =
            Arc::new(crate::tools::acp_client::tests::FakeClient::default());
        let outcome = run_llm_turn(
            Arc::clone(&state),
            "s-acp-host-gate".to_string(),
            ChatMessage::user("read it"),
            Arc::new(AcpHostForGateTests {
                client: Some(client),
            }),
            None,
        )
        .await;

        assert_eq!(outcome.text.as_deref(), Some("done"));
        assert!(
            ran.load(std::sync::atomic::Ordering::SeqCst),
            "an ACP turn reaches the editor's machine, so the deployment's \
             host_access switch has nothing to refuse here"
        );
    }

    /// The other side of the same gate, with one variable changed — the
    /// client's presence. With no editor, a name the gate hides is not
    /// merely refused at execution: it is never offered, so the model
    /// cannot name its way past the switch. `/rpc`, voice, the heartbeat
    /// and the autonomous loops take this path.
    #[tokio::test]
    async fn a_non_acp_turn_never_offers_a_host_tool_to_begin_with() {
        let (state, log) = ServeState::for_test_scripted_with_log(
            true,
            vec![
                crate::provider::ChatResponse {
                    prompt_usage: None,
                    text: None,
                    tool_calls: vec![crate::provider::ToolCall {
                        id: "call-1".to_string(),
                        name: "file_read".to_string(),
                        input: json!({"path": "note.txt"}),
                    }],
                    stop_reason: None,
                },
                crate::provider::ChatResponse {
                    prompt_usage: None,
                    text: Some("done".to_string()),
                    tool_calls: Vec::new(),
                    stop_reason: None,
                },
            ],
        );

        let ran = Arc::new(std::sync::atomic::AtomicBool::new(false));
        state
            .tools
            .register_tool(Box::new(FakeFileRead::new(Arc::clone(&ran))))
            .await;

        let outcome = run_llm_turn(
            Arc::clone(&state),
            "s-nonacp-host-gate".to_string(),
            ChatMessage::user("read it"),
            Arc::new(AcpHostForGateTests { client: None }),
            None,
        )
        .await;

        assert_eq!(outcome.text.as_deref(), Some("done"));
        assert!(
            !ran.load(std::sync::atomic::Ordering::SeqCst),
            "host access is off in this deployment, so a host-named call must \
             not reach execution on a turn with no client"
        );
        let offered = &log.calls()[0].1;
        assert!(
            !offered.contains(&"file_read".to_string()),
            "the name was never advertised to the model: {offered:?}"
        );
    }

    /// The bug this pins: `round` used to be counted over the whole
    /// history, which restoring a session now seeds with everything it
    /// did before this turn started. A session that arrives with more
    /// tool_use messages already in it than the budget allows — exactly
    /// what a restored, long-lived room looks like — tripped the budget
    /// check on the very first iteration, before the provider was ever
    /// called, and broke silently (`text: None`, nothing sent). It must
    /// still get an ordinary reply: the budget is rounds *this turn*.
    ///
    /// The cap is pinned here (three, via `for_test_scripted_with_rounds`)
    /// rather than inherited from `ToolRounds::default()`. A test that
    /// borrows the shipped default only discriminates for as long as that
    /// default happens to stay below the seeded history's length — it
    /// silently stops catching the regression the moment the default
    /// moves past it, which is exactly what happened when the unattended
    /// default became 25 (`for_test`'s cap) while this test still seeded
    /// only 11: the reintroduced bug would start `round` at 11, still
    /// under 25, and the turn would reply normally either way, so the
    /// test would pass with or without the bug. Seeding 11 pairs against
    /// an explicit cap of 3 keeps the history well past the budget no
    /// matter what the shipped default is: if `round` is ever seeded from
    /// history again, it starts at 11, the very first check trips
    /// (`11 >= 3`), and the turn comes back `BudgetExhausted` instead of
    /// `Replied` — this test would then fail, which is the signal it
    /// exists to give.
    #[tokio::test]
    async fn a_session_restored_with_more_than_the_round_budget_still_replies() {
        let state = ServeState::for_test_scripted_with_rounds(
            true,
            vec![crate::provider::ChatResponse {
                prompt_usage: None,
                text: Some("ok".to_string()),
                tool_calls: Vec::new(),
                stop_reason: None,
            }],
            crate::config::ToolRounds {
                interactive: 3,
                unattended: 3,
            },
        );

        // More than the pinned round budget's worth of assistant messages
        // carrying a ToolUse part, paired with their tool_results, as a
        // restored session's history would arrive already hydrated.
        let mut history = Vec::new();
        for i in 0..11 {
            let id = format!("old-{i}");
            history.push(ChatMessage::assistant_with_tools(
                None,
                vec![crate::provider::ToolCall {
                    id: id.clone(),
                    name: "file_read".to_string(),
                    input: json!({}),
                }],
            ));
            history.push(ChatMessage::tool_results_with_images(
                vec![(id, "contents".to_string())],
                Vec::new(),
            ));
        }
        state
            .sessions
            .lock()
            .await
            .insert("s-restored".to_string(), history);

        let outcome = run_llm_turn(
            Arc::clone(&state),
            "s-restored".to_string(),
            ChatMessage::user("still there?"),
            Arc::new(NullProgress),
            None,
        )
        .await;

        assert!(
            matches!(outcome.stop, TurnStop::Replied),
            "a fresh turn on a restored session must reach the provider \
             and reply normally, not exhaust its budget on history from \
             before this turn"
        );
        assert_eq!(outcome.text.as_deref(), Some("ok"));
    }

    /// `handle_get_session` must hand back the record exactly as written —
    /// not `load_session`'s model view, which trims everything a
    /// checkpoint covers and prefixes a synthetic summary stub the client
    /// never sent. This was already gotten wrong once during this
    /// branch's development (see 789c5f0) and nothing else pins it:
    /// swapping `load_session_full` for `load_session` here still
    /// compiles and still passes the rest of the suite.
    #[tokio::test]
    async fn get_session_returns_the_full_record_even_past_a_checkpoint() {
        let state = ServeState::for_test(true);
        let sid = "get-session-full".to_string();
        let key: ConversationKey = (sid.clone(), None);
        state
            .cross_device_session_store
            .ensure_session(&sid, &key, "rpc", None, "default")
            .unwrap();

        state
            .cross_device_session_store
            .append(&sid, &ChatMessage::user("first"))
            .unwrap();
        state
            .cross_device_session_store
            .append(&sid, &ChatMessage::assistant("second"))
            .unwrap();

        // A checkpoint with keep_recent = 0 covers everything appended so
        // far, the same shape a day-boundary compaction writes.
        state
            .cross_device_session_store
            .append_summary(&sid, "earlier stuff happened", 0)
            .unwrap();

        state
            .cross_device_session_store
            .append(&sid, &ChatMessage::user("third"))
            .unwrap();

        let response = handle_get_session(Arc::clone(&state), json!(1), Some(sid.clone())).await;
        let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        let messages = v["result"]["messages"].as_array().expect("messages array");

        assert_eq!(
            messages.len(),
            3,
            "the endpoint must return every message as written, not the \
             model's post-checkpoint view: {v}"
        );
        let has_stub = messages.iter().any(|m| {
            m["parts"].as_array().is_some_and(|parts| {
                parts.iter().any(|p| {
                    p["type"] == "text"
                        && p["text"]
                            .as_str()
                            .is_some_and(|t| t.starts_with("[Context Summary"))
                })
            })
        });
        assert!(
            !has_stub,
            "the synthetic compaction stub must never appear in a \
             client-facing get_session response: {v}"
        );
    }

    /// `0` は無制限、それ以外はその数。二段階の写像を一箇所に留める。
    #[test]
    fn a_zero_budget_means_unbounded() {
        use crate::config::ToolRounds;
        let rounds = ToolRounds {
            interactive: 0,
            unattended: 25,
        };
        assert_eq!(rounds.limit(RoundBudget::Interactive), None);
        assert_eq!(rounds.limit(RoundBudget::Unattended), Some(25));
    }

    /// 中間テキストは溜められるのではなく、そのラウンドで渡される。ここが
    /// この変更の全部である。ツールを呼ばない最後の応答のテキストも同じ口を
    /// 通るので、ホストから見たテキストの並びは会話そのものになる。
    #[tokio::test]
    async fn text_reaches_the_host_round_by_round() {
        #[derive(Default)]
        struct ChunkRecorder {
            chunks: std::sync::Mutex<Vec<String>>,
        }
        #[async_trait::async_trait]
        impl TurnHost for ChunkRecorder {
            async fn tool_start(&self, _id: &str, _name: &str, _input: &Value) {}
            async fn tool_end(&self, _id: &str, _name: &str) {}
            async fn turn_error(&self, _message: &str) {}
            async fn message_chunk(&self, text: &str) {
                self.chunks.lock().unwrap().push(text.to_string());
            }
        }

        let state = ServeState::for_test_scripted(
            false,
            vec![
                crate::provider::ChatResponse {
                    prompt_usage: None,
                    text: Some("looking now".to_string()),
                    tool_calls: vec![crate::provider::ToolCall {
                        id: "call-1".to_string(),
                        name: "echo".to_string(),
                        input: json!({ "text": "ping" }),
                    }],
                    stop_reason: None,
                },
                crate::provider::ChatResponse {
                    prompt_usage: None,
                    text: Some("found it".to_string()),
                    tool_calls: Vec::new(),
                    stop_reason: None,
                },
            ],
        );
        let host = Arc::new(ChunkRecorder::default());
        let outcome = run_llm_turn(
            Arc::clone(&state),
            "s-chunks".to_string(),
            ChatMessage::user("go"),
            Arc::clone(&host) as Arc<dyn TurnHost>,
            None,
        )
        .await;

        assert_eq!(
            *host.chunks.lock().unwrap(),
            vec!["looking now".to_string(), "found it".to_string()]
        );
        // `outcome.text` は据え置き。これを読む `/rpc`・A2A・音声が壊れない
        // ことが、この変更が既定 no-op で足りる理由である。
        assert_eq!(outcome.text.as_deref(), Some("looking now\n\nfound it"));
    }

    /// 空のテキストは渡さない。ツールだけ呼ぶラウンドで空メッセージが
    /// 流れると、チャンネル経路では空の吹き出しになる。
    #[tokio::test]
    async fn an_empty_text_is_not_handed_to_the_host() {
        #[derive(Default)]
        struct ChunkRecorder {
            chunks: std::sync::Mutex<Vec<String>>,
        }
        #[async_trait::async_trait]
        impl TurnHost for ChunkRecorder {
            async fn tool_start(&self, _id: &str, _name: &str, _input: &Value) {}
            async fn tool_end(&self, _id: &str, _name: &str) {}
            async fn turn_error(&self, _message: &str) {}
            async fn message_chunk(&self, text: &str) {
                self.chunks.lock().unwrap().push(text.to_string());
            }
        }

        let state = ServeState::for_test_scripted(
            false,
            vec![
                crate::provider::ChatResponse {
                    prompt_usage: None,
                    text: None,
                    tool_calls: vec![crate::provider::ToolCall {
                        id: "call-1".to_string(),
                        name: "echo".to_string(),
                        input: json!({ "text": "ping" }),
                    }],
                    stop_reason: None,
                },
                crate::provider::ChatResponse {
                    prompt_usage: None,
                    text: Some("done".to_string()),
                    tool_calls: Vec::new(),
                    stop_reason: None,
                },
            ],
        );
        let host = Arc::new(ChunkRecorder::default());
        let _ = run_llm_turn(
            Arc::clone(&state),
            "s-empty".to_string(),
            ChatMessage::user("go"),
            Arc::clone(&host) as Arc<dyn TurnHost>,
            None,
        )
        .await;

        assert_eq!(*host.chunks.lock().unwrap(), vec!["done".to_string()]);
    }

    /// 既定 `Unattended` のホストは、設定した数で打ち切られる。上限が
    /// ハードコードではなく config から来ていることを、10 以外の数で示す。
    #[tokio::test]
    async fn an_unattended_host_stops_at_the_configured_budget() {
        use crate::config::ToolRounds;

        let script: Vec<crate::provider::ChatResponse> = (0..3)
            .map(|i| crate::provider::ChatResponse {
                prompt_usage: None,
                text: Some(format!("step {i}")),
                tool_calls: vec![crate::provider::ToolCall {
                    id: format!("call-{i}"),
                    name: "echo".to_string(),
                    input: json!({ "text": "ping" }),
                }],
                stop_reason: None,
            })
            .collect();
        let state = ServeState::for_test_scripted_with_rounds(
            false,
            script,
            ToolRounds {
                interactive: 0,
                unattended: 3,
            },
        );
        let outcome = run_llm_turn(
            Arc::clone(&state),
            "s-budget".to_string(),
            ChatMessage::user("go"),
            Arc::new(NullProgress) as Arc<dyn TurnHost>,
            None,
        )
        .await;

        assert!(
            matches!(outcome.stop, TurnStop::BudgetExhausted { .. }),
            "3 ラウンド使い切ったら打ち切られる"
        );
    }

    /// `interactive = 0` のホストは、既定の10ラウンドを超えても回り続ける。
    /// スクリプトは12本 — 11本目に到達する時点で、旧来の上限は破れている。
    #[tokio::test]
    async fn an_interactive_host_runs_past_the_old_hard_coded_ten() {
        use crate::config::ToolRounds;

        struct InteractiveHost;
        #[async_trait::async_trait]
        impl TurnHost for InteractiveHost {
            async fn tool_start(&self, _id: &str, _name: &str, _input: &Value) {}
            async fn tool_end(&self, _id: &str, _name: &str) {}
            async fn turn_error(&self, _message: &str) {}
            fn round_budget(&self) -> RoundBudget {
                RoundBudget::Interactive
            }
        }

        let mut script: Vec<crate::provider::ChatResponse> = (0..12)
            .map(|i| crate::provider::ChatResponse {
                prompt_usage: None,
                text: None,
                tool_calls: vec![crate::provider::ToolCall {
                    id: format!("call-{i}"),
                    name: "echo".to_string(),
                    input: json!({ "text": "ping" }),
                }],
                stop_reason: None,
            })
            .collect();
        script.push(crate::provider::ChatResponse {
            prompt_usage: None,
            text: Some("finished".to_string()),
            tool_calls: Vec::new(),
            stop_reason: None,
        });

        let state = ServeState::for_test_scripted_with_rounds(
            false,
            script,
            ToolRounds {
                interactive: 0,
                unattended: 25,
            },
        );
        let outcome = run_llm_turn(
            Arc::clone(&state),
            "s-unbounded".to_string(),
            ChatMessage::user("go"),
            Arc::new(InteractiveHost) as Arc<dyn TurnHost>,
            None,
        )
        .await;

        assert_eq!(outcome.text.as_deref(), Some("finished"));
    }

    /// The autonomous store owns its sessions, and `store_for_session`
    /// has to say so — otherwise every `append` in an autonomous turn
    /// lands in the cross-device tree and the session is never found
    /// again.
    #[tokio::test]
    async fn an_autonomous_session_resolves_to_the_autonomous_store() {
        let state = ServeState::for_test(false);
        let sid = state
            .autonomous_session_store
            .create_autonomous_session("refactor", "default")
            .unwrap();

        let store = state.store_for_session(&sid);
        assert!(Arc::ptr_eq(store, &state.autonomous_session_store));
        assert!(!Arc::ptr_eq(store, &state.cross_device_session_store));
    }

    /// The default host is `Trusted`, and `round_budget` is left at the
    /// `Unattended` default — nobody can cancel an autonomous turn, so
    /// its budget must stay finite.
    #[test]
    fn the_autonomous_host_is_trusted_and_unattended() {
        let host = AutonomousHost {
            origin: crate::tools::policy::Origin::Trusted,
        };
        assert_eq!(host.origin(), crate::tools::policy::Origin::Trusted);
        assert_eq!(host.round_budget(), RoundBudget::Unattended);

        let host = AutonomousHost {
            origin: crate::tools::policy::Origin::Channel,
        };
        assert_eq!(host.origin(), crate::tools::policy::Origin::Channel);
    }
}
