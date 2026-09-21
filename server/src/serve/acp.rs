//! Agent Client Protocol over WebSocket, at `GET /acp`.
//!
//! Auth: `Authorization: Bearer <token>` on the upgrade request, resolved
//! through `DeviceAuth` — the same mechanism as `/a2a` and `/mcp`. The
//! match resolves the room profile the ACP session runs under, so a
//! dedicated Zed token can pin the editor to its own profile, provider
//! and memory namespace.
//!
//! Rejection happens *before* the 101: an error delivered after a successful
//! upgrade reaches the operator as an unexplained disconnect, whereas a
//! status code reaches them through `websocat`.
//!
//! Framing follows the ACP transport RFD: one JSON-RPC message per WebSocket
//! text frame. That is exactly the newline framing the SDK's `Lines`
//! transport expects, so [`lines_transport`] adapts the socket without
//! reframing anything.
//!
//! `initialize`, `session/new`, `session/prompt`, `session/cancel`,
//! `session/list`, `session/load`, `session/resume` and `session/set_mode`
//! are answered here.
//!
//! A prompt is not implemented here beyond its ACP shape: it extracts the
//! text, hands the turn to [`super::run_llm_turn`] — the same executor
//! behind `/rpc`, voice and A2A — and translates that turn's progress back
//! into `session/update` notifications. There is deliberately no second
//! tool loop or history handling on this path. Persistence, though,
//! lands in `acp_session_store`, ACP's own store — separate from every
//! other transport's `SessionStore` (see `acp_session.rs`'s module doc
//! for why) — while still writing under the same memory namespace and
//! system prompt as every other transport's.
//!
//! A turn does *not* run in the handler that received it. The SDK's request
//! callbacks hold its dispatch loop, which parses no further frame on the
//! connection until they return, so awaiting a turn there would make the
//! `session/cancel` meant to stop it unreadable until the turn had already
//! finished. Instead the handler spawns the turn with `ConnectionTo::spawn`
//! and sends the `Responder` along with it — see the `session/prompt`
//! handler. The visible consequence is that prompts on one connection run
//! concurrently rather than one after another.
//!
//! Two caveats come with that, both about history, and both worth knowing
//! before building on this:
//!
//! - **Concurrent prompts on *one session* are not history-safe.**
//!   [`super::run_llm_turn`] clones the session's history at the top and
//!   writes the whole vector back at the end, so two overlapping turns on
//!   the same session are last-writer-wins in memory — while both have
//!   already appended their user messages to JSONL. The durable transcript
//!   and the in-memory history diverge. The race is not ACP's: `/rpc` and
//!   the voice heartbeat path can already reach it. What is new here is
//!   that concurrency became *documented* rather than accidental, so this
//!   says plainly that concurrent prompts on separate sessions are fine and
//!   concurrent prompts on one session are not.
//! - **A cancelled turn's future is dropped**, and `run_llm_turn` was not
//!   written to be. Its user message is already on disk; its write-back to
//!   `state.sessions` never runs. So a cancelled prompt is missing from the
//!   next turn's model context but present in `list_sessions` and after a
//!   restart. See the drop note on [`super::run_llm_turn`] itself.

use super::{ServeState, extract_bearer};
use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::{
    AgentCapabilities, CancelNotification, ClientCapabilities, ContentBlock, ContentChunk,
    CreateTerminalRequest, CurrentModeUpdate, Error, Implementation, InitializeRequest,
    InitializeResponse, KillTerminalRequest, ListSessionsRequest, ListSessionsResponse,
    LoadSessionRequest, LoadSessionResponse, NewSessionRequest, NewSessionResponse,
    PermissionOption, PermissionOptionKind, PromptRequest, PromptResponse, ReadTextFileRequest,
    ReleaseTerminalRequest, RequestPermissionOutcome, RequestPermissionRequest,
    ResumeSessionRequest, ResumeSessionResponse, SessionCapabilities, SessionId, SessionInfo,
    SessionListCapabilities, SessionMode as AcpSessionMode, SessionModeState, SessionNotification,
    SessionResumeCapabilities, SessionUpdate, SetSessionModeRequest, SetSessionModeResponse,
    StopReason, TerminalId, TerminalOutputRequest, TextContent, ToolCall as AcpToolCall,
    ToolCallId, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields, WaitForTerminalExitRequest,
    WriteTextFileRequest,
};
use agent_client_protocol::{
    Agent, Client, ConnectionTo, Lines, on_receive_notification, on_receive_request,
};
use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use futures_util::{SinkExt, StreamExt};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

/// One ACP session, mapped onto an agent session.
struct AcpSession {
    /// The agent-side session id, minted the same way `handle_initialize`
    /// (`src/serve/mod.rs`) mints one for a brand-new `/rpc` session:
    /// `uuid::Uuid::now_v7().to_string()`. This is also the key under which
    /// `state.session_room_profiles` pins the session to a room profile, so
    /// it is what routes this ACP session through the same namespace chain
    /// and provider selection every other transport uses. It duplicates the
    /// map key (a `SessionId` wrapping this same string) because the
    /// `session/prompt` handler needs the plain `String` to call
    /// `run_llm_turn`, which does not know about ACP's `SessionId` type.
    agent_session_id: String,
    /// The client's workspace root, as reported in `session/new`'s `cwd`
    /// field. This is an absolute path on the *client's* machine, not this
    /// server's — nothing in this phase may canonicalise it, check it
    /// exists, or otherwise treat it as a local path. It is recorded now
    /// because a later phase needs it as the default working directory for
    /// client-side terminals (`terminal/create`); until then it is inert.
    ///
    /// Recorded straight from `req.cwd` on both `session/new` and
    /// `session/load`/`session/resume` — which does NOT feed it back into
    /// the store: `AcpSessionStore::create` only writes a session's cwd
    /// once, at creation, so a loaded session's recorded cwd here cannot
    /// be used to update the stored one, and no code should try.
    ///
    /// Read at `session/prompt` time and copied onto the turn's
    /// [`AcpProgress`] the same way `client_capabilities` is, so
    /// the client shell path (`src/tools/client_tools.rs`) can default a
    /// `terminal/create` call's working directory to it when the model
    /// doesn't pass one.
    cwd: PathBuf,
    /// What the client declared it can do, from `initialize`'s
    /// `clientCapabilities`. Copied in from the connection-wide
    /// [`AcpSessions::client_capabilities`] at `session/new` and at
    /// `adopt_session` time, so a client-side tool can read it straight
    /// off the session it is running under rather than reaching back
    /// into connection state.
    ///
    /// Read at `session/prompt` time and copied onto the turn's
    /// [`AcpProgress`], whose `client_fs_caps` is what makes the two
    /// flags observable in `visible_tool_predicate`
    /// (`src/serve/mod.rs`).
    client_capabilities: ClientCapabilities,
    /// Cancellation tokens for the turns *currently in flight* on this
    /// session, keyed by their connection-wide turn number.
    ///
    /// Every live turn, not just the newest. Prompts on one connection run
    /// concurrently (see the `session/prompt` handler), so a client can have
    /// two turns open on one session, and `session/cancel` is scoped to the
    /// session rather than to a request — the schema calls it "ongoing
    /// operations for a session". Keeping only the latest token would leave
    /// an older turn calling the provider until the connection died and then
    /// answering `EndTurn`, and `Cancelled` is a MUST.
    ///
    /// A fresh token per turn rather than one per session, because a
    /// `CancellationToken` stays cancelled for good: reusing one would make
    /// every prompt after a cancel come straight back as `cancelled` without
    /// the provider ever being called. Each is a child of the connection's
    /// token, which is how one vanished client stops every turn at once.
    ///
    /// Entries are removed by the turn that owns them, so this holds only
    /// turns that are still running.
    turns: HashMap<u64, CancellationToken>,
    /// The permission mode this session is in. Per session, not per
    /// connection: two sessions on one socket are judged separately.
    mode: crate::tools::policy::SessionMode,
}

/// Sessions live for the lifetime of one ACP connection: a `HashMap` behind
/// a `tokio::sync::Mutex`, shared across this connection's request handlers
/// via `Arc`.
#[derive(Default)]
struct AcpSessions {
    inner: tokio::sync::Mutex<HashMap<SessionId, AcpSession>>,
    /// Hands every turn on this connection a number, so a turn that finishes
    /// can remove exactly its own token — `CancellationToken` has no identity
    /// to match on, and "the newest" is not the answer once turns overlap.
    next_turn: std::sync::atomic::AtomicU64,
    /// What the client told `initialize` it can do. Set once, by the
    /// `initialize` handler, and read by `session/new` and
    /// `adopt_session` to stamp a copy onto each [`AcpSession`] — the
    /// capability is a property of the client at the other end of this
    /// connection, not of any one session, so there is exactly one
    /// connection-wide copy rather than one per session that could
    /// drift from it.
    client_capabilities: std::sync::Mutex<ClientCapabilities>,
}

/// Reports one prompt turn's progress to an ACP client as `session/update`
/// notifications, and remembers why a turn failed.
///
/// The remembering is not incidental: [`super::LlmTurnOutcome`] carries only
/// the final text, so a failed turn reaches the prompt handler as `None` with
/// no cause attached, and `turn_error` is the only place the provider's
/// message is ever offered. Stashing it here is what lets the JSON-RPC error
/// say *what* went wrong rather than merely *that* something did.
struct AcpProgress {
    session_id: SessionId,
    /// The connection this turn is running on. `ConnectionTo<Client>` is the
    /// handle the SDK passes to every request handler; it is `Clone`, so the
    /// turn keeps its own to push notifications through while it runs.
    connection: ConnectionTo<Client>,
    /// The message from the most recent `turn_error`. A blocking mutex is
    /// enough: it is never held across an await.
    error: std::sync::Mutex<Option<String>>,
    /// The room profile this connection's bearer token resolved to.
    /// Standing permission answers are recorded under it.
    profile: String,
    /// The session's mode as of the moment this turn started. Copied
    /// rather than shared: a `session/set_mode` arriving mid-turn must
    /// not change the rules under a call already being judged.
    mode: crate::tools::policy::SessionMode,
    permissions: Arc<super::acp_permissions::PermissionStore>,
    /// This turn's session's recorded `client_capabilities`
    /// ([`AcpSession::client_capabilities`]), copied in at construction
    /// time rather than looked up again mid-turn. `client_fs_caps`
    /// reads `fs.read_text_file`/`fs.write_text_file` off this, and
    /// `client_terminal_cap` reads `terminal` off it the same way.
    client_capabilities: ClientCapabilities,
    /// This turn's session's recorded [`AcpSession::cwd`], copied in at
    /// construction time the same way `client_capabilities` is. Two uses:
    /// handed to `AcpClientHandle` by `acp_client()` below, which is what
    /// lets the client shell path default a terminal's working directory to the
    /// session's; and exposed to `run_llm_turn` through the `TurnHost::cwd`
    /// override below, which injects it into the session's system prompt.
    cwd: PathBuf,
    /// The whole host's terminal registry (`ServeState.acp_terminals`),
    /// cloned in at construction time. Handed to `AcpClientHandle` by
    /// `acp_client()` below, which is what lets the client-shell tools
    /// (`src/tools/client_tools.rs`) track and cap terminals per
    /// session without this struct or `AcpClientHandle` holding the
    /// whole `ServeState`.
    terminals: crate::tools::acp_client::TerminalRegistry,
}

/// The `(read, write)` pair `TurnHost::client_fs_caps` reports, pulled
/// out of `AcpProgress::client_fs_caps` into its own free function so a
/// test can call it directly against a hand-built `ClientCapabilities`
/// — the only way to prove these two lines actually read the recorded
/// value rather than, say, returning a constant or swapping the two
/// fields. `AcpProgress` itself is built at exactly one call site
/// (`serve_connection`'s `session/prompt` handler) and never in a test,
/// so a test that only reached `AcpProgress::client_fs_caps` through
/// `&self` would need one anyway; this is the smaller thing to build.
///
/// The two flags are read independently — never folded into one "fs"
/// bit — because a client can implement `fs/read_text_file` without
/// `fs/write_text_file`, or vice versa.
fn fs_caps_from(caps: &ClientCapabilities) -> (bool, bool) {
    (caps.fs.read_text_file, caps.fs.write_text_file)
}

/// The flag `TurnHost::client_terminal_cap` reports, pulled out for the
/// same reason as `fs_caps_from` above: a test can call it directly
/// against a hand-built `ClientCapabilities` without constructing an
/// `AcpProgress`.
fn terminal_cap_from(caps: &ClientCapabilities) -> bool {
    caps.terminal
}

/// The capability line logged when a client initializes.
///
/// These three flags decide which tools the model is offered at all, and
/// after #270 that is still the whole story for an ACP turn: the shared
/// `file_read` / `file_write` / `file_append` names need their flag, the
/// names that reach the editor's shell — `shell`, `file_delete`,
/// `dir_list`, `dir_walk` and the three `shell_*` lifecycle tools —
/// need the terminal one, and through it come all four skill tools
/// (`visible_tool_predicate`, `src/serve/mod.rs`). When a tool is missing
/// there are exactly two possible reasons, and this was the invisible
/// one: the namespace's `skills` flag is in a file the operator can read,
/// while the capabilities arrive on the wire and were recorded without
/// ever being shown. Diagnosing a missing `skill_install` meant reading
/// the editor's source or guessing.
///
/// Rendered by a free function, like `fs_caps_from` above and for the
/// same reason: a test can pin the text against a hand-built
/// `ClientCapabilities` without standing up a connection, which is the
/// only way to catch the two `fs` flags being printed the wrong way
/// round.
fn describe_capabilities(caps: &ClientCapabilities) -> String {
    let (read, write) = fs_caps_from(caps);
    format!(
        "fs.read={read} fs.write={write} terminal={}",
        terminal_cap_from(caps)
    )
}

/// How the log names the peer.
///
/// `client_info` is optional in ACP v1 — the schema notes it becomes
/// required in a later version — so a client that omits it has to be
/// named as unidentified rather than logged as an empty string, which
/// would read as a bug in this line rather than an omission by the
/// client. `title` is the human-facing name when the client sends one,
/// with `name` (the programmatic id) as the documented fallback.
fn describe_client(info: Option<&Implementation>) -> String {
    match info {
        Some(info) => format!(
            "{} {}",
            info.title.as_deref().unwrap_or(&info.name),
            info.version
        ),
        None => "an unidentified client".to_string(),
    }
}

/// The working directory `AcpClientHandle::create_terminal` sends on
/// the wire: the model's explicit `cwd` when it gave one, else the
/// turn's session cwd. Pulled out for the same reason as
/// `fs_caps_from`/`terminal_cap_from` above — this is a two-way branch
/// picking between two paths, and inlined it is easy to get backwards
/// (session cwd when the model *did* supply one) without a test able to
/// tell the difference, since a real session's cwd usually differs from
/// wherever the model happens to ask for anyway.
fn effective_cwd(requested: Option<&str>, session_cwd: &Path) -> PathBuf {
    requested
        .map(PathBuf::from)
        .unwrap_or_else(|| session_cwd.to_path_buf())
}

impl AcpProgress {
    #[allow(clippy::too_many_arguments)]
    fn new(
        session_id: SessionId,
        connection: ConnectionTo<Client>,
        profile: String,
        mode: crate::tools::policy::SessionMode,
        permissions: Arc<super::acp_permissions::PermissionStore>,
        client_capabilities: ClientCapabilities,
        cwd: PathBuf,
        terminals: crate::tools::acp_client::TerminalRegistry,
    ) -> Self {
        Self {
            session_id,
            connection,
            error: std::sync::Mutex::new(None),
            profile,
            mode,
            permissions,
            client_capabilities,
            cwd,
            terminals,
        }
    }

    /// Push one `session/update` for this turn's session.
    ///
    /// A send failure means the client is already gone, which the connection
    /// task notices on its own; there is nothing useful for a turn in flight
    /// to do about it beyond saying so.
    fn notify(&self, update: SessionUpdate) {
        if let Err(e) = self
            .connection
            .send_notification(SessionNotification::new(self.session_id.clone(), update))
        {
            warn!(
                "ACP: dropped a session/update for session {}: {e}",
                self.session_id
            );
        }
    }

    /// The reason the turn failed, if it reported one.
    fn failure(&self) -> Option<String> {
        self.error.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl super::TurnHost for AcpProgress {
    /// The provider's own tool-call id becomes ACP's `toolCallId`, so the
    /// completion below can name the call it completes. There is no input to
    /// report — `TurnHost` does not carry one — so the tool's name serves
    /// as the title.
    ///
    /// `Pending`, not `InProgress`. This fires *before* the permission
    /// gate, so at this moment the call may be waiting on the user's
    /// answer, or about to be refused outright — which is exactly what
    /// `Pending` means. It said `InProgress` when nothing could stand
    /// between the executor and the call; that stopped being true when
    /// the gate landed.
    async fn tool_start(&self, id: &str, name: &str) {
        self.notify(SessionUpdate::ToolCall(
            AcpToolCall::new(ToolCallId::new(id), name).status(ToolCallStatus::Pending),
        ));
    }

    /// Only the status changes: everything else the client already has from
    /// `tool_start`, and `ToolCallUpdate` carries just what moved.
    async fn tool_end(&self, id: &str, _name: &str) {
        self.notify(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
            ToolCallId::new(id),
            ToolCallUpdateFields::new().status(ToolCallStatus::Completed),
        )));
    }

    /// Recorded rather than sent: ACP reports a failed turn by answering the
    /// `session/prompt` request with a JSON-RPC error, and this is the only
    /// place the cause is offered to put in it.
    async fn turn_error(&self, message: &str) {
        *self.error.lock().unwrap() = Some(message.to_string());
    }

    /// Sent the moment the round that produced it is done, rather than
    /// held until the turn ends.
    ///
    /// One chunk per round, not per token: `Provider::chat` returns a
    /// whole response, so a round is the finest boundary that actually
    /// exists. Splitting further would invent boundaries the model never
    /// produced.
    ///
    /// This is the *only* way text reaches an ACP client now. The turn's
    /// `outcome.text` is deliberately ignored on this path — see the
    /// `session/prompt` handler — because everything in it has already
    /// come through here, and sending it again would duplicate the whole
    /// reply.
    async fn message_chunk(&self, text: &str) {
        self.notify(SessionUpdate::AgentMessageChunk(ContentChunk::new(
            ContentBlock::Text(TextContent::new(text.to_string())),
        )));
    }

    /// ACP is the one route that can stop a turn in flight:
    /// `session/cancel` is implemented, and the editor's Escape sends it.
    /// That is what makes an unbounded budget safe to offer here and
    /// nowhere else.
    fn round_budget(&self) -> super::RoundBudget {
        super::RoundBudget::Interactive
    }

    fn origin(&self) -> crate::tools::policy::Origin {
        crate::tools::policy::Origin::Acp(self.mode)
    }

    /// The client-side tools reach the editor through this. Built fresh
    /// from this turn's own `session_id` and `connection` rather than
    /// wrapping `self`: `AcpProgress` carries turn-scoped state
    /// (`error`, `permissions`, `mode`) that a client-side tool has no
    /// business touching, and `Arc<Self>` is not reachable from `&self`
    /// here anyway — `AcpProgress` is not `Clone` (its `error` mutex
    /// isn't), so a small handle holding just what `AcpClient` needs is
    /// both the simplest thing that works and the right boundary.
    fn acp_client(&self) -> Option<Arc<dyn crate::tools::acp_client::AcpClient>> {
        Some(Arc::new(AcpClientHandle {
            session_id: self.session_id.clone(),
            connection: self.connection.clone(),
            cwd: self.cwd.clone(),
            terminals: Arc::clone(&self.terminals),
        }))
    }

    /// The session's recorded cwd, verbatim — `AcpSession::cwd`'s doc
    /// spells out why it must not be canonicalised or existence-checked
    /// here: it is a path on the *client's* machine, and this process is
    /// not on it.
    fn cwd(&self) -> Option<String> {
        Some(self.cwd.to_string_lossy().into_owned())
    }

    /// Read straight off this turn's recorded `client_capabilities`,
    /// via `fs_caps_from` — see that function's doc for why the read
    /// is pulled out rather than written here.
    fn client_fs_caps(&self) -> (bool, bool) {
        fs_caps_from(&self.client_capabilities)
    }

    /// Whether this turn's editor declared `terminal/*` support, via
    /// `terminal_cap_from` — see that function's doc for why the read
    /// is pulled out rather than written here.
    fn client_terminal_cap(&self) -> bool {
        terminal_cap_from(&self.client_capabilities)
    }

    /// Move a call from `Pending` to `InProgress`.
    ///
    /// `tool_start` fires before the permission gate, so every call
    /// begins as `Pending` — which is accurate then, and wrong the
    /// moment the call is actually running. Without this edge a
    /// permitted call would sit at `Pending` for its whole runtime and
    /// then jump to `Completed`, telling the user it was still waiting
    /// on them while it ran.
    async fn tool_allowed(&self, id: &str) {
        self.notify(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
            ToolCallId::new(id),
            ToolCallUpdateFields::new().status(ToolCallStatus::InProgress),
        )));
    }

    /// Put the call to the user, unless a standing answer settles it.
    ///
    /// The standing answer is consulted here rather than inside
    /// `decide` because `decide` is a pure function over the policy
    /// table and knows nothing about what this host has been told
    /// before.
    async fn approve(
        &self,
        call: &crate::provider::ToolCall,
        kind: crate::tools::ToolKind,
    ) -> crate::tools::policy::Approval {
        use crate::tools::policy::Approval;

        // A standing answer settles it without a round trip. Moving the
        // call off `Pending` is not done here: the gate calls
        // `tool_allowed` for every permitted call, asked or not.
        match self.permissions.standing(&self.profile, &call.name) {
            Some(true) => return Approval::AllowAlways,
            Some(false) => return Approval::RejectAlways,
            None => {}
        }

        let request = RequestPermissionRequest::new(
            self.session_id.clone(),
            ToolCallUpdate::new(
                ToolCallId::new(call.id.as_str()),
                ToolCallUpdateFields::new()
                    .title(call.name.clone())
                    .kind(kind)
                    .raw_input(call.input.clone()),
            ),
            vec![
                PermissionOption::new("allow_once", "Allow once", PermissionOptionKind::AllowOnce),
                PermissionOption::new(
                    "allow_always",
                    "Always allow this tool",
                    PermissionOptionKind::AllowAlways,
                ),
                PermissionOption::new("reject_once", "Reject", PermissionOptionKind::RejectOnce),
                PermissionOption::new(
                    "reject_always",
                    "Never allow this tool",
                    PermissionOptionKind::RejectAlways,
                ),
            ],
        );

        // `block_task`, and it is only sound because of where this
        // runs. The SDK's own documentation says never to await a sent
        // request inside a handler — the dispatch loop would stop
        // parsing frames and the client's answer could never arrive, so
        // it deadlocks. `approve` is called from `run_llm_turn`, which
        // the `session/prompt` handler hands to `ConnectionTo::spawn`
        // precisely so the turn runs outside that loop. That is the
        // case the SDK marks as safe.
        let answer = match self.connection.send_request(request).block_task().await {
            Ok(a) => a,
            Err(e) => {
                // The client went away, or refused the method. Either
                // way nobody said yes, and running unguarded because
                // the question failed to arrive is the wrong direction
                // to fail in.
                warn!(
                    "ACP: could not ask about '{}' on session {}: {e}. Treating as declined.",
                    call.name, self.session_id
                );
                return Approval::RejectOnce;
            }
        };

        let approval = match answer.outcome {
            // The turn is being cancelled; the cancel path answers the
            // prompt with `Cancelled`, so this call must simply not run.
            RequestPermissionOutcome::Cancelled => Approval::RejectOnce,
            RequestPermissionOutcome::Selected(selected) => match selected.option_id.0.as_ref() {
                "allow_once" => Approval::AllowOnce,
                "allow_always" => Approval::AllowAlways,
                "reject_always" => Approval::RejectAlways,
                "reject_once" => Approval::RejectOnce,
                other => {
                    warn!(
                        "ACP: unknown permission option '{other}' for '{}'; treating as declined.",
                        call.name
                    );
                    Approval::RejectOnce
                }
            },
            // `RequestPermissionOutcome` is `#[non_exhaustive]`, so a
            // future ACP version can add an outcome this build has
            // never heard of. Nobody said yes, so nothing runs.
            _ => {
                warn!(
                    "ACP: unrecognised permission outcome for '{}'; treating as declined.",
                    call.name
                );
                Approval::RejectOnce
            }
        };

        if approval.is_sticky() {
            self.permissions.record(&self.profile, &call.name, approval);
        }
        approval
    }
}

/// What `AcpClient`'s methods need to reach the editor: which session
/// to name on the wire, and the connection to send on. Cheap to clone
/// (both fields are), so [`AcpProgress::acp_client`] hands out a fresh
/// one rather than sharing state with the turn it was built from.
#[derive(Clone)]
struct AcpClientHandle {
    session_id: SessionId,
    connection: ConnectionTo<Client>,
    /// This turn's session's cwd, copied in from [`AcpProgress::cwd`].
    /// `create_terminal` falls back to it when called with `cwd: None`
    /// (the system-prompt injection goes through `TurnHost::cwd` on
    /// `AcpProgress` itself, not through this handle).
    cwd: PathBuf,
    /// Shared with `ServeState.acp_terminals` via [`AcpProgress::terminals`].
    /// `try_reserve_terminal_slot`/`track_terminal`/`untrack_terminal`
    /// below key into it with `session_id.to_string()` — the same string
    /// `AcpSession::agent_session_id` holds for this session, since a
    /// `SessionId` is minted from exactly that string.
    terminals: crate::tools::acp_client::TerminalRegistry,
}

#[async_trait::async_trait]
impl crate::tools::acp_client::AcpClient for AcpClientHandle {
    async fn read_text_file(
        &self,
        path: &str,
        line: Option<u32>,
        limit: Option<u32>,
    ) -> anyhow::Result<String> {
        let request = ReadTextFileRequest::new(self.session_id.clone(), path)
            .line(line)
            .limit(limit);
        // Not awaited inside a request handler: `AcpClientHandle` is
        // only ever reached through the `current_acp_client` task-local,
        // which is scoped around tool execution inside `run_llm_turn` —
        // the same "outside the dispatch loop" position `approve` above
        // relies on, and for the same reason: awaiting a sent request
        // inside a handler would stop the dispatch loop from parsing
        // the client's answer.
        let response = self.connection.send_request(request).block_task().await?;
        Ok(response.content)
    }

    async fn write_text_file(&self, path: &str, content: &str) -> anyhow::Result<()> {
        let request = WriteTextFileRequest::new(self.session_id.clone(), path, content);
        self.connection.send_request(request).block_task().await?;
        Ok(())
    }

    async fn create_terminal(
        &self,
        command: &str,
        args: &[String],
        cwd: Option<&str>,
        output_byte_limit: Option<u64>,
    ) -> anyhow::Result<crate::tools::acp_client::TerminalHandle> {
        let request = CreateTerminalRequest::new(self.session_id.clone(), command)
            .args(args.to_vec())
            .cwd(Some(effective_cwd(cwd, &self.cwd)))
            .output_byte_limit(output_byte_limit);
        let response = self.connection.send_request(request).block_task().await?;
        Ok(crate::tools::acp_client::TerminalHandle(
            response.terminal_id.0.to_string(),
        ))
    }

    async fn terminal_output(
        &self,
        terminal: &crate::tools::acp_client::TerminalHandle,
    ) -> anyhow::Result<crate::tools::acp_client::TerminalOutput> {
        let request = TerminalOutputRequest::new(
            self.session_id.clone(),
            TerminalId::new(terminal.0.clone()),
        );
        let response = self.connection.send_request(request).block_task().await?;
        Ok(crate::tools::acp_client::TerminalOutput {
            output: response.output,
            truncated: response.truncated,
            exit_status: response
                .exit_status
                .map(|s| crate::tools::acp_client::ExitStatus {
                    exit_code: s.exit_code,
                    signal: s.signal,
                }),
        })
    }

    async fn wait_for_terminal_exit(
        &self,
        terminal: &crate::tools::acp_client::TerminalHandle,
    ) -> anyhow::Result<crate::tools::acp_client::ExitStatus> {
        let request = WaitForTerminalExitRequest::new(
            self.session_id.clone(),
            TerminalId::new(terminal.0.clone()),
        );
        let response = self.connection.send_request(request).block_task().await?;
        Ok(crate::tools::acp_client::ExitStatus {
            exit_code: response.exit_status.exit_code,
            signal: response.exit_status.signal,
        })
    }

    async fn kill_terminal(
        &self,
        terminal: &crate::tools::acp_client::TerminalHandle,
    ) -> anyhow::Result<()> {
        let request =
            KillTerminalRequest::new(self.session_id.clone(), TerminalId::new(terminal.0.clone()));
        self.connection.send_request(request).block_task().await?;
        Ok(())
    }

    async fn release_terminal(
        &self,
        terminal: &crate::tools::acp_client::TerminalHandle,
    ) -> anyhow::Result<()> {
        let request = ReleaseTerminalRequest::new(
            self.session_id.clone(),
            TerminalId::new(terminal.0.clone()),
        );
        self.connection.send_request(request).block_task().await?;
        Ok(())
    }

    async fn try_reserve_terminal_slot(
        &self,
    ) -> Result<crate::tools::acp_client::TerminalReservation, crate::tools::acp_client::CapHeld>
    {
        let mut registry = self.terminals.lock().unwrap();
        let held = registry.entry(self.session_id.to_string()).or_default();
        if held.len() >= crate::tools::client_tools::MAX_TERMINALS_PER_SESSION {
            let handles: Vec<crate::tools::acp_client::TerminalHandle> = held
                .iter()
                .filter(|h| h.0 != crate::tools::acp_client::RESERVED_TERMINAL_MARKER)
                .cloned()
                .collect();
            let reservations = held.len() - handles.len();
            return Err(crate::tools::acp_client::CapHeld {
                handles,
                reservations,
            });
        }
        held.push(crate::tools::acp_client::reserved_terminal_placeholder());
        Ok(crate::tools::acp_client::TerminalReservation::new(
            Arc::clone(&self.terminals),
            self.session_id.to_string(),
        ))
    }

    async fn track_terminal(
        &self,
        reservation: crate::tools::acp_client::TerminalReservation,
        handle: crate::tools::acp_client::TerminalHandle,
    ) {
        reservation.resolve(handle);
    }

    async fn untrack_terminal(&self, handle: &crate::tools::acp_client::TerminalHandle) {
        let mut registry = self.terminals.lock().unwrap();
        if let Some(held) = registry.get_mut(&self.session_id.to_string()) {
            if let Some(pos) = held.iter().position(|h| h == handle) {
                held.remove(pos);
            }
            if held.is_empty() {
                registry.remove(&self.session_id.to_string());
            }
        }
    }
}

/// How one prompt turn stopped.
enum TurnEnd {
    /// The client asked for it to stop, or its connection went away.
    Cancelled,
    /// The shared executor finished — successfully or not; the outcome says
    /// which.
    Ran(super::LlmTurnOutcome),
}

/// Log, rather than propagate, a failure to answer a `session/prompt`.
///
/// The turn that produced this answer runs in a task spawned on the
/// connection, and the SDK shuts the whole connection down when one of those
/// returns an error (`ConnectionTo::spawn`'s own documentation says so). A
/// send that fails here only ever means the client already left — which the
/// connection notices on its own — so it must not be reported that way.
fn answered(session_id: &SessionId, sent: Result<(), Error>) -> Result<(), Error> {
    if let Err(e) = sent {
        warn!("ACP: could not answer session/prompt for session {session_id}: {e}");
    }
    Ok(())
}

/// The wire name of a prompt content block, for the log line that says one
/// was dropped. `ContentBlock` is `#[non_exhaustive]`, so the catch-all arm
/// is load-bearing rather than defensive.
fn prompt_block_kind(block: &ContentBlock) -> &'static str {
    match block {
        ContentBlock::Text(_) => "text",
        ContentBlock::Image(_) => "image",
        ContentBlock::Audio(_) => "audio",
        ContentBlock::ResourceLink(_) => "resource_link",
        ContentBlock::Resource(_) => "resource",
        _ => "unknown",
    }
}

pub async fn handle_acp_ws(
    State(state): State<Arc<ServeState>>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    // 0. Feature gate, mirroring /a2a.
    if !state.config.acp.as_ref().is_some_and(|c| c.enabled) {
        return (StatusCode::NOT_FOUND, "ACP disabled").into_response();
    }

    // 1. Bearer auth → room profile. Both failure modes are 401 at the HTTP
    //    layer; ACP never sees an unauthenticated peer.
    let Some(bearer) = extract_bearer(&headers) else {
        return (StatusCode::UNAUTHORIZED, "missing bearer token").into_response();
    };
    let Some(profile_name) = state
        .device_auth
        .resolve(&bearer)
        .map(|r| r.room_profile.to_string())
    else {
        warn!("ACP: rejected an unknown or revoked bearer token");
        return (StatusCode::UNAUTHORIZED, "unknown or revoked bearer token").into_response();
    };

    info!("ACP: connection accepted for room profile '{profile_name}'");
    // Read before the upgrade closure takes `state`: `keepalive_secs` is
    // fixed for the life of the connection, so there is nothing to gain by
    // looking it up again later.
    let keepalive = state
        .config
        .acp
        .as_ref()
        .map(|c| c.keepalive_secs)
        .filter(|secs| *secs > 0)
        .map(Duration::from_secs);
    ws.on_upgrade(move |socket| serve_connection(socket, state, profile_name, keepalive))
}

/// Prefix of a JSON-RPC error response whose `id` is `null` — the shape the
/// spec reserves for "a response to a request whose id could not be
/// determined" (a peer answering something it could not parse enough of to
/// correlate). See [`lines_transport`] for why this text is matched and
/// dropped before it ever reaches the SDK's transport.
const NULL_ID_ERROR_RESPONSE_PREFIX: &str = r#"{"jsonrpc":"2.0","id":null,"error"#;

/// Wrap the socket as the SDK's line transport.
///
/// Per the ACP transport RFD one JSON-RPC message rides in one text frame,
/// and `Lines` hands the sink one JSON-RPC message per `String` with no
/// trailing newline — so a text frame *is* a line and the outgoing direction
/// needs no reframing at all. The incoming direction still runs every text
/// frame through [`reassemble_split_messages`] first: see that function for
/// why a frame is not always trustworthy as a complete message on its own.
///
/// Everything that is not a text frame is dropped on the floor: binary
/// frames carry no ACP meaning, axum answers incoming pings itself, and a
/// close frame is followed by the end of the stream, which is what actually
/// ends the connection.
///
/// A text frame shaped like [`NULL_ID_ERROR_RESPONSE_PREFIX`] is dropped the
/// same way. That shape is a JSON-RPC response, not a request — the SDK has
/// no request of its own with a `null` id to match it against, so it can
/// only log the response as unroutable and move on. Answering it anyway
/// would itself be a response to a response, and a peer that treats *that*
/// as needing an answer in turn echoes forever: the two sides trade the same
/// error back and forth with no request underneath either send, which is
/// the frame storm this branch exists to stop before it starts.
///
/// `connection_cancel` is cancelled when this socket stops delivering
/// frames — see [`cancel_when_exhausted`].
///
/// `keepalive` is how often to put a ping on the wire, and is the reason
/// the sink is not simply the socket's own write half. A reverse proxy in
/// front of `/acp` closes a WebSocket that carries no traffic — measured
/// against the HAProxy this agent runs behind, at 1800.005s — and ACP has
/// no idle traffic of its own: an editor with its panel open but nothing
/// being asked sends nothing for as long as the person is away. The agent
/// pings rather than leaving it to the bridge's arguments because that
/// covers every client at once, and because the client's automatic pong
/// then puts traffic on the tunnel in *both* directions, whichever one the
/// proxy is actually watching.
///
/// The write half therefore moves into a task of its own, which owns it and
/// interleaves two sources: ACP messages arriving over a channel, and the
/// ping timer. Backpressure survives the indirection — the channel is
/// bounded, so a client that has stopped reading still blocks the sink
/// rather than growing a queue.
fn lines_transport(
    socket: WebSocket,
    connection_cancel: CancellationToken,
    keepalive: Option<Duration>,
) -> Lines<
    impl futures_util::Sink<String, Error = std::io::Error> + Send + 'static,
    impl futures_util::Stream<Item = std::io::Result<String>> + Send + 'static,
> {
    let (tx, rx) = socket.split();

    let (lines_tx, lines_rx) = tokio::sync::mpsc::channel::<Message>(32);
    tokio::spawn(write_frames(tx, lines_rx, keepalive));

    // Dropped with the `Lines` the SDK holds, which closes the channel and
    // is what tells `write_frames` the connection is over.
    let outgoing = futures_util::sink::unfold(lines_tx, |tx, line: String| async move {
        tx.send(Message::Text(line.into()))
            .await
            .map_err(std::io::Error::other)?;
        Ok::<_, std::io::Error>(tx)
    });

    let frames = rx.filter_map(|frame| async move {
        match frame {
            Ok(Message::Text(text)) if text.starts_with(NULL_ID_ERROR_RESPONSE_PREFIX) => {
                debug!(
                    "ACP: dropping a null-id error response instead of relaying it, to avoid \
                     an echo storm"
                );
                None
            }
            Ok(Message::Text(text)) => Some(Ok(text.to_string())),
            // Not ACP: no reply, and the connection carries on.
            Ok(Message::Binary(_) | Message::Ping(_) | Message::Pong(_)) => None,
            // The stream ends right after this, which closes the connection.
            Ok(Message::Close(_)) => None,
            Err(e) => Some(Err(std::io::Error::other(e))),
        }
    });
    let frames = reassemble_split_messages(frames);

    Lines::new(outgoing, cancel_when_exhausted(frames, connection_cancel))
}

/// Bound on how much text [`reassemble_split_messages`] holds across frames
/// while waiting for a JSON-RPC message's tail to arrive. A peer that never
/// completes the value it started would otherwise make this grow without
/// limit; no legitimate ACP message needs anywhere near this much, so
/// hitting it is itself treated as the fragment being invalid rather than
/// merely incomplete.
const MAX_REASSEMBLY_BYTES: usize = 64 * 1024 * 1024;

/// Reassemble a JSON-RPC message a peer split across multiple WebSocket text
/// frames, back into the single complete line [`Lines`] requires.
///
/// [`lines_transport`]'s doc says a text frame *is* a line per the ACP
/// transport RFD, which holds for a peer that honors "one JSON-RPC message
/// per text frame". One observed in production does not: a subagent tool
/// result carrying an ~80KB code-review diff arrived as several consecutive
/// text frames that only parse as JSON once concatenated, so each fragment
/// alone was handed to the SDK as its own "line", and each failed with
/// `-32700 Parse error` — a burst of error responses to requests that were
/// never split in the first place, since nothing on this side ever asked
/// for a reply to a reply. This sits in front of that: a frame that already
/// parses on its own passes straight through (the common case, and the only
/// one before this existed), so this changes nothing for a conforming peer.
///
/// A held-back fragment is flushed — concatenated so far, whatever shape
/// that is — the moment appending a frame does *not* leave the buffer
/// failing to parse for the specific reason of ending before the value was
/// complete (`serde_json::Error::is_eof`). Any other parse failure means the
/// fragment was simply invalid, not incomplete, and buffering further would
/// only delay reporting the same error while holding memory for no benefit.
/// [`MAX_REASSEMBLY_BYTES`] is the same policy applied to a peer that keeps
/// extending a value without ever finishing it.
fn reassemble_split_messages(
    frames: impl futures_util::Stream<Item = std::io::Result<String>>,
) -> impl futures_util::Stream<Item = std::io::Result<String>> {
    frames
        .scan(String::new(), |pending, item| {
            let text = match item {
                Err(e) => {
                    pending.clear();
                    return std::future::ready(Some(Some(Err(e))));
                }
                Ok(text) => text,
            };
            pending.push_str(&text);

            match serde_json::from_str::<serde_json::Value>(pending) {
                Ok(_) => std::future::ready(Some(Some(Ok(std::mem::take(pending))))),
                Err(e) if e.is_eof() && pending.len() < MAX_REASSEMBLY_BYTES => {
                    std::future::ready(Some(None))
                }
                Err(_) => std::future::ready(Some(Some(Ok(std::mem::take(pending))))),
            }
        })
        .filter_map(|item| async move { item })
}

/// Own the socket's write half: forward ACP messages, and ping on the
/// keepalive interval.
///
/// Ends when the channel closes (the SDK dropped its sink, so the
/// connection is finished) or when a write fails (the peer is gone). The
/// ping doubles as detection for the second case: without it, a server
/// with nothing to say never writes, so it never learns that a client
/// which vanished without a close frame is no longer there.
async fn write_frames(
    mut sink: futures_util::stream::SplitSink<WebSocket, Message>,
    mut lines: tokio::sync::mpsc::Receiver<Message>,
    keepalive: Option<Duration>,
) {
    let mut ticker = keepalive.map(|period| {
        // `interval_at`, not `interval`: the latter's first tick completes
        // immediately, which would ping every connection the moment it
        // opens. The first ping belongs one full period in.
        let mut ticker = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
        // A missed tick means the writer was busy, which is the one case
        // where a keepalive is not needed. Delay, so a stall cannot queue
        // up a burst of pings to fire back to back once it clears.
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ticker
    });

    loop {
        // `pending()` when there is no keepalive parks this branch forever,
        // so the `select!` waits on the channel alone.
        let tick = async {
            match ticker.as_mut() {
                Some(t) => {
                    t.tick().await;
                }
                None => std::future::pending().await,
            }
        };

        let message = tokio::select! {
            line = lines.recv() => match line {
                Some(message) => message,
                // The SDK is done with the connection.
                None => break,
            },
            () = tick => Message::Ping(Default::default()),
        };

        if sink.send(message).await.is_err() {
            // The peer is gone. The incoming half sees the same thing and
            // ends the connection; there is nothing to report here that is
            // not already reported there.
            return;
        }
    }

    let _ = sink.close().await;
}

/// Cancel `token` as soon as `frames` stops producing items.
///
/// This is *not* what keeps a turn from outliving its connection — the SDK
/// already does that. `Builder::connect_to` runs `incoming_closed()` then
/// `drain_outgoing()` as the foreground of the connection future
/// (`jsonrpc.rs:1602-1611`), with the task actor as the background; when the
/// foreground finishes the background is dropped, taking every spawned turn
/// with it. Cancelling after `connect_to` returns would be redundant, not too
/// late.
///
/// What this adds is *timing*. The guard fires at EOF, which is before the
/// drain — and a client that has gone away without reading can hold the drain
/// up indefinitely by backpressuring the outgoing sink. Until the drain
/// finishes, a turn would otherwise still be calling the provider.
///
/// The token rides in the stream's own state as a `DropGuard`: `Unfold` drops
/// the completed step future, and with it the guard, at the poll that observes
/// `None`. On the error path the item is `Some(Err(_))` and the state is
/// retained, so there the guard fires only when the SDK drops the stream while
/// failing its actors — same outcome, different mechanism.
fn cancel_when_exhausted<S>(
    frames: S,
    token: CancellationToken,
) -> impl futures_util::Stream<Item = S::Item> + Send + 'static
where
    S: futures_util::Stream + Send + 'static,
{
    futures_util::stream::unfold(
        (Box::pin(frames), token.drop_guard()),
        |(mut frames, guard)| async move { frames.next().await.map(|item| (item, (frames, guard))) },
    )
}

/// The mode list every session starts with.
///
/// A loaded session starts in `default` like a new one: the mode is a
/// statement about how the editor wants the agent to behave right now,
/// not a property of the conversation, so it is not persisted.
fn mode_state() -> SessionModeState {
    SessionModeState::new(
        crate::tools::policy::SessionMode::Default.id(),
        crate::tools::policy::SessionMode::ALL
            .into_iter()
            .map(|m| AcpSessionMode::new(m.id(), m.name()).description(m.description()))
            .collect(),
    )
}

/// Count this connection as holding `id`, warning when another
/// connection already does.
///
/// Shared by `adopt_session` (`session/load`, `session/resume`) and
/// `session/new` — every path that hands a connection a session must
/// count it, since the decrement in `serve_connection` walks every
/// session a connection's map holds and expects each to have been
/// counted on the way in.
async fn count_session_open(state: &Arc<ServeState>, id: &str) {
    let mut open = state.open_acp_sessions.lock().await;
    let count = open.entry(id.to_string()).or_insert(0);
    *count += 1;
    if *count > 1 {
        warn!(
            "ACP: session {id} is now open on {count} connections. Concurrent prompts on \
             one session are not history-safe: run_llm_turn clones the history at the top \
             and writes it back whole, so the last turn to finish wins in memory while both \
             have already appended to the transcript."
        );
    }
}

/// Release every session id a closing connection held: decrement
/// `open_acp_sessions` for each, dropping the entry once its count
/// reaches zero.
///
/// Called from `serve_connection`'s teardown, and from
/// `client_tools`'s `simulate_connection_teardown` test helper — the
/// same function both reach lets a test prove "closing a connection
/// releases nothing in `acp_terminals`" against the real teardown
/// code, rather than against a test's own idea of what teardown does.
///
/// Deliberately does **not** touch `acp_terminals`: `terminal/release`
/// kills the command it releases, so freeing a terminal here because a
/// socket dropped would kill a build over a network blip. The handles
/// stay tracked against the session id, so a client that reconnects
/// and reloads the session can still reach them.
pub(crate) async fn release_connection_sessions(state: &Arc<ServeState>, held: Vec<String>) {
    let mut open = state.open_acp_sessions.lock().await;
    for id in held {
        if let Some(count) = open.get_mut(&id) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                open.remove(&id);
            }
        }
    }
}

/// Validate an existing session id and adopt it onto this connection.
///
/// Shared by `session/load` and `session/resume`, which differ only in
/// whether they replay afterwards. `Err` carries the refusal to answer
/// with — one wording for both "no such session" and "not yours", so
/// the pair cannot be used to enumerate ids.
async fn adopt_session(
    state: &Arc<ServeState>,
    sessions: &Arc<AcpSessions>,
    profile_name: &str,
    session_id: &SessionId,
    cwd: PathBuf,
) -> Result<String, Error> {
    let id = session_id.to_string();
    let namespace = state
        .config
        .namespace_for_room_profile(profile_name)
        .to_string();
    let refuse = || Error::invalid_params().data("no such session is available on this connection");

    let Some(summary) = state.acp_session_store.summary(&id) else {
        return Err(refuse());
    };
    if summary.header.namespace != namespace {
        warn!(
            "ACP: refused adopting {id}: it belongs to namespace {}, not {namespace}",
            summary.header.namespace
        );
        return Err(refuse());
    }
    if summary.is_closed {
        // Symmetric with `session/list`, which excludes closed sessions:
        // an archived conversation must be unreachable by id the same way
        // it is unlisted, not just invisible in the picker. Same refusal
        // as the other two cases, so the trio stays indistinguishable.
        return Err(refuse());
    }

    state
        .session_room_profiles
        .lock()
        .await
        .insert(id.clone(), profile_name.to_string());
    state.acp_sessions.lock().await.insert(id.clone());
    let already_held = sessions
        .inner
        .lock()
        .await
        .insert(
            session_id.clone(),
            AcpSession {
                agent_session_id: id.clone(),
                cwd,
                client_capabilities: sessions.client_capabilities.lock().unwrap().clone(),
                turns: HashMap::new(),
                mode: crate::tools::policy::SessionMode::Default,
            },
        )
        .is_some();
    if !already_held {
        // Count holders, not adoptions. Re-adopting a session this
        // connection already has — a `load` then a `resume`, or a
        // client retry over the same socket — replaces the map entry
        // rather than adding one, and teardown subtracts one per entry.
        // Incrementing anyway would leave a phantom holder behind after
        // every real one had gone, and the next connection to open the
        // session would be warned about a peer that no longer exists.
        count_session_open(state, &id).await;
    }
    Ok(id)
}

/// Drive one ACP connection until the peer goes away.
async fn serve_connection(
    socket: WebSocket,
    state: Arc<ServeState>,
    profile_name: String,
    keepalive: Option<Duration>,
) {
    let sessions = Arc::new(AcpSessions::default());
    // Cancelled when the socket goes away (see `lines_transport`). Every
    // turn's token is a child of this one, so one client leaving stops every
    // turn it left running — including turns on sessions created later.
    let connection_cancel = CancellationToken::new();

    let result = Agent
        .builder()
        .name("sapphire-agent")
        .on_receive_request(
            {
                let sessions = Arc::clone(&sessions);
                async move |req: InitializeRequest, responder, _connection| {
                    // Which client tools exist is decided per session from
                    // this, so it has to survive past this handler rather
                    // than being parsed and dropped. There is no session yet
                    // to hang it on — `session/new` and `adopt_session` are
                    // what copy it onto one, from here.
                    *sessions.client_capabilities.lock().unwrap() =
                        req.client_capabilities.clone();

                    info!(
                        "ACP: {} initialized: {}",
                        describe_client(req.client_info.as_ref()),
                        describe_capabilities(&req.client_capabilities)
                    );

                    // Answer with the version we will actually speak, which the
                    // ACP specification defines as the client's version if we
                    // support it and otherwise the latest version we do support
                    // (the prose spec's Initialization / "Protocol Version
                    // Negotiation" section; the SDK only carries the version
                    // constants themselves, in `schema/src/version.rs`). Handing
                    // the request back unchanged lies in both directions: a v2
                    // client told "2" sends v2-shaped traffic nothing here
                    // understands, and a v0 client told "0" is the same lie at the
                    // other end — v0 being a pre-release the schema documents as
                    // one to treat as unsupported.
                    //
                    // An explicit supported set rather than a clamp: with one
                    // version implemented the correct answer is `V1` for every
                    // input, and this keeps saying what we actually support as
                    // versions accumulate, instead of silently claiming each new
                    // gap in the range.
                    //
                    // `V1`, not `LATEST`: this is a claim about what *this* code
                    // implements. `LATEST` means "newest stable the SDK knows of"
                    // and would resume telling exactly this lie the day the SDK
                    // promotes v2 (it is cfg-gated off when `unstable_protocol_v2`
                    // is enabled, precisely so that choice stays explicit).
                    const SUPPORTED: [ProtocolVersion; 1] = [ProtocolVersion::V1];
                    let version = if SUPPORTED.contains(&req.protocol_version) {
                        req.protocol_version
                    } else {
                        ProtocolVersion::V1
                    };

                    // `authMethods` is empty because the bearer token checked
                    // above already authenticated the peer, so ACP never sees
                    // an unauthenticated client.
                    responder.respond(
                        InitializeResponse::new(version).agent_capabilities(
                            AgentCapabilities::new()
                                .load_session(true)
                                .session_capabilities(
                                    SessionCapabilities::new()
                                        .list(SessionListCapabilities::new())
                                        .resume(SessionResumeCapabilities::new()),
                                ),
                        ),
                    )
                }
            },
            on_receive_request!(),
        )
        .on_receive_request(
            {
                let sessions = Arc::clone(&sessions);
                let state = Arc::clone(&state);
                let profile_name = profile_name.clone();
                async move |req: NewSessionRequest, responder, _connection| {
                    // Mint the agent-side session id the same way a
                    // brand-new `/rpc` session gets one in `handle_initialize`
                    // (`src/serve/mod.rs`), rather than inventing a second
                    // convention.
                    let agent_session_id = uuid::Uuid::now_v7().to_string();

                    // `mcp_servers` is not honoured, and silence about that
                    // would be indistinguishable from a bug: the editor's
                    // configured servers would simply be absent, with no
                    // tools from them and nothing said. They are the
                    // *client's* servers, mostly stdio commands to spawn on
                    // the client's machine, which an agent reached over a
                    // socket cannot launch and must not try to — this
                    // process is not on that machine. MCP servers for this
                    // agent are configured server-side, in its own config.
                    if !req.mcp_servers.is_empty() {
                        warn!(
                            "ACP: ignoring {} MCP server(s) offered by the client for session {}: \
                             they are the client's to launch, and this agent runs elsewhere. \
                             Configure MCP servers in the agent's own config instead.",
                            req.mcp_servers.len(),
                            agent_session_id,
                        );
                    }

                    // Pin the session to the room profile the bearer token
                    // resolved to at connection time. That pin is what gives
                    // the ACP session its namespace chain and provider
                    // through the paths that already exist for `/rpc`.
                    state
                        .session_room_profiles
                        .lock()
                        .await
                        .insert(agent_session_id.clone(), profile_name.clone());

                    // Write the session's header now, rather than leaving
                    // the cwd to wait for a first turn: `AcpSessionStore`
                    // has no lazy-create path the way the old `/rpc` store
                    // did (`append_*` requires the file to already exist),
                    // so this is what makes the session appendable at all.
                    // Registering it in `acp_sessions` is what makes
                    // `run_llm_turn` route this session's history and
                    // messages through that store instead of `/rpc`'s.
                    let namespace = state
                        .config
                        .namespace_for_room_profile(&profile_name)
                        .to_string();
                    let cwd_string = req.cwd.to_string_lossy().to_string();
                    if let Err(e) =
                        state
                            .acp_session_store
                            .create(&agent_session_id, &namespace, &cwd_string)
                    {
                        warn!(
                            "ACP: failed to create the session store entry for {agent_session_id}: {e}"
                        );
                        return responder.respond_with_internal_error(e.to_string());
                    }
                    state
                        .acp_sessions
                        .lock()
                        .await
                        .insert(agent_session_id.clone());

                    let session_id = SessionId::new(agent_session_id.clone());
                    count_session_open(&state, &agent_session_id).await;
                    sessions.inner.lock().await.insert(
                        session_id.clone(),
                        AcpSession {
                            agent_session_id,
                            cwd: req.cwd.clone(),
                            client_capabilities: sessions
                                .client_capabilities
                                .lock()
                                .unwrap()
                                .clone(),
                            // No turn is running yet, so there is nothing for
                            // a `session/cancel` to reach.
                            turns: HashMap::new(),
                            mode: crate::tools::policy::SessionMode::Default,
                        },
                    );

                    // The client learns the modes here, and starts in
                    // the one that asks. `NewSessionResponse.modes` is a
                    // plain `Option` in the schema, with no capability
                    // gating it, so `initialize` needs no change.
                    responder.respond(NewSessionResponse::new(session_id).modes(mode_state()))
                }
            },
            on_receive_request!(),
        )
        .on_receive_request(
            {
                let state = Arc::clone(&state);
                let profile_name = profile_name.clone();
                async move |req: ListSessionsRequest, responder, _connection| {
                    let namespace = state
                        .config
                        .namespace_for_room_profile(&profile_name)
                        .to_string();
                    let store = Arc::clone(&state.acp_session_store);
                    let wanted_cwd = req.cwd.as_ref().map(|c| c.to_string_lossy().to_string());

                    let sessions: Vec<SessionInfo> = store
                        .list_summaries(&namespace)
                        .into_iter()
                        // An editor opens a thread on every panel open, so
                        // most sessions are created and never typed into —
                        // those are not conversations to show.
                        .filter(|summary| summary.has_messages)
                        // A closed session is archived, not current.
                        .filter(|summary| !summary.is_closed)
                        .filter(|summary| {
                            // `list_summaries` already scopes by namespace
                            // directory, but the boundary is checked here
                            // too on purpose: it is what `session/load`
                            // also enforces, and the redundancy is
                            // deliberate defense in depth rather than an
                            // oversight.
                            summary.header.namespace == namespace
                        })
                        .filter(|summary| match &wanted_cwd {
                            Some(wanted) => &summary.header.cwd == wanted,
                            None => true,
                        })
                        .map(|summary| {
                            let updated_at = store
                                .absolute_path_for(&summary.header.session_id)
                                .and_then(|path| std::fs::metadata(&path).and_then(|m| m.modified()).ok())
                                .map(|t| chrono::DateTime::<chrono::Utc>::from(t).to_rfc3339());
                            let mut info = SessionInfo::new(
                                SessionId::new(summary.header.session_id.clone()),
                                PathBuf::from(&summary.header.cwd),
                            );
                            info = info.title(summary.title.clone());
                            info = info.updated_at(updated_at);
                            info
                        })
                        .collect();

                    // No pagination: the whole list, every time. A
                    // cursor earns its keep when a namespace holds
                    // thousands of sessions, and none does yet.
                    responder.respond(ListSessionsResponse::new(sessions))
                }
            },
            on_receive_request!(),
        )
        .on_receive_request(
            {
                let sessions = Arc::clone(&sessions);
                let state = Arc::clone(&state);
                let profile_name = profile_name.clone();
                async move |req: LoadSessionRequest, responder, connection: ConnectionTo<Client>| {
                    // Validation and adoption are shared with
                    // `session/resume` — see `adopt_session`. `load`'s
                    // own job starts after that: replaying the history
                    // the resumed session already has.
                    let id = match adopt_session(
                        &state,
                        &sessions,
                        &profile_name,
                        &req.session_id,
                        req.cwd.clone(),
                    )
                    .await
                    {
                        Ok(id) => id,
                        Err(e) => return responder.respond_with_error(e),
                    };
                    let store = Arc::clone(&state.acp_session_store);

                    // Replay BEFORE answering: the ACP specification
                    // orders it that way, and a client that got the
                    // reply first would render an empty thread and then
                    // watch messages appear underneath it.
                    for message in store.history(&id).unwrap_or_default() {
                        let text: String = message
                            .parts
                            .iter()
                            .filter_map(|part| match part {
                                crate::provider::ContentPart::Text(t) => Some(t.as_str()),
                                _ => None,
                            })
                            .collect::<Vec<_>>()
                            .join("\n");
                        if text.is_empty() {
                            continue;
                        }
                        let chunk = ContentChunk::new(ContentBlock::Text(TextContent::new(text)));
                        let update = match message.role {
                            crate::provider::Role::User => SessionUpdate::UserMessageChunk(chunk),
                            crate::provider::Role::Assistant => {
                                SessionUpdate::AgentMessageChunk(chunk)
                            }
                        };
                        if let Err(e) = connection.send_notification(SessionNotification::new(
                            req.session_id.clone(),
                            update,
                        )) {
                            warn!("ACP: dropped a replay update for {id}: {e}");
                        }
                    }

                    responder.respond(LoadSessionResponse::new().modes(mode_state()))
                }
            },
            on_receive_request!(),
        )
        .on_receive_request(
            {
                let sessions = Arc::clone(&sessions);
                let state = Arc::clone(&state);
                let profile_name = profile_name.clone();
                async move |req: ResumeSessionRequest, responder, _connection| {
                    // Same adoption as `load`, no replay. The ACP
                    // specification frames `resume` as the fallback for
                    // agents that cannot load at all; offered here so a
                    // client can skip redrawing a long conversation.
                    match adopt_session(
                        &state,
                        &sessions,
                        &profile_name,
                        &req.session_id,
                        req.cwd.clone(),
                    )
                    .await
                    {
                        Ok(_) => {
                            responder.respond(ResumeSessionResponse::new().modes(mode_state()))
                        }
                        Err(e) => responder.respond_with_error(e),
                    }
                }
            },
            on_receive_request!(),
        )
        .on_receive_notification(
            {
                let sessions = Arc::clone(&sessions);
                async move |notif: CancelNotification, _connection: ConnectionTo<Client>| {
                    // *Every* turn on the session, not the newest one: the
                    // notification names a session, and the schema scopes it
                    // to that session's "ongoing operations" — plural, which
                    // concurrent prompts make reachable.
                    //
                    // A session with nothing running is not an error; the
                    // client may simply have raced the reply, and the empty
                    // map makes that a no-op. An unknown session is a
                    // different thing, and a notification has no way to
                    // report it but the log.
                    match sessions.inner.lock().await.get(&notif.session_id) {
                        Some(session) => {
                            for turn in session.turns.values() {
                                turn.cancel();
                            }
                        }
                        None => warn!(
                            "ACP: session/cancel named a session this connection never \
                             minted: {}",
                            notif.session_id
                        ),
                    }
                    Ok(())
                }
            },
            on_receive_notification!(),
        )
        .on_receive_request(
            {
                let sessions = Arc::clone(&sessions);
                async move |req: SetSessionModeRequest,
                            responder,
                            connection: ConnectionTo<Client>| {
                    let Some(mode) =
                        crate::tools::policy::SessionMode::from_id(req.mode_id.0.as_ref())
                    else {
                        // `plan` lands here. An error is the honest
                        // reply: silently picking another mode would
                        // leave the user believing the agent is
                        // planning when it is about to act.
                        return responder.respond_with_error(
                            Error::invalid_params().data(format!("unknown mode '{}'", req.mode_id)),
                        );
                    };

                    {
                        let mut guard = sessions.inner.lock().await;
                        let Some(session) = guard.get_mut(&req.session_id) else {
                            return responder.respond_with_error(
                                Error::invalid_params()
                                    .data(format!("unknown session '{}'", req.session_id)),
                            );
                        };
                        session.mode = mode;
                    }

                    // Announce it: a client that changed the mode from
                    // one surface should see it reflected on the others.
                    if let Err(e) = connection.send_notification(SessionNotification::new(
                        req.session_id.clone(),
                        SessionUpdate::CurrentModeUpdate(CurrentModeUpdate::new(mode.id())),
                    )) {
                        warn!("ACP: dropped a current_mode_update: {e}");
                    }

                    responder.respond(SetSessionModeResponse::new())
                }
            },
            on_receive_request!(),
        )
        .on_receive_request(
            {
                let sessions = Arc::clone(&sessions);
                let state = Arc::clone(&state);
                let connection_cancel = connection_cancel.clone();
                let profile_name = profile_name.clone();
                async move |req: PromptRequest, responder, connection: ConnectionTo<Client>| {
                    // Register this turn's cancellation token in the same
                    // lock that resolves the session, so a `session/cancel`
                    // arriving after this point cannot miss it.
                    let turn = sessions
                        .next_turn
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let looked_up = {
                        let mut guard = sessions.inner.lock().await;
                        guard.get_mut(&req.session_id).map(|session| {
                            let turn_cancel = connection_cancel.child_token();
                            session.turns.insert(turn, turn_cancel.clone());
                            (
                                session.agent_session_id.clone(),
                                turn_cancel,
                                session.mode,
                                session.client_capabilities.clone(),
                                session.cwd.clone(),
                            )
                        })
                    };
                    let Some((agent_session_id, turn_cancel, mode, client_capabilities, cwd)) =
                        looked_up
                    else {
                        // Not created on the fly: a prompt naming a session
                        // this connection never minted is a client bug, and
                        // starting one here would quietly open a second
                        // conversation the client believes it is continuing.
                        // `invalid_params`, because the offending thing is a
                        // parameter of this request.
                        return responder.respond_with_error(Error::invalid_params().data(
                            format!(
                                "unknown session '{}'; call session/new first",
                                req.session_id
                            ),
                        ));
                    };

                    // Flatten the prompt's blocks into one user message.
                    //
                    // Text and ResourceLink are both mandatory for every
                    // agent — the schema's words are "All agents MUST support
                    // resource links in prompts" — and a resource link is how
                    // Zed sends an `@file` mention. Dropping one would leave
                    // the model reading "explain" with no idea what it refers
                    // to. The link is folded in by name and uri: this build
                    // cannot open the file (client-side filesystem access is
                    // a later phase), but naming the reference keeps the
                    // mention in the conversation until it can.
                    //
                    // Image, Audio and Resource are capability-gated and
                    // `initialize` advertises no prompt capabilities, so they
                    // should never arrive — if one does, it is dropped
                    // loudly rather than silently.
                    let text = req
                        .prompt
                        .iter()
                        .filter_map(|block| match block {
                            ContentBlock::Text(t) => Some(t.text.clone()),
                            ContentBlock::ResourceLink(link) => Some(format!(
                                "[referenced resource: {} ({})]",
                                link.name, link.uri
                            )),
                            other => {
                                warn!(
                                    "ACP: dropped an unsupported prompt block ({}) from a \
                                     session/prompt for session {}",
                                    prompt_block_kind(other),
                                    req.session_id
                                );
                                None
                            }
                        })
                        .collect::<Vec<_>>()
                        .join("\n");

                    // Everything past this point belongs to `run_llm_turn`:
                    // history, the tool loop and the memory namespace all
                    // come from the shared executor. Persistence still
                    // routes to ACP's own `acp_session_store`, separate
                    // from `/rpc` and A2A's `SessionStore` — but under the
                    // same memory namespace and system prompt as either.
                    let session_id = req.session_id.clone();
                    let progress = Arc::new(AcpProgress::new(
                        session_id.clone(),
                        connection.clone(),
                        profile_name.clone(),
                        mode,
                        Arc::clone(&state.permissions),
                        client_capabilities,
                        cwd,
                        Arc::clone(&state.acp_terminals),
                    ));

                    // The turn runs OUTSIDE the dispatch loop, and the
                    // `Responder` travels with it.
                    //
                    // This is not a stylistic choice and must not be
                    // "simplified" back into an `await` here. Handlers
                    // registered with `on_receive_request` run *inside* the
                    // SDK's dispatch loop, which parses no further frame on
                    // this connection until the handler returns. Awaiting the
                    // turn here would mean the `session/cancel` sent to stop
                    // it could not be read until the turn had already
                    // finished — the feature would be unimplementable rather
                    // than merely slow. `ConnectionTo::spawn` is the SDK's own
                    // escape hatch for exactly this, and `Responder` is
                    // movable, so the spawned task answers the request
                    // whenever the turn really ends.
                    //
                    // The consequence, accepted deliberately: prompts on one
                    // connection now run concurrently instead of one after
                    // another.
                    connection.spawn({
                        let state = Arc::clone(&state);
                        let sessions = Arc::clone(&sessions);
                        async move {
                            // Bound to a `let` on purpose: that drops the
                            // losing branch's future here, before the answer
                            // goes out, so a client told its turn was
                            // cancelled knows the provider call is already
                            // abandoned rather than merely disowned.
                            let end = tokio::select! {
                                // `biased`, so a cancellation that lands in
                                // the same poll as a finished turn still wins:
                                // the schema makes `Cancelled` a MUST "even if
                                // the cancellation causes exceptions in
                                // underlying operations", which means the
                                // error path below must be unreachable once
                                // the token has fired.
                                biased;
                                () = turn_cancel.cancelled() => TurnEnd::Cancelled,
                                outcome = super::run_llm_turn(
                                    state,
                                    agent_session_id,
                                    crate::provider::ChatMessage::user(&text),
                                    Arc::clone(&progress) as Arc<dyn super::TurnHost>,
                                    None,
                                ) => TurnEnd::Ran(outcome),
                            };

                            // This turn is no longer an "ongoing operation",
                            // so drop its token: a later `session/cancel`
                            // should not have to fire at turns that have
                            // already answered, and a long-lived session must
                            // not accumulate one token per prompt it ever
                            // received. The local clone above still works, so
                            // the reply paths below are unaffected.
                            if let Some(session) = sessions.inner.lock().await.get_mut(&session_id)
                            {
                                session.turns.remove(&turn);
                            }

                            let TurnEnd::Ran(outcome) = end else {
                                return answered(
                                    &session_id,
                                    responder.respond(PromptResponse::new(StopReason::Cancelled)),
                                );
                            };

                            let Some(_reply) = outcome.text else {
                                if turn_cancel.is_cancelled() {
                                    // The turn failed *because* it was being
                                    // cancelled, or was cancelled in the
                                    // moment it failed. Either way the client
                                    // asked for this, and the schema says it
                                    // hears `Cancelled`.
                                    return answered(
                                        &session_id,
                                        responder
                                            .respond(PromptResponse::new(StopReason::Cancelled)),
                                    );
                                }

                                // Running out of tool rounds is not a
                                // failure, and ACP has the exact word for
                                // it: `MaxTurnRequests`, "the agent reached
                                // the maximum number of allowed agent
                                // requests between user turns". With
                                // `[tools.tool_rounds] interactive = 0` —
                                // the default — this never fires at all;
                                // an operator who sets a cap gets a
                                // routine ending rather than an error
                                // dialog, and the work done on the way is
                                // already in the client's hands because
                                // every round's prose went out through
                                // `message_chunk` as it happened.
                                if matches!(&outcome.stop, super::TurnStop::BudgetExhausted { .. })
                                {
                                    return answered(
                                        &session_id,
                                        responder.respond(PromptResponse::new(
                                            StopReason::MaxTurnRequests,
                                        )),
                                    );
                                }

                                // A failed turn is a JSON-RPC error, not a
                                // stop reason: none of ACP v1's stop reasons
                                // means "the agent broke", and `Refusal` would
                                // tell the user the agent *declined*, which is
                                // a materially different thing to show them.
                                return answered(
                                    &session_id,
                                    responder.respond_with_internal_error(
                                        progress.failure().unwrap_or_else(|| {
                                            "the turn produced no reply".to_string()
                                        }),
                                    ),
                                );
                            };

                            // No chunk here: every non-empty text this turn
                            // produced — the final one included — already
                            // went out from `AcpProgress::message_chunk` in
                            // the round that produced it. `outcome.text` is
                            // the same prose joined back together, kept for
                            // `/rpc`, `/a2a` and the voice pipeline, and
                            // sending it here as well would deliver the
                            // whole reply twice.
                            answered(
                                &session_id,
                                responder.respond(PromptResponse::new(StopReason::EndTurn)),
                            )
                        }
                    })?;

                    Ok(())
                }
            },
            on_receive_request!(),
        )
        .connect_to(lines_transport(
            socket,
            connection_cancel.clone(),
            keepalive,
        ))
        .await;

    // Release every session this connection held. Missing this would turn
    // `open_acp_sessions` into a leak that warns forever: the next
    // connection to load the same session would find a stale count already
    // above one and warn about a collision that no longer exists.
    //
    // Terminals are deliberately NOT released here. `terminal/release`
    // kills the command, and a dropped socket is not a reason to kill a
    // build running on the user's machine. The handles stay tracked
    // against the session, so a client that reconnects and loads the
    // session can still reach them. See `release_connection_sessions`'s
    // doc for the rest of the reasoning.
    {
        let held: Vec<String> = sessions
            .inner
            .lock()
            .await
            .values()
            .map(|s| s.agent_session_id.clone())
            .collect();
        release_connection_sessions(&state, held).await;
    }

    // Belt and braces, and known to be so. By the time this runs the socket
    // has gone, so `cancel_when_exhausted` has already fired the token, and
    // the SDK has dropped the task actor holding any turn along with it. It
    // is kept because it states the connection's contract where a reader
    // looks for it, and costs one atomic on a path that runs once per
    // connection.
    connection_cancel.cancel();

    // Name the profile on the way out as well as the way in: with several
    // editors connected under different profiles, a bare "connection closed"
    // cannot be matched to the connection it belongs to.
    if let Err(e) = result {
        warn!("ACP: connection for room profile '{profile_name}' ended with an error: {e}");
    } else {
        info!("ACP: connection for room profile '{profile_name}' closed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // `TurnHost` is only needed here, for `.acp_client()` in
    // `the_default_turn_host_offers_no_client` below — production code
    // in this file calls it fully qualified as `super::TurnHost`
    // instead, so importing it at module scope would be unused there.
    use crate::serve::{HangingChat, NullProgress, TurnHost};
    // Only `fs_caps_from_reads_the_recorded_flags_independently` below
    // builds a `FileSystemCapabilities` directly; production code in
    // this file only ever reads `.fs` off an already-built
    // `ClientCapabilities`.
    use agent_client_protocol::schema::v1::FileSystemCapabilities;
    use futures_util::{SinkExt, StreamExt};
    use tokio::net::TcpListener;
    use tokio_tungstenite::tungstenite::Message;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

    /// One end of an ACP socket, as the test client holds it.
    type TestSocket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

    /// Bind the router on an ephemeral port and return its `host:port`.
    async fn spawn(state: Arc<ServeState>) -> String {
        let app = axum::Router::new()
            .route("/acp", axum::routing::get(handle_acp_ws))
            .with_state(state);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("127.0.0.1:{}", addr.port())
    }

    /// Attempt the upgrade. Returns the HTTP status the server refused with,
    /// or `None` when the upgrade succeeded (101).
    async fn upgrade_status(addr: &str, token: Option<&str>) -> Option<u16> {
        let mut req = format!("ws://{addr}/acp").into_client_request().unwrap();
        if let Some(t) = token {
            req.headers_mut()
                .insert("authorization", format!("Bearer {t}").parse().unwrap());
        }
        match tokio_tungstenite::connect_async(req).await {
            Ok(_) => None,
            Err(tokio_tungstenite::tungstenite::Error::Http(resp)) => Some(resp.status().as_u16()),
            Err(e) => panic!("unexpected transport error: {e}"),
        }
    }

    #[tokio::test]
    async fn a_frame_that_already_parses_passes_through_unchanged() {
        let line = r#"{"jsonrpc":"2.0","id":1,"method":"session/cancel"}"#.to_string();
        let frames = futures_util::stream::iter([Ok(line.clone())]);
        let out: Vec<_> = reassemble_split_messages(frames).collect().await;
        assert_eq!(
            out.into_iter().map(Result::unwrap).collect::<Vec<_>>(),
            [line]
        );
    }

    #[tokio::test]
    async fn a_message_split_across_frames_is_reassembled_into_one_line() {
        let full =
            r#"{"jsonrpc":"2.0","id":1,"result":{"content":"line one\nline two"}}"#.to_string();
        let (first, second) = full.split_at(30);
        let frames = futures_util::stream::iter([Ok(first.to_string()), Ok(second.to_string())]);
        let out: Vec<_> = reassemble_split_messages(frames).collect().await;
        let lines = out.into_iter().map(Result::unwrap).collect::<Vec<_>>();
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0], full);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&lines[0]).unwrap(),
            serde_json::from_str::<serde_json::Value>(&full).unwrap()
        );
    }

    #[tokio::test]
    async fn a_message_split_across_three_frames_is_reassembled_into_one_line() {
        let full = r#"{"jsonrpc":"2.0","id":1,"result":{"content":"one\ntwo\nthree"}}"#;
        let a = &full[..20];
        let b = &full[20..45];
        let c = &full[45..];
        let frames =
            futures_util::stream::iter([Ok(a.to_string()), Ok(b.to_string()), Ok(c.to_string())]);
        let out: Vec<_> = reassemble_split_messages(frames).collect().await;
        let lines = out.into_iter().map(Result::unwrap).collect::<Vec<_>>();
        assert_eq!(lines, [full.to_string()]);
    }

    #[tokio::test]
    async fn a_frame_that_is_invalid_rather_than_incomplete_is_not_held_back() {
        // Trailing comma: a syntax error once the array closes, not an
        // end-of-input while a value was still open, so it must not wait
        // for a frame that would never complete it.
        let line = "[1, 2,]".to_string();
        let frames = futures_util::stream::iter([Ok(line.clone()), Ok("next".to_string())]);
        let out: Vec<_> = reassemble_split_messages(frames).collect().await;
        assert_eq!(
            out.into_iter().map(Result::unwrap).collect::<Vec<_>>(),
            [line, "next".to_string()]
        );
    }

    #[tokio::test]
    async fn a_transport_error_flushes_any_held_fragment_and_passes_through() {
        let frames = futures_util::stream::iter([
            Ok(r#"{"jsonrpc":"2.0","#.to_string()),
            Err(std::io::Error::other("peer gone")),
        ]);
        let out: Vec<_> = reassemble_split_messages(frames).collect().await;
        assert_eq!(out.len(), 1);
        assert!(out[0].is_err());
    }

    #[tokio::test]
    async fn disabled_endpoint_is_not_found() {
        let addr = spawn(ServeState::for_test(false)).await;
        assert_eq!(upgrade_status(&addr, Some("sa-acp-token")).await, Some(404));
    }

    #[tokio::test]
    async fn missing_and_unknown_tokens_are_unauthorized() {
        let addr = spawn(ServeState::for_test(true)).await;
        assert_eq!(upgrade_status(&addr, None).await, Some(401));
        assert_eq!(upgrade_status(&addr, Some("sa-wrong")).await, Some(401));
    }

    #[tokio::test]
    async fn valid_token_upgrades() {
        let addr = spawn(ServeState::for_test(true)).await;
        assert_eq!(upgrade_status(&addr, Some("sa-acp-token")).await, None);
    }

    /// Open an authenticated ACP socket.
    pub(super) async fn connect(addr: &str) -> TestSocket {
        let mut req = format!("ws://{addr}/acp").into_client_request().unwrap();
        req.headers_mut()
            .insert("authorization", "Bearer sa-acp-token".parse().unwrap());
        tokio_tungstenite::connect_async(req).await.unwrap().0
    }

    pub(super) fn initialize_request(id: i64) -> serde_json::Value {
        initialize_request_asking_for(id, 1)
    }

    /// An `initialize` that asks for a specific protocol version.
    fn initialize_request_asking_for(id: i64, protocol_version: u16) -> serde_json::Value {
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "initialize",
            "params": {
                "protocolVersion": protocol_version,
                "clientCapabilities": {
                    "fs": { "readTextFile": true, "writeTextFile": true },
                    "terminal": true
                },
                "clientInfo": { "name": "test-client", "version": "0.0.0" }
            }
        })
    }

    fn test_cwd() -> &'static str {
        if cfg!(windows) {
            "C:\\work\\proj"
        } else {
            "/work/proj"
        }
    }

    fn new_session_request(id: i64) -> serde_json::Value {
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "session/new",
            "params": { "cwd": test_cwd(), "mcpServers": [] }
        })
    }

    /// Read the next frame. A missing reply fails loudly instead of hanging
    /// the test run forever.
    async fn next_frame(ws: &mut TestSocket) -> Message {
        tokio::time::timeout(std::time::Duration::from_secs(10), ws.next())
            .await
            .expect("timed out waiting for an ACP frame")
            .expect("stream ended")
            .unwrap()
    }

    /// Run several requests over ONE connection, in order.
    pub(super) async fn conversation(
        addr: &str,
        requests: Vec<serde_json::Value>,
    ) -> Vec<serde_json::Value> {
        let mut ws = connect(addr).await;
        let mut responses = Vec::new();
        for request in requests {
            let want_id = request["id"].clone();
            ws.send(Message::Text(request.to_string().into()))
                .await
                .unwrap();
            loop {
                match next_frame(&mut ws).await {
                    Message::Text(t) => {
                        let v: serde_json::Value = serde_json::from_str(&t).unwrap();
                        if v["id"] == want_id {
                            responses.push(v);
                            break;
                        }
                    }
                    Message::Ping(_) | Message::Pong(_) => continue,
                    other => panic!("unexpected frame: {other:?}"),
                }
            }
        }
        responses
    }

    /// Send one request on a fresh connection and read the reply to it.
    async fn roundtrip(addr: &str, request: serde_json::Value) -> serde_json::Value {
        conversation(addr, vec![request])
            .await
            .pop()
            .expect("one reply per request")
    }

    #[tokio::test]
    async fn initialize_answers_with_v1_capabilities() {
        let addr = spawn(ServeState::for_test(true)).await;
        let resp = roundtrip(&addr, initialize_request(0)).await;

        assert_eq!(resp["id"], 0);
        let result = &resp["result"];
        assert_eq!(result["protocolVersion"], 1);
        assert_eq!(result["agentCapabilities"]["loadSession"], true);
        assert_eq!(
            result["authMethods"],
            serde_json::json!([]),
            "auth already happened at the HTTP layer"
        );
    }

    /// A client offering a version this build does not implement must be told
    /// v1 — the version we will actually speak — not handed its own request
    /// back. That holds in both directions: answering "2" to a v2 client makes
    /// it send v2-shaped traffic nothing here understands, and answering "0"
    /// to a v0 client is the same lie at the other end of the range. v0 is a
    /// pre-release the schema documents as unsupported, and this build does
    /// not implement it either.
    #[tokio::test]
    async fn unsupported_versions_are_negotiated_to_v1() {
        let addr = spawn(ServeState::for_test(true)).await;

        for asked in [0u16, 2, 99] {
            let resp = roundtrip(&addr, initialize_request_asking_for(0, asked)).await;
            assert_eq!(
                resp["result"]["protocolVersion"], 1,
                "asked for {asked}, must be negotiated to v1, got {resp}"
            );
        }
    }

    #[tokio::test]
    async fn malformed_frame_errors_without_closing_the_connection() {
        let addr = spawn(ServeState::for_test(true)).await;
        let mut ws = connect(&addr).await;

        ws.send(Message::Text("{ this is not json".into()))
            .await
            .unwrap();
        // A parse error carries a null id per JSON-RPC.
        let first = loop {
            match next_frame(&mut ws).await {
                Message::Text(t) => break serde_json::from_str::<serde_json::Value>(&t).unwrap(),
                Message::Ping(_) | Message::Pong(_) => continue,
                other => panic!("unexpected frame: {other:?}"),
            }
        };
        assert!(first["error"].is_object(), "got {first}");
        assert_eq!(
            first["id"],
            serde_json::Value::Null,
            "an error that cannot be correlated to a request carries a null id"
        );
        assert_eq!(first["error"]["code"], -32700, "JSON-RPC parse error");

        // The connection is still usable.
        ws.send(Message::Text(initialize_request(1).to_string().into()))
            .await
            .unwrap();
        loop {
            match next_frame(&mut ws).await {
                Message::Text(t) => {
                    let v: serde_json::Value = serde_json::from_str(&t).unwrap();
                    if v["id"] == 1 {
                        assert_eq!(v["result"]["protocolVersion"], 1);
                        return;
                    }
                }
                Message::Ping(_) | Message::Pong(_) => continue,
                other => panic!("unexpected frame: {other:?}"),
            }
        }
    }

    /// A null-id error response — the shape a peer sends back for a frame it
    /// could not correlate to a request of its own — must never get a reply.
    /// Answering one is itself a response to a response, which is exactly
    /// the shape of an echo storm: a peer that also auto-replies to anything
    /// resembling a response would receive that reply and send back another
    /// null-id error of its own, forever. This asserts the connection stays
    /// open and quiet through one, and still answers ordinary requests
    /// afterward.
    #[tokio::test]
    async fn null_id_error_response_is_dropped_without_a_reply() {
        let addr = spawn(ServeState::for_test(true)).await;
        let mut ws = connect(&addr).await;

        ws.send(Message::Text(
            r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"parse error"}}"#.into(),
        ))
        .await
        .unwrap();

        // Nothing answers the dropped frame; the next thing on the wire is
        // the reply to a request sent right after it.
        ws.send(Message::Text(initialize_request(1).to_string().into()))
            .await
            .unwrap();
        loop {
            match next_frame(&mut ws).await {
                Message::Text(t) => {
                    let v: serde_json::Value = serde_json::from_str(&t).unwrap();
                    assert_eq!(
                        v["id"], 1,
                        "the dropped frame must not have produced a reply"
                    );
                    assert_eq!(v["result"]["protocolVersion"], 1);
                    return;
                }
                Message::Ping(_) | Message::Pong(_) => continue,
                other => panic!("unexpected frame: {other:?}"),
            }
        }
    }

    /// A method nothing here registers a handler for, and whose params carry
    /// no `sessionId` field (so the SDK's session-scoped retry —
    /// `role/acp.rs:293-311`, keyed on `Dispatch::has_session_id` — never
    /// applies), falls straight through to JSON-RPC `method not found`.
    ///
    /// DEVIATION from the plan's expectation. The plan predicted the RFD's
    /// rule — `initialize` must come first — would be enforced, so that an
    /// unhandled method on a fresh connection is rejected *because* it is out
    /// of order. The SDK enforces no ordering at all: it has no notion of an
    /// initialized connection. What actually rejects it is that no handler is
    /// registered, so the dispatch loop falls through to `method not found` —
    /// and, as the second half of this test shows, the answer is identical
    /// once `initialize` has been done.
    ///
    /// This test originally used `session/new` as its unhandled example.
    /// Task 7 registered a `session/new` handler, so it was replaced with a
    /// fabricated method name: any real ACP method scoped to a session (e.g.
    /// `session/load`) carries `sessionId` in its own params and would hang
    /// on the retry instead of demonstrating this behaviour.
    #[tokio::test]
    async fn an_unimplemented_method_is_answered_with_method_not_found() {
        fn unknown_method_request(id: i64) -> serde_json::Value {
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "totally/unknown",
                "params": {}
            })
        }

        let addr = spawn(ServeState::for_test(true)).await;

        let before = roundtrip(&addr, unknown_method_request(0)).await;
        assert_eq!(before["id"], 0);
        assert_eq!(
            before["error"]["code"], -32601,
            "no handler is registered for it, got {before}"
        );

        // Initializing first changes nothing: the rejection is about the
        // missing handler, not about the connection's state.
        let after = conversation(
            &addr,
            vec![initialize_request(0), unknown_method_request(1)],
        )
        .await;
        assert!(after[0]["result"].is_object(), "got {}", after[0]);
        assert_eq!(
            after[1]["error"]["code"], -32601,
            "ordering is not what rejected it, got {}",
            after[1]
        );
    }

    #[tokio::test]
    async fn binary_frames_are_ignored() {
        let addr = spawn(ServeState::for_test(true)).await;
        let mut ws = connect(&addr).await;

        ws.send(Message::Binary(vec![0xde, 0xad].into()))
            .await
            .unwrap();
        ws.send(Message::Text(initialize_request(0).to_string().into()))
            .await
            .unwrap();
        loop {
            match next_frame(&mut ws).await {
                Message::Text(t) => {
                    let v: serde_json::Value = serde_json::from_str(&t).unwrap();
                    assert_eq!(v["id"], 0, "the binary frame produced no reply of its own");
                    return;
                }
                Message::Ping(_) | Message::Pong(_) => continue,
                other => panic!("unexpected frame: {other:?}"),
            }
        }
    }

    /// An idle connection must get a ping without having asked for one.
    ///
    /// Nothing in ACP keeps a silent connection warm, and a reverse proxy
    /// in front of `/acp` closes one that stays silent: the HAProxy this
    /// agent runs behind cuts an idle tunnel at 1800.005s, measured, which
    /// reaches the agent as `Connection reset by peer` and reaches the
    /// editor as a `websocat` stuck in CLOSE-WAIT that never exits — so the
    /// editor keeps writing into a dead socket instead of reconnecting.
    ///
    /// The agent is the end that can fix this for every client at once,
    /// which is why the ping originates here rather than in the bridge's
    /// arguments. A client answers it with an automatic pong, so one timer
    /// puts traffic on the tunnel in both directions.
    #[tokio::test]
    async fn an_idle_connection_gets_a_server_ping() {
        let addr = spawn(ServeState::for_test_acp_keepalive(1)).await;
        let mut ws = connect(&addr).await;

        // Not one frame is sent from this end — not even `initialize`.
        // Whatever arrives, arrives because the server decided to.
        let deadline = std::time::Duration::from_secs(10);
        // The very first frame settles it: on a connection nobody has
        // spoken on, a ping is the only thing that should ever arrive.
        let pinged = tokio::time::timeout(deadline, ws.next())
            .await
            .expect("no server ping within 10s on a connection configured to ping every 1s");

        let pinged = match pinged {
            Some(Ok(Message::Ping(_))) => true,
            Some(Ok(other)) => panic!("unexpected frame on an idle connection: {other:?}"),
            // The connection died instead of being kept alive.
            Some(Err(_)) | None => false,
        };

        assert!(
            pinged,
            "the idle connection ended instead of receiving a keepalive ping"
        );
    }

    /// One ping is not a keepalive. The connection has to be kept warm for
    /// as long as it is open, so the timer must keep firing — a single
    /// ping at the first tick would move the drop from 1800s to 1800s
    /// after the ping and look identical in every short test.
    #[tokio::test]
    async fn the_keepalive_keeps_pinging() {
        let addr = spawn(ServeState::for_test_acp_keepalive(1)).await;
        let mut ws = connect(&addr).await;

        let mut pings = 0;
        tokio::time::timeout(std::time::Duration::from_secs(20), async {
            while pings < 3 {
                match ws.next().await {
                    Some(Ok(Message::Ping(_))) => pings += 1,
                    Some(Ok(other)) => panic!("unexpected frame: {other:?}"),
                    Some(Err(e)) => panic!("connection failed after {pings} ping(s): {e}"),
                    None => panic!("connection ended after {pings} ping(s)"),
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("only {pings} ping(s) in 20s at a 1s interval"));

        assert_eq!(pings, 3, "the keepalive stopped after the first tick");
    }

    /// `keepalive_secs = 0` means silence, and has to reach the timer as
    /// "no timer" rather than as a zero-length period — `interval_at`
    /// panics on one, which would take down the writer task, and with it
    /// the connection, for every client of an agent whose operator had
    /// merely switched the pings off.
    #[tokio::test]
    async fn keepalive_zero_sends_no_pings() {
        let addr = spawn(ServeState::for_test_acp_keepalive(0)).await;
        let mut ws = connect(&addr).await;

        // The connection still has to *work*. A zero period reaching
        // `interval_at` kills the writer task, and a dead writer is
        // invisible from the reading end — the socket stays open and the
        // request simply never comes back — so asking for a reply is what
        // separates "pings are off" from "the write half is gone".
        ws.send(Message::Text(initialize_request(1).to_string().into()))
            .await
            .unwrap();

        let replied = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                match ws.next().await {
                    Some(Ok(Message::Text(_))) => return true,
                    Some(Ok(Message::Ping(_))) => {
                        panic!("a ping arrived on a connection that asked for none")
                    }
                    Some(Ok(_)) => continue,
                    Some(Err(_)) | None => return false,
                }
            }
        })
        .await
        .expect("no answer to initialize: the writer task is gone");

        assert!(replied, "the connection dropped instead of answering");

        // Now that the socket is known good, hold it open well past the
        // period a zero would have meant, and require silence.
        let ping = tokio::time::timeout(std::time::Duration::from_millis(1500), async {
            loop {
                match ws.next().await {
                    Some(Ok(Message::Ping(_))) => return true,
                    Some(Ok(_)) => continue,
                    Some(Err(_)) | None => return false,
                }
            }
        })
        .await;
        assert!(
            ping.is_err(),
            "keepalive_secs = 0 must leave the connection silent"
        );
    }

    /// The close arm of the frame filter, which every later task's connection
    /// lifetime rests on. A close frame must let the incoming stream end so
    /// the ACP connection finishes and the server drops its half — rather than
    /// the filter swallowing it and leaving the connection task parked forever.
    /// The 10s timeout is the real assertion here: a leak shows up as a hang.
    #[tokio::test]
    async fn a_close_frame_ends_the_connection() {
        let addr = spawn(ServeState::for_test(true)).await;
        let mut ws = connect(&addr).await;

        ws.send(Message::Close(None)).await.unwrap();

        loop {
            let next = tokio::time::timeout(std::time::Duration::from_secs(10), ws.next())
                .await
                .expect("server still holding the connection open after a close frame");
            match next {
                // The server closed its side: what this test is for.
                None => return,
                // axum echoes the close frame back before closing.
                Some(Ok(Message::Close(_))) => continue,
                // A transport error also means the connection is gone. What
                // this rules out is the server hanging on to it, not the
                // niceties of the closing handshake.
                Some(Err(_)) => return,
                Some(Ok(other)) => panic!("unexpected frame after close: {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn session_new_returns_a_session_id() {
        let state = ServeState::for_test(true);
        let addr = spawn(Arc::clone(&state)).await;
        let responses =
            conversation(&addr, vec![initialize_request(0), new_session_request(1)]).await;

        let session_id = responses[1]["result"]["sessionId"]
            .as_str()
            .expect("sessionId present");
        assert!(!session_id.is_empty());

        // The central new behaviour of this task: `session/new` must pin the
        // agent-side session id to the room profile the bearer token
        // resolved to (the fixture's `sa-acp-token` resolves to
        // `"developer"`). Asserting the exact mapping, not just that the map
        // is non-empty, so a swapped key/value or a write into the wrong map
        // fails this test.
        let profiles = state.session_room_profiles.lock().await;
        assert_eq!(
            profiles.get(session_id).map(String::as_str),
            Some("developer"),
            "session/new must pin the session to the token's room profile, got {profiles:?}"
        );
    }

    /// The `prompt` array of a text-only message.
    fn text_prompt(text: &str) -> serde_json::Value {
        serde_json::json!([{ "type": "text", "text": text }])
    }

    fn prompt_request(id: i64, session_id: &str, prompt: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "session/prompt",
            "params": { "sessionId": session_id, "prompt": prompt }
        })
    }

    /// initialize → session/new → session/prompt on ONE connection.
    ///
    /// Returns the session id the agent minted, every `session/update`
    /// notification the turn emitted in arrival order, and the whole
    /// JSON-RPC reply to the prompt (so a caller can inspect either
    /// `result.stopReason` or `error`).
    ///
    /// The session id is returned rather than discarded so callers can
    /// assert against *that* session — most now in `acp_session_store`,
    /// ACP's own store — instead of against whatever session happens to
    /// be the only one there.
    ///
    /// `conversation` cannot be used here: it filters frames down to one
    /// request id and would drop exactly the notifications under test,
    /// turning a wrong ordering into a slow hang instead of a failure.
    async fn drive(
        addr: &str,
        prompt: serde_json::Value,
    ) -> (String, Vec<serde_json::Value>, serde_json::Value) {
        let mut ws = connect(addr).await;
        for request in [initialize_request(0), new_session_request(1)] {
            ws.send(Message::Text(request.to_string().into()))
                .await
                .unwrap();
        }

        let mut session_id: Option<String> = None;
        let mut updates = Vec::new();

        loop {
            let frame = next_frame(&mut ws).await;
            let Message::Text(t) = frame else { continue };
            let v: serde_json::Value = serde_json::from_str(&t).unwrap();

            if v["id"] == 1 {
                let id = v["result"]["sessionId"]
                    .as_str()
                    .expect("sessionId present")
                    .to_string();
                session_id = Some(id.clone());
                ws.send(Message::Text(
                    prompt_request(2, &id, prompt.clone()).to_string().into(),
                ))
                .await
                .unwrap();
            } else if v["method"] == "session/update" {
                assert_eq!(
                    v["params"]["sessionId"],
                    *session_id.as_ref().expect("a session exists by now"),
                    "an update must name the session it belongs to, got {v}"
                );
                updates.push(v["params"]["update"].clone());
            } else if v["id"] == 2 {
                return (
                    session_id.expect("the prompt was sent, so a session exists"),
                    updates,
                    v,
                );
            }
        }
    }

    /// Like `drive`, but answers any `session/request_permission` the
    /// agent sends with `option_id`. Returns the session updates, the
    /// final reply, and how many permission requests arrived.
    ///
    /// The count is the point: a test that only checks the outcome
    /// cannot tell "allowed without asking" from "asked and allowed",
    /// and those are the two things this feature is about.
    async fn drive_answering(
        addr: &str,
        prompt: serde_json::Value,
        option_id: &str,
    ) -> (Vec<serde_json::Value>, serde_json::Value, usize) {
        let mut ws = connect(addr).await;
        for request in [initialize_request(0), new_session_request(1)] {
            ws.send(Message::Text(request.to_string().into()))
                .await
                .unwrap();
        }

        let mut updates = Vec::new();
        let mut asked = 0usize;

        loop {
            let Message::Text(t) = next_frame(&mut ws).await else {
                continue;
            };
            let v: serde_json::Value = serde_json::from_str(&t).unwrap();

            if v["id"] == 1 {
                let id = v["result"]["sessionId"].as_str().unwrap().to_string();
                ws.send(Message::Text(
                    prompt_request(2, &id, prompt.clone()).to_string().into(),
                ))
                .await
                .unwrap();
            } else if v["method"] == "session/request_permission" {
                asked += 1;
                let answer = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": v["id"],
                    "result": {
                        "outcome": { "outcome": "selected", "optionId": option_id }
                    }
                });
                ws.send(Message::Text(answer.to_string().into()))
                    .await
                    .unwrap();
            } else if v["method"] == "session/update" {
                updates.push(v["params"]["update"].clone());
            } else if v["id"] == 2 {
                return (updates, v, asked);
            }
        }
    }

    /// The grant this feature exists for, pinned end to end: an editor's
    /// `session/prompt` turn scopes the room profile its bearer token pinned,
    /// so `agent_config` — which ACP has no room id to allow — can be judged
    /// against `[tools.admin].room_profiles`. The probe stands in for one of
    /// the admin tools: it is offered and judged on the same path.
    #[tokio::test]
    async fn an_acp_turn_scopes_its_pinned_room_profile() {
        let state = ServeState::for_test_scripted(
            true,
            vec![
                crate::provider::ChatResponse {
                    prompt_usage: None,
                    text: None,
                    tool_calls: vec![crate::provider::ToolCall {
                        id: "call-1".to_string(),
                        name: "heartbeat_config".to_string(),
                        input: serde_json::json!({"action": "list"}),
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
        let probe = crate::serve::AdminProfileProbe::new();
        state
            .tools
            .register_tool(Box::new(Arc::clone(&probe)))
            .await;

        let addr = spawn(Arc::clone(&state)).await;
        let (_, updates, reply) = drive(&addr, text_prompt("manage the definitions")).await;
        assert!(reply["error"].is_null(), "the turn must not fail: {reply}");
        assert_eq!(
            probe.seen(),
            Some(Some("developer".to_string())),
            "the fixture's `sa-acp-token` resolves to room profile 'developer', so that is \
             the name the admin gate must see; updates: {updates:?}"
        );
    }

    /// `Provider::chat` returns a whole response, so the reply arrives as
    /// exactly one `agent_message_chunk` — one, not zero, and not a faked
    /// token stream.
    #[tokio::test]
    async fn prompt_answers_with_one_chunk_and_ends_the_turn() {
        let state = ServeState::for_test_scripted(
            true,
            vec![crate::provider::ChatResponse {
                prompt_usage: None,
                text: Some("hello from the agent".to_string()),
                tool_calls: Vec::new(),
                stop_reason: None,
            }],
        );
        let addr = spawn(state).await;
        let (_session_id, updates, reply) = drive(&addr, text_prompt("hi")).await;

        let chunks: Vec<&str> = updates
            .iter()
            .filter(|u| u["sessionUpdate"] == "agent_message_chunk")
            .map(|u| u["content"]["text"].as_str().unwrap())
            .collect();
        assert_eq!(chunks, vec!["hello from the agent"], "got {updates:?}");
        assert_eq!(reply["result"]["stopReason"], "end_turn", "got {reply}");
    }

    /// 中間の散文がラウンドごとに届く。まとめて1通ではなく、モデルが
    /// 出した単位で並ぶ。これが「作業中に喋れる」の全部である。
    #[tokio::test]
    async fn prose_arrives_round_by_round_rather_than_all_at_the_end() {
        let state = ServeState::for_test_scripted(
            true,
            vec![
                crate::provider::ChatResponse {
                    prompt_usage: None,
                    text: Some("checking the config".to_string()),
                    tool_calls: vec![crate::provider::ToolCall {
                        id: "call-1".to_string(),
                        name: "echo".to_string(),
                        input: serde_json::json!({ "text": "ping" }),
                    }],
                    stop_reason: None,
                },
                crate::provider::ChatResponse {
                    prompt_usage: None,
                    text: Some("it was the timeout".to_string()),
                    tool_calls: Vec::new(),
                    stop_reason: None,
                },
            ],
        );
        let addr = spawn(state).await;
        let (_session_id, updates, reply) = drive(&addr, text_prompt("why is it slow")).await;

        let chunks: Vec<&str> = updates
            .iter()
            .filter(|u| u["sessionUpdate"] == "agent_message_chunk")
            .map(|u| u["content"]["text"].as_str().unwrap())
            .collect();
        assert_eq!(
            chunks,
            vec!["checking the config", "it was the timeout"],
            "got {chunks:?}"
        );
        assert_eq!(reply["result"]["stopReason"], "end_turn", "got {reply}");
    }

    /// 最終テキストが二重に届かない。`message_chunk` で流したものを
    /// ターン終了時にもう一度送っていたら、ここで2通になる。
    #[tokio::test]
    async fn the_final_reply_is_not_sent_twice() {
        let state = ServeState::for_test_scripted(
            true,
            vec![crate::provider::ChatResponse {
                prompt_usage: None,
                text: Some("just this".to_string()),
                tool_calls: Vec::new(),
                stop_reason: None,
            }],
        );
        let addr = spawn(state).await;
        let (_session_id, updates, _reply) = drive(&addr, text_prompt("hello")).await;

        let chunks: Vec<&str> = updates
            .iter()
            .filter(|u| u["sessionUpdate"] == "agent_message_chunk")
            .map(|u| u["content"]["text"].as_str().unwrap())
            .collect();
        assert_eq!(chunks, vec!["just this"], "got {chunks:?}");
    }

    /// ACP は `interactive` 側で判定される。既定は 0 = 無制限なので、
    /// 旧来の10ラウンドを超えても止まらない。
    #[tokio::test]
    async fn an_acp_turn_runs_past_ten_rounds_by_default() {
        let mut script: Vec<crate::provider::ChatResponse> = (0..12)
            .map(|i| crate::provider::ChatResponse {
                prompt_usage: None,
                text: None,
                tool_calls: vec![crate::provider::ToolCall {
                    id: format!("call-{i}"),
                    name: "echo".to_string(),
                    input: serde_json::json!({ "text": "ping" }),
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
        let addr = spawn(ServeState::for_test_scripted(true, script)).await;
        let (_session_id, _updates, reply) = drive(&addr, text_prompt("do a lot")).await;

        assert_eq!(reply["result"]["stopReason"], "end_turn", "got {reply}");
    }

    /// A prompt turn's history reaches `run_llm_turn`'s in-memory
    /// `state.sessions` map, keyed by the exact session id the agent
    /// handed back in `session/new`'s reply: `session/prompt` delegates
    /// to `run_llm_turn`, so the user message and the reply are in
    /// `state.sessions` afterwards, not lost or filed under a different
    /// id.
    #[tokio::test]
    async fn a_prompt_turns_history_lands_under_its_own_session_id() {
        let state = ServeState::for_test_scripted(
            true,
            vec![crate::provider::ChatResponse {
                prompt_usage: None,
                text: Some("hello from the agent".to_string()),
                tool_calls: Vec::new(),
                stop_reason: None,
            }],
        );
        let addr = spawn(Arc::clone(&state)).await;
        let (session_id, _updates, _reply) = drive(&addr, text_prompt("hi")).await;

        // Keyed on the session id the agent handed the client, not on
        // "whatever single entry exists": a handler that minted a session
        // id of its own — quietly starting a second conversation — would
        // still leave exactly one entry here and pass a looser assertion.
        let sessions = state.sessions.lock().await;
        let history = sessions
            .get(&session_id)
            .unwrap_or_else(|| panic!("no history under {session_id}, got {:?}", sessions.keys()));
        let texts: Vec<&str> = history
            .iter()
            .filter_map(|m| match m.parts.first() {
                Some(crate::provider::ContentPart::Text(t)) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(texts, vec!["hi", "hello from the agent"], "got {texts:?}");
    }

    /// `session/new` writes the session's header — `cwd` included —
    /// eagerly, before any turn runs: `AcpSessionStore` has no
    /// lazy-create path, so this is what makes the session appendable
    /// at all. `session/list` will have nothing else to filter a
    /// project by, so if this regresses the listing is always empty.
    #[tokio::test]
    async fn a_new_sessions_cwd_reaches_the_store() {
        let state = ServeState::for_test_scripted(
            true,
            vec![crate::provider::ChatResponse {
                prompt_usage: None,
                text: Some("ok".to_string()),
                tool_calls: Vec::new(),
                stop_reason: None,
            }],
        );
        let store = Arc::clone(&state.acp_session_store);
        let addr = spawn(state).await;

        let (session_id, _updates, _reply) = drive(&addr, text_prompt("hi")).await;

        let summary = store
            .summary(&session_id)
            .expect("session/new persisted the session's header");
        assert_eq!(summary.header.cwd, test_cwd());
    }

    /// issue #245: an ACP turn's system prompt must carry the session's
    /// recorded cwd — an absolute path on the *client's* machine, injected
    /// verbatim — so the model knows where it is working. Asserted where
    /// the feature is wired: the `system` string the provider call was
    /// actually made with (`StubProvider`'s call log), not where it is
    /// computed.
    #[tokio::test]
    async fn a_prompt_turn_injects_the_session_cwd_into_the_system_prompt() {
        let (state, chat_log) = ServeState::for_test_scripted_with_log(
            true,
            vec![crate::provider::ChatResponse {
                prompt_usage: None,
                text: Some("ok".to_string()),
                tool_calls: Vec::new(),
                stop_reason: None,
            }],
        );
        let addr = spawn(state).await;
        let (_session_id, _updates, _reply) = drive(&addr, text_prompt("hi")).await;

        let calls = chat_log.calls();
        assert_eq!(calls.len(), 1, "unexpected call count: {calls:?}");
        let system = calls[0].0.as_deref().unwrap_or_default();
        assert!(
            system.contains(&format!(
                "# Current Workspace\n\n- Working directory: {}",
                test_cwd()
            )),
            "the turn's system prompt must carry the session cwd verbatim \
             (no canonicalisation — it names a path on the client's \
             machine): {system}"
        );
    }

    /// Zed sends every `@file` mention as a `resource_link` block, which
    /// the schema makes mandatory for all agents ("All agents MUST support
    /// resource links in prompts"). It is not capability-gated, so it will
    /// arrive, and it must reach the model rather than being dropped on the
    /// way — otherwise "explain @main.rs" reaches the provider as "explain".
    #[tokio::test]
    async fn a_resource_link_reaches_the_model() {
        let state = ServeState::for_test_scripted(
            true,
            vec![crate::provider::ChatResponse {
                prompt_usage: None,
                text: Some("ok".to_string()),
                tool_calls: Vec::new(),
                stop_reason: None,
            }],
        );
        let addr = spawn(Arc::clone(&state)).await;
        let (session_id, _updates, reply) = drive(
            &addr,
            serde_json::json!([
                { "type": "text", "text": "explain" },
                {
                    "type": "resource_link",
                    "name": "main.rs",
                    "uri": "file:///work/proj/src/main.rs"
                }
            ]),
        )
        .await;
        assert_eq!(reply["result"]["stopReason"], "end_turn", "got {reply}");

        let sessions = state.sessions.lock().await;
        let history = sessions.get(&session_id).expect("history for the session");
        let Some(crate::provider::ContentPart::Text(user)) = history[0].parts.first() else {
            panic!("expected a text user message, got {:?}", history[0].parts);
        };
        assert!(user.contains("explain"), "got {user:?}");
        assert!(
            user.contains("main.rs"),
            "the reference is lost, got {user:?}"
        );
        assert!(
            user.contains("file:///work/proj/src/main.rs"),
            "the reference is lost, got {user:?}"
        );
    }

    /// Tool progress is reported as it happens, so a tool call must reach
    /// the client BEFORE the reply that depended on it — presence alone
    /// would also pass if everything were flushed at the end of the turn.
    #[tokio::test]
    async fn tool_calls_are_reported_before_the_reply() {
        // The fixture registers one tool named "echo"; script a turn that
        // calls it and then answers.
        let state = ServeState::for_test_scripted(
            true,
            vec![
                crate::provider::ChatResponse {
                    prompt_usage: None,
                    text: None,
                    tool_calls: vec![crate::provider::ToolCall {
                        id: "call-1".to_string(),
                        name: "echo".to_string(),
                        input: serde_json::json!({ "text": "ping" }),
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
        let addr = spawn(state).await;
        let (_session_id, updates, reply) = drive(&addr, text_prompt("use the tool")).await;

        let kinds: Vec<&str> = updates
            .iter()
            .map(|u| u["sessionUpdate"].as_str().unwrap())
            .collect();
        let started = kinds
            .iter()
            .position(|k| *k == "tool_call")
            .unwrap_or_else(|| panic!("no tool_call update, got {kinds:?}"));
        // Two `tool_call_update`s now, in order: the gate clearing the
        // call, then the executor finishing it. `tool_call` itself
        // fires before the gate, so it can only say `pending`.
        let mut updated = kinds
            .iter()
            .enumerate()
            .filter(|(_, k)| **k == "tool_call_update")
            .map(|(i, _)| i);
        let in_progress = updated
            .next()
            .unwrap_or_else(|| panic!("no tool_call_update, got {kinds:?}"));
        let completed = updated
            .next()
            .unwrap_or_else(|| panic!("only one tool_call_update, got {kinds:?}"));
        let first_chunk = kinds
            .iter()
            .position(|k| *k == "agent_message_chunk")
            .unwrap_or_else(|| panic!("no agent_message_chunk, got {kinds:?}"));
        assert!(
            started < in_progress && in_progress < completed && completed < first_chunk,
            "start, allowed, end, then reply, got {kinds:?}"
        );

        // The provider's own tool-call id is what ACP's toolCallId carries,
        // so a client can correlate every later update with the start.
        assert_eq!(updates[started]["toolCallId"], "call-1");
        assert_eq!(updates[started]["title"], "echo");
        // `Pending` is the schema's default, so it is omitted from the
        // wire rather than sent — a client seeing no status reads it as
        // pending. Absent is therefore the correct assertion here, and
        // anything else would mean the call claimed to be underway
        // before the gate had cleared it.
        assert!(
            updates[started]["status"].is_null(),
            "tool_call fires before the gate, so it must not claim a status: {}",
            updates[started]
        );
        assert_eq!(updates[in_progress]["toolCallId"], "call-1");
        assert_eq!(updates[in_progress]["status"], "in_progress");
        assert_eq!(updates[completed]["toolCallId"], "call-1");
        assert_eq!(updates[completed]["status"], "completed");
        assert_eq!(reply["result"]["stopReason"], "end_turn", "got {reply}");
    }

    /// Running out of tool rounds is an ordinary ending, not a broken
    /// agent, and ACP has a stop reason for exactly it. The default no
    /// longer reaches it — `interactive = 0` is unbounded — so this pins
    /// the configured case: an operator who sets a cap gets
    /// `max_turn_requests`, not an internal-error dialog.
    ///
    /// The prose is pinned too, in its new shape: one chunk per round, as
    /// each round produced it, rather than one joined chunk at the end.
    #[tokio::test]
    async fn a_configured_budget_ends_the_turn_with_max_turn_requests() {
        const ROUNDS: usize = 4;

        let script: Vec<crate::provider::ChatResponse> = (0..ROUNDS)
            .map(|i| crate::provider::ChatResponse {
                prompt_usage: None,
                text: Some(format!("step {i}")),
                tool_calls: vec![crate::provider::ToolCall {
                    id: format!("call-{i}"),
                    name: "echo".to_string(),
                    input: serde_json::json!({ "text": "ping" }),
                }],
                stop_reason: None,
            })
            .collect();
        let state = ServeState::for_test_scripted_with_rounds(
            true,
            script,
            crate::config::ToolRounds {
                interactive: ROUNDS,
                unattended: ROUNDS,
            },
        );
        let addr = spawn(state).await;
        let (_session_id, updates, reply) = drive(&addr, text_prompt("do a big refactor")).await;

        assert!(
            reply.get("error").is_none(),
            "a spent budget is not an error, got {reply}"
        );
        assert_eq!(
            reply["result"]["stopReason"], "max_turn_requests",
            "got {reply}"
        );

        // Every round's prose reached the editor, in its own chunk.
        let expected: Vec<String> = (0..ROUNDS).map(|i| format!("step {i}")).collect();
        let chunks: Vec<&str> = updates
            .iter()
            .filter(|u| u["sessionUpdate"] == "agent_message_chunk")
            .map(|u| u["content"]["text"].as_str().unwrap())
            .collect();
        assert_eq!(chunks, expected, "got {chunks:?}");
    }

    /// A session id this connection never minted is answered — not queued
    /// on the SDK's session-scoped retry, and not treated as a fresh
    /// session.
    #[tokio::test]
    async fn prompt_for_an_unknown_session_is_an_error() {
        let addr = spawn(ServeState::for_test(true)).await;
        let responses = conversation(
            &addr,
            vec![
                initialize_request(0),
                prompt_request(1, "no-such-session", text_prompt("hi")),
            ],
        )
        .await;

        assert_eq!(
            responses[1]["error"]["code"], -32602,
            "an unknown sessionId is a bad parameter, got {}",
            responses[1]
        );
    }

    /// A provider failure is a JSON-RPC error carrying the cause, not a
    /// stop reason: none of ACP's stop reasons means "the agent broke",
    /// and `refusal` would tell the user the agent declined.
    #[tokio::test]
    async fn a_provider_failure_answers_with_an_error_carrying_the_cause() {
        // An empty script makes the stub provider fail the chat call.
        let state = ServeState::for_test_scripted(true, Vec::new());
        let addr = spawn(state).await;
        let (_session_id, updates, reply) = drive(&addr, text_prompt("hi")).await;

        assert!(
            !updates
                .iter()
                .any(|u| u["sessionUpdate"] == "agent_message_chunk"),
            "a failed turn has no reply to send, got {updates:?}"
        );
        assert!(reply["result"].is_null(), "got {reply}");
        assert_eq!(reply["error"]["code"], -32603, "got {reply}");
        let data = reply["error"]["data"].as_str().unwrap_or_default();
        assert!(
            data.contains("script exhausted"),
            "the error must carry the provider's cause, got {reply}"
        );
    }

    /// `session/cancel` carries no id: it is a notification, and gets no
    /// reply of its own. The reply it produces is the one to the prompt.
    fn cancel_notification(session_id: &str) -> serde_json::Value {
        serde_json::json!({
            "jsonrpc": "2.0",
            "method": "session/cancel",
            "params": { "sessionId": session_id }
        })
    }

    /// initialize → session/new → session/prompt on one connection, returning
    /// as soon as the prompt (request id 2) is on the wire.
    ///
    /// Unlike [`drive`], the socket comes back open and undrained, so the
    /// caller decides how the turn ends — by cancelling it or by vanishing.
    /// Only usable with a provider that does not answer, which is what makes
    /// "still in flight" true rather than merely likely.
    async fn prompt_in_flight(addr: &str) -> (TestSocket, String) {
        let mut ws = connect(addr).await;
        for request in [initialize_request(0), new_session_request(1)] {
            ws.send(Message::Text(request.to_string().into()))
                .await
                .unwrap();
        }

        loop {
            let Message::Text(t) = next_frame(&mut ws).await else {
                continue;
            };
            let v: serde_json::Value = serde_json::from_str(&t).unwrap();
            if v["id"] == 1 {
                let session_id = v["result"]["sessionId"]
                    .as_str()
                    .expect("sessionId present")
                    .to_string();
                ws.send(Message::Text(
                    prompt_request(2, &session_id, text_prompt("hang"))
                        .to_string()
                        .into(),
                ))
                .await
                .unwrap();
                return (ws, session_id);
            }
        }
    }

    /// The schema makes `cancelled` a MUST for a turn stopped by
    /// `session/cancel`, "even if the cancellation causes exceptions in
    /// underlying operations".
    ///
    /// The provider never returns, so the turn is unambiguously still running
    /// when the cancel arrives — which also makes this a test of *where* the
    /// turn runs. A turn awaited inside the dispatch loop would stop that loop
    /// from ever parsing this notification, and the reply would never come.
    #[tokio::test]
    async fn session_cancel_ends_the_turn_with_cancelled() {
        let (state, hanging) = ServeState::for_test_hanging(true);
        let addr = spawn(state).await;
        let (mut ws, session_id) = prompt_in_flight(&addr).await;

        // Cancelling before the turn reaches the provider would prove
        // nothing about tearing a live call down, so wait for it to get
        // there first.
        HangingChat::wait_for(&hanging.entered, 1, "the turn to reach the provider").await;

        ws.send(Message::Text(
            cancel_notification(&session_id).to_string().into(),
        ))
        .await
        .unwrap();

        loop {
            let Message::Text(t) = next_frame(&mut ws).await else {
                continue;
            };
            let v: serde_json::Value = serde_json::from_str(&t).unwrap();
            if v["id"] == 2 {
                assert_eq!(v["result"]["stopReason"], "cancelled", "got {v}");
                break;
            }
        }

        // The stop reason alone would also be satisfied by a handler that
        // answers `cancelled` and leaves the turn running.
        assert_eq!(
            hanging.dropped.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the provider call outlived the cancellation it reported"
        );
    }

    /// A client that leaves mid-turn must stop the turn: a tool loop still
    /// calling a provider with nobody listening spends money and can still
    /// write to the workspace.
    ///
    /// The assertion is the hanging provider's drop counter, not the server's
    /// liveness: a test that merely reconnects and finds the server healthy
    /// passes whether or not the turn is still burning tokens.
    async fn assert_a_disconnect_stops_the_turn(say_goodbye: bool) {
        let (state, hanging) = ServeState::for_test_hanging(true);
        let addr = spawn(state).await;
        let (mut ws, _session_id) = prompt_in_flight(&addr).await;

        HangingChat::wait_for(&hanging.entered, 1, "the turn to reach the provider").await;
        if say_goodbye {
            ws.send(Message::Close(None)).await.unwrap();
        }
        drop(ws);

        HangingChat::wait_for(
            &hanging.dropped,
            1,
            "the turn to stop after the client left",
        )
        .await;
    }

    /// The polite exit — the one an editor performs on quit.
    ///
    /// Neither this nor its rude sibling isolates *our* cancellation: both
    /// were measured passing with `cancel_when_exhausted`'s guard removed,
    /// because the SDK tears the turn down on its own either way (a clean EOF
    /// finishes `connect_to`'s foreground and drops the task actor; an
    /// abrupt one fails the actors outright). They pin the observable
    /// contract — a client that leaves stops paying for a turn — and
    /// `the_transport_cancels_when_the_frame_stream_ends` pins the mechanism
    /// this module adds on top.
    #[tokio::test]
    async fn a_closing_client_stops_an_in_flight_turn() {
        assert_a_disconnect_stops_the_turn(true).await;
    }

    /// The rude exit: a client whose process died.
    #[tokio::test]
    async fn a_vanishing_client_stops_an_in_flight_turn() {
        assert_a_disconnect_stops_the_turn(false).await;
    }

    /// The connection's cancellation must fire the moment the socket stops
    /// producing frames.
    ///
    /// Tested directly on [`cancel_when_exhausted`] rather than through a
    /// live connection, because an end-to-end test cannot tell this guard
    /// apart from the SDK's own teardown: at EOF `connect_to`'s foreground
    /// (`incoming_closed` then `drain_outgoing`) finishes and the task actor
    /// holding the turn is dropped with it, so a turn stops either way. What
    /// only the guard gives is the *timing* — before the drain, which a
    /// client that has stopped reading can hold up indefinitely.
    #[tokio::test]
    async fn the_transport_cancels_when_the_frame_stream_ends() {
        let cancel = CancellationToken::new();
        let mut frames = Box::pin(cancel_when_exhausted(
            futures_util::stream::iter(vec!["{}".to_string()]),
            cancel.clone(),
        ));

        assert_eq!(frames.next().await.as_deref(), Some("{}"));
        assert!(
            !cancel.is_cancelled(),
            "a connection still delivering frames must not be cancelled"
        );

        assert!(frames.next().await.is_none(), "the stream ends here");
        assert!(
            cancel.is_cancelled(),
            "the end of the frame stream must cancel the connection's turns"
        );
    }

    /// `session/cancel` names a *session*, not a request — the schema scopes
    /// it to "ongoing operations for a session" — and prompts on one
    /// connection now run concurrently, so a session can have two turns open
    /// at once. Both must answer `cancelled`.
    ///
    /// Keeping only the newest turn's token would leave the older one calling
    /// the provider until the connection died, and then answering `end_turn`:
    /// the one path in this design where a cancelled turn does not report
    /// `Cancelled`, which the schema makes a MUST.
    #[tokio::test]
    async fn session_cancel_ends_every_turn_on_the_session() {
        let (state, hanging) = ServeState::for_test_hanging(true);
        let addr = spawn(state).await;
        // Leaves prompt id 2 in flight.
        let (mut ws, session_id) = prompt_in_flight(&addr).await;
        HangingChat::wait_for(&hanging.entered, 1, "the first turn to reach the provider").await;

        // A second prompt on the SAME session while the first still runs.
        ws.send(Message::Text(
            prompt_request(3, &session_id, text_prompt("hang as well"))
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
        HangingChat::wait_for(&hanging.entered, 2, "the second turn to reach the provider").await;

        // One cancel, naming the session both turns belong to.
        ws.send(Message::Text(
            cancel_notification(&session_id).to_string().into(),
        ))
        .await
        .unwrap();

        let mut cancelled = Vec::new();
        while cancelled.len() < 2 {
            let Message::Text(t) = next_frame(&mut ws).await else {
                continue;
            };
            let v: serde_json::Value = serde_json::from_str(&t).unwrap();
            if v["id"] == 2 || v["id"] == 3 {
                assert_eq!(
                    v["result"]["stopReason"], "cancelled",
                    "every turn on a cancelled session answers cancelled, got {v}"
                );
                cancelled.push(v["id"].clone());
            }
        }
        cancelled.sort_by_key(|id| id.as_i64().unwrap_or_default());
        assert_eq!(cancelled, vec![serde_json::json!(2), serde_json::json!(3)]);

        assert_eq!(
            hanging.dropped.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "both provider calls must be abandoned, not just the newest"
        );
    }

    /// Scripts a turn that calls `risky` once, then replies.
    fn risky_then_reply(reply: &str) -> Vec<crate::provider::ChatResponse> {
        vec![
            crate::provider::ChatResponse {
                prompt_usage: None,
                text: None,
                tool_calls: vec![crate::provider::ToolCall {
                    id: "call-1".to_string(),
                    name: "risky".to_string(),
                    input: serde_json::json!({}),
                }],
                stop_reason: None,
            },
            crate::provider::ChatResponse {
                prompt_usage: None,
                text: Some(reply.to_string()),
                tool_calls: Vec::new(),
                stop_reason: None,
            },
        ]
    }

    /// An `Execute` tool in the default mode puts the question to the
    /// user, and an allow lets it run.
    #[tokio::test]
    async fn an_execute_tool_asks_and_runs_when_allowed() {
        let state = ServeState::for_test_scripted(true, risky_then_reply("done"));
        let risky = super::super::RiskyTool::new();
        let ran = risky.ran_flag();
        state.tools.register_tool(Box::new(risky)).await;
        let addr = spawn(state).await;

        let (_updates, reply, asked) =
            drive_answering(&addr, text_prompt("run it"), "allow_once").await;

        assert_eq!(asked, 1, "exactly one permission request, got {asked}");
        assert!(
            ran.load(std::sync::atomic::Ordering::SeqCst),
            "an allowed tool must actually run"
        );
        assert_eq!(reply["result"]["stopReason"], "end_turn", "got {reply}");
    }

    /// A refusal does not end the turn: the model gets a tool_result
    /// saying so and answers normally. Showing the user an error dialog
    /// because they declined would be wrong twice over — the agent is
    /// fine, and they already know what they chose.
    #[tokio::test]
    async fn a_refusal_does_not_end_the_turn() {
        let state = ServeState::for_test_scripted(true, risky_then_reply("understood"));
        let risky = super::super::RiskyTool::new();
        let ran = risky.ran_flag();
        state.tools.register_tool(Box::new(risky)).await;
        let addr = spawn(state).await;

        let (updates, reply, asked) =
            drive_answering(&addr, text_prompt("run it"), "reject_once").await;

        assert_eq!(asked, 1);
        assert!(
            !ran.load(std::sync::atomic::Ordering::SeqCst),
            "a declined tool must not run"
        );
        assert_eq!(
            reply["result"]["stopReason"], "end_turn",
            "a declined tool is not a failed turn, got {reply}"
        );
        let chunks: Vec<&str> = updates
            .iter()
            .filter(|u| u["sessionUpdate"] == "agent_message_chunk")
            .filter_map(|u| u["content"]["text"].as_str())
            .collect();
        assert_eq!(chunks, vec!["understood"]);
    }

    /// A `Read` tool is never put to the user. This is what keeps the
    /// feature usable: a dialog per `file_read` would be intolerable.
    #[tokio::test]
    async fn a_safe_tool_is_not_put_to_the_user() {
        let state = ServeState::for_test_scripted(
            true,
            vec![
                crate::provider::ChatResponse {
                    prompt_usage: None,
                    text: None,
                    tool_calls: vec![crate::provider::ToolCall {
                        id: "call-1".to_string(),
                        name: "echo".to_string(),
                        input: serde_json::json!({ "text": "ping" }),
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
        let addr = spawn(state).await;

        let (_updates, reply, asked) =
            drive_answering(&addr, text_prompt("echo"), "allow_once").await;

        assert_eq!(asked, 0, "a Read tool must not ask");
        assert_eq!(reply["result"]["stopReason"], "end_turn");
    }

    /// `allow_always` is recorded, so a second call in the same turn
    /// uses the standing answer instead of asking again.
    #[tokio::test]
    async fn allow_always_is_not_asked_twice() {
        let state = ServeState::for_test_scripted(
            true,
            vec![
                crate::provider::ChatResponse {
                    prompt_usage: None,
                    text: None,
                    tool_calls: vec![crate::provider::ToolCall {
                        id: "call-1".to_string(),
                        name: "risky".to_string(),
                        input: serde_json::json!({}),
                    }],
                    stop_reason: None,
                },
                crate::provider::ChatResponse {
                    prompt_usage: None,
                    text: None,
                    tool_calls: vec![crate::provider::ToolCall {
                        id: "call-2".to_string(),
                        name: "risky".to_string(),
                        input: serde_json::json!({}),
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
        state
            .tools
            .register_tool(Box::new(super::super::RiskyTool::new()))
            .await;
        let addr = spawn(state).await;

        let (_updates, reply, asked) =
            drive_answering(&addr, text_prompt("run it twice"), "allow_always").await;

        assert_eq!(asked, 1, "the second call must use the recorded answer");
        assert_eq!(reply["result"]["stopReason"], "end_turn", "got {reply}");
    }

    fn set_mode_request(id: i64, session_id: &str, mode_id: &str) -> serde_json::Value {
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "session/set_mode",
            "params": { "sessionId": session_id, "modeId": mode_id }
        })
    }

    /// The client learns the modes when the session is created, and
    /// starts in the one that asks.
    #[tokio::test]
    async fn session_new_advertises_the_three_modes() {
        let state = ServeState::for_test_scripted(true, Vec::new());
        let addr = spawn(state).await;
        let replies =
            conversation(&addr, vec![initialize_request(0), new_session_request(1)]).await;

        let modes = &replies[1]["result"]["modes"];
        assert_eq!(modes["currentModeId"], "default", "got {modes}");
        let ids: Vec<&str> = modes["availableModes"]
            .as_array()
            .expect("availableModes is an array")
            .iter()
            .map(|m| m["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, vec!["default", "accept_edits", "bypass"]);
        // A picker with no labels is not a picker.
        assert!(
            modes["availableModes"][0]["name"]
                .as_str()
                .is_some_and(|n| !n.is_empty()),
            "each mode needs a human-readable name, got {modes}"
        );
    }

    /// Switching modes is acknowledged and announced.
    #[tokio::test]
    async fn set_mode_switches_and_notifies() {
        let state = ServeState::for_test_scripted(true, Vec::new());
        let addr = spawn(state).await;

        let mut ws = connect(&addr).await;
        for request in [initialize_request(0), new_session_request(1)] {
            ws.send(Message::Text(request.to_string().into()))
                .await
                .unwrap();
        }

        let mut saw_mode_update = false;
        loop {
            let Message::Text(t) = next_frame(&mut ws).await else {
                continue;
            };
            let v: serde_json::Value = serde_json::from_str(&t).unwrap();
            if v["id"] == 1 {
                let id = v["result"]["sessionId"].as_str().unwrap().to_string();
                ws.send(Message::Text(
                    set_mode_request(2, &id, "bypass").to_string().into(),
                ))
                .await
                .unwrap();
            } else if v["method"] == "session/update"
                && v["params"]["update"]["sessionUpdate"] == "current_mode_update"
            {
                assert_eq!(v["params"]["update"]["currentModeId"], "bypass");
                saw_mode_update = true;
            } else if v["id"] == 2 {
                assert!(v["error"].is_null(), "set_mode failed: {v}");
                break;
            }
        }
        assert!(saw_mode_update, "a mode change must be announced");
    }

    /// `plan` is not implemented, and must not silently resolve to
    /// something else — a client told "fine" would believe the agent is
    /// planning when it is about to act.
    ///
    /// Hand-rolled rather than via `roundtrip`, because a session lives
    /// only as long as the connection that minted it: a second
    /// connection would fail on the session id, not on the mode id, and
    /// the test would pass for the wrong reason.
    #[tokio::test]
    async fn an_unknown_mode_is_invalid_params() {
        let state = ServeState::for_test_scripted(true, Vec::new());
        let addr = spawn(state).await;

        let mut ws = connect(&addr).await;
        for request in [initialize_request(0), new_session_request(1)] {
            ws.send(Message::Text(request.to_string().into()))
                .await
                .unwrap();
        }

        loop {
            let Message::Text(t) = next_frame(&mut ws).await else {
                continue;
            };
            let v: serde_json::Value = serde_json::from_str(&t).unwrap();
            if v["id"] == 1 {
                let id = v["result"]["sessionId"].as_str().unwrap().to_string();
                ws.send(Message::Text(
                    set_mode_request(2, &id, "plan").to_string().into(),
                ))
                .await
                .unwrap();
            } else if v["id"] == 2 {
                assert_eq!(v["error"]["code"], -32602, "got {v}");
                assert!(
                    v["error"]["data"]
                        .as_str()
                        .is_some_and(|d| d.contains("plan")),
                    "the error should name the mode it rejected, got {v}"
                );
                break;
            }
        }
    }

    /// The mode is not decoration: `bypass` stops the asking.
    #[tokio::test]
    async fn bypass_mode_does_not_ask() {
        let state = ServeState::for_test_scripted(true, risky_then_reply("done"));
        let risky = super::super::RiskyTool::new();
        let ran = risky.ran_flag();
        state.tools.register_tool(Box::new(risky)).await;
        let addr = spawn(state).await;

        let mut ws = connect(&addr).await;
        for request in [initialize_request(0), new_session_request(1)] {
            ws.send(Message::Text(request.to_string().into()))
                .await
                .unwrap();
        }

        loop {
            let Message::Text(t) = next_frame(&mut ws).await else {
                continue;
            };
            let v: serde_json::Value = serde_json::from_str(&t).unwrap();
            if v["id"] == 1 {
                let id = v["result"]["sessionId"].as_str().unwrap().to_string();
                ws.send(Message::Text(
                    set_mode_request(2, &id, "bypass").to_string().into(),
                ))
                .await
                .unwrap();
                ws.send(Message::Text(
                    prompt_request(3, &id, text_prompt("run it"))
                        .to_string()
                        .into(),
                ))
                .await
                .unwrap();
            } else if v["method"] == "session/request_permission" {
                panic!("bypass mode must not ask, got {v}");
            } else if v["id"] == 3 {
                assert_eq!(v["result"]["stopReason"], "end_turn", "got {v}");
                break;
            }
        }
        assert!(
            ran.load(std::sync::atomic::Ordering::SeqCst),
            "bypass should have run the tool, not merely skipped asking"
        );
    }

    /// `accept_edits` is the middle mode, and the only one whose whole
    /// point is that it changes *some* answers and not others. An
    /// `Execute` tool must still be asked about there.
    #[tokio::test]
    async fn accept_edits_still_asks_about_commands() {
        let state = ServeState::for_test_scripted(true, risky_then_reply("done"));
        state
            .tools
            .register_tool(Box::new(super::super::RiskyTool::new()))
            .await;
        let addr = spawn(state).await;

        let mut ws = connect(&addr).await;
        for request in [initialize_request(0), new_session_request(1)] {
            ws.send(Message::Text(request.to_string().into()))
                .await
                .unwrap();
        }

        let mut asked = 0usize;
        loop {
            let Message::Text(t) = next_frame(&mut ws).await else {
                continue;
            };
            let v: serde_json::Value = serde_json::from_str(&t).unwrap();
            if v["id"] == 1 {
                let id = v["result"]["sessionId"].as_str().unwrap().to_string();
                ws.send(Message::Text(
                    set_mode_request(2, &id, "accept_edits").to_string().into(),
                ))
                .await
                .unwrap();
                ws.send(Message::Text(
                    prompt_request(3, &id, text_prompt("run it"))
                        .to_string()
                        .into(),
                ))
                .await
                .unwrap();
            } else if v["method"] == "session/request_permission" {
                asked += 1;
                let answer = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": v["id"],
                    "result": { "outcome": { "outcome": "selected", "optionId": "allow_once" } }
                });
                ws.send(Message::Text(answer.to_string().into()))
                    .await
                    .unwrap();
            } else if v["id"] == 3 {
                assert_eq!(v["result"]["stopReason"], "end_turn", "got {v}");
                break;
            }
        }
        assert_eq!(
            asked, 1,
            "accept_edits must still ask about an Execute tool"
        );
    }

    fn list_request(id: i64, cwd: Option<&str>) -> serde_json::Value {
        let params = match cwd {
            Some(cwd) => serde_json::json!({ "cwd": cwd }),
            None => serde_json::json!({}),
        };
        serde_json::json!({
            "jsonrpc": "2.0", "id": id, "method": "session/list", "params": params
        })
    }

    /// `initialize` then an unfiltered `session/list` on a fresh
    /// connection, returning just the session ids in the reply's order.
    async fn list_session_ids(addr: &str) -> Vec<String> {
        let replies = conversation(addr, vec![initialize_request(0), list_request(1, None)]).await;
        replies[1]["result"]["sessions"]
            .as_array()
            .expect("sessions is an array")
            .iter()
            .map(|s| s["sessionId"].as_str().unwrap().to_string())
            .collect()
    }

    /// The listing shows ACP sessions and nothing else. An editor's
    /// thread list must not contain the user's Matrix/`/rpc` conversations
    /// — that mixing is what this whole branch exists to undo. Unlike the
    /// other `list_*` tests below, this one puts a real entry in
    /// `cross_device_session_store` and asserts it does NOT leak through,
    /// so an edit that made `session/list` read the shared store again
    /// (instead of `acp_session_store`) would fail it even though every
    /// other list test only ever populates the ACP store.
    #[tokio::test]
    async fn the_listing_shows_only_acp_sessions() {
        let state = ServeState::for_test_scripted(true, Vec::new());
        let ours = state
            .config
            .namespace_for_room_profile("developer")
            .to_string();

        state
            .acp_session_store
            .create("acp-1", &ours, "/work")
            .unwrap();
        state
            .acp_session_store
            .append_message("acp-1", &crate::provider::ChatMessage::user("hi"))
            .unwrap();

        // A conversation from the shared store, which must not appear.
        state
            .cross_device_session_store
            .ensure_session("rpc-1", &("rpc-1".to_string(), None), "rpc", None, &ours)
            .unwrap();

        let addr = spawn(state).await;
        let ids = list_session_ids(&addr).await;
        assert_eq!(ids, vec!["acp-1".to_string()], "got {ids:?}");
    }

    /// A thread the editor opened and never typed into is not a
    /// conversation. Zed calls `session/new` on every panel open, so
    /// listing those would bury the real ones.
    #[tokio::test]
    async fn an_empty_session_is_not_listed() {
        let state = ServeState::for_test_scripted(true, Vec::new());
        let ours = state
            .config
            .namespace_for_room_profile("developer")
            .to_string();
        state
            .acp_session_store
            .create("never-used", &ours, "/work")
            .unwrap();

        let addr = spawn(state).await;
        assert!(
            list_session_ids(&addr).await.is_empty(),
            "an unprompted session must not be listed"
        );
    }

    /// The boundary. A token pinned to one room profile must not see
    /// another profile's conversations.
    #[tokio::test]
    async fn list_only_returns_this_namespaces_sessions() {
        let state = ServeState::for_test_scripted(true, Vec::new());
        let store = Arc::clone(&state.acp_session_store);
        let ours = state
            .config
            .namespace_for_room_profile("developer")
            .to_string();

        store.create("mine", &ours, "/p").unwrap();
        store
            .append_message("mine", &crate::provider::ChatMessage::user("hi"))
            .unwrap();
        store.create("theirs", "someone-else", "/p").unwrap();
        store
            .append_message("theirs", &crate::provider::ChatMessage::user("hi"))
            .unwrap();

        let addr = spawn(state).await;
        let replies = conversation(&addr, vec![initialize_request(0), list_request(1, None)]).await;

        let ids: Vec<&str> = replies[1]["result"]["sessions"]
            .as_array()
            .expect("sessions is an array")
            .iter()
            .map(|s| s["sessionId"].as_str().unwrap())
            .collect();
        assert!(ids.contains(&"mine"), "got {ids:?}");
        assert!(
            !ids.contains(&"theirs"),
            "another namespace leaked into the list: {ids:?}"
        );
        assert_eq!(replies[1]["result"]["nextCursor"], serde_json::Value::Null);
    }

    /// A header line that fails to parse cannot say whose it is — the same
    /// property `main`'s `SessionStore` protects with an optional
    /// `namespace`, but `AcpSessionStore`'s `SessionHeader.namespace` is a
    /// required field, so the equivalent of "written before namespace
    /// existed" is a header the type cannot even deserialize. An unknown
    /// owner is not the same as "mine", so it is not listed either way.
    #[tokio::test]
    async fn list_omits_sessions_with_an_unparseable_header() {
        let state = ServeState::for_test_scripted(true, Vec::new());
        let store = Arc::clone(&state.acp_session_store);
        let ours = state
            .config
            .namespace_for_room_profile("developer")
            .to_string();
        store.create("mine", &ours, "/p").unwrap();
        store
            .append_message("mine", &crate::provider::ChatMessage::user("hi"))
            .unwrap();

        // Hand-written: a "header" line missing the `namespace` field
        // `SessionHeader` requires, so it fails to deserialize.
        let dir = store
            .absolute_path_for("mine")
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf();
        std::fs::write(
            dir.join("broken.jsonl"),
            format!(
                "{}\n",
                serde_json::json!({
                    "kind": "header",
                    "session_id": "broken",
                    "cwd": "/p",
                    "created_at": "2020-01-01T00:00:00Z"
                })
            ),
        )
        .unwrap();

        let addr = spawn(state).await;
        let replies = conversation(&addr, vec![initialize_request(0), list_request(1, None)]).await;

        let ids: Vec<&str> = replies[1]["result"]["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["sessionId"].as_str().unwrap())
            .collect();
        assert_eq!(
            ids,
            vec!["mine"],
            "an unparseable header must not leak into the list: {ids:?}"
        );
    }

    /// A closed session is archived, not current. Listing it would offer
    /// the user a thread the agent has already summarised and moved on
    /// from.
    #[tokio::test]
    async fn list_omits_closed_sessions() {
        let state = ServeState::for_test_scripted(true, Vec::new());
        let store = Arc::clone(&state.acp_session_store);
        let ours = state
            .config
            .namespace_for_room_profile("developer")
            .to_string();

        store.create("open", &ours, "/p").unwrap();
        store
            .append_message("open", &crate::provider::ChatMessage::user("hi"))
            .unwrap();
        store.create("closed", &ours, "/p").unwrap();
        store
            .append_message("closed", &crate::provider::ChatMessage::user("hi"))
            .unwrap();
        store.close("closed").unwrap();

        let addr = spawn(state).await;
        let replies = conversation(&addr, vec![initialize_request(0), list_request(1, None)]).await;

        let ids: Vec<&str> = replies[1]["result"]["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["sessionId"].as_str().unwrap())
            .collect();
        assert_eq!(ids, vec!["open"], "got {ids:?}");
    }

    /// The editor filters by project: an exact match against the recorded
    /// `cwd`. `AcpSessionStore`'s `cwd` is a required field — every ACP
    /// session reports one at `session/new` — so unlike the old shared
    /// store there is no "no cwd" case left to test here.
    #[tokio::test]
    async fn list_honours_the_cwd_filter() {
        let state = ServeState::for_test_scripted(true, Vec::new());
        let store = Arc::clone(&state.acp_session_store);
        let ours = state
            .config
            .namespace_for_room_profile("developer")
            .to_string();

        store.create("here", &ours, "/projects/here").unwrap();
        store
            .append_message("here", &crate::provider::ChatMessage::user("hi"))
            .unwrap();
        store
            .create("elsewhere", &ours, "/projects/elsewhere")
            .unwrap();
        store
            .append_message("elsewhere", &crate::provider::ChatMessage::user("hi"))
            .unwrap();

        let addr = spawn(state).await;
        let replies = conversation(
            &addr,
            vec![
                initialize_request(0),
                list_request(1, Some("/projects/here")),
                list_request(2, None),
            ],
        )
        .await;

        let ids = |r: &serde_json::Value| -> Vec<String> {
            r["result"]["sessions"]
                .as_array()
                .unwrap()
                .iter()
                .map(|s| s["sessionId"].as_str().unwrap().to_string())
                .collect()
        };

        assert_eq!(
            ids(&replies[1]),
            vec!["here".to_string()],
            "filtered by cwd"
        );
        let all = ids(&replies[2]);
        assert!(all.contains(&"here".to_string()), "got {all:?}");
        assert!(all.contains(&"elsewhere".to_string()), "got {all:?}");
        assert_eq!(all.len(), 2, "unfiltered must include both, got {all:?}");
    }

    #[tokio::test]
    async fn initialize_advertises_listing() {
        let state = ServeState::for_test_scripted(true, Vec::new());
        let addr = spawn(state).await;
        let reply = roundtrip(&addr, initialize_request(0)).await;
        assert!(
            !reply["result"]["agentCapabilities"]["sessionCapabilities"]["list"].is_null(),
            "got {reply}"
        );
    }

    fn load_request(id: i64, session_id: &str) -> serde_json::Value {
        serde_json::json!({
            "jsonrpc": "2.0", "id": id, "method": "session/load",
            "params": { "sessionId": session_id, "cwd": test_cwd(), "mcpServers": [] }
        })
    }

    /// Loading replays the conversation as session/update notifications,
    /// and does so BEFORE answering — a client that got the reply first
    /// would render an empty thread and then have messages appear under
    /// it.
    #[tokio::test]
    async fn load_replays_the_conversation_before_replying() {
        let state = ServeState::for_test_scripted(true, Vec::new());
        let store = Arc::clone(&state.acp_session_store);
        let ours = state
            .config
            .namespace_for_room_profile("developer")
            .to_string();
        let sid = "sess-replay".to_string();
        store.create(&sid, &ours, "/p").unwrap();
        store
            .append_message(&sid, &crate::provider::ChatMessage::user("first"))
            .unwrap();
        store
            .append_message(&sid, &crate::provider::ChatMessage::assistant("second"))
            .unwrap();

        let addr = spawn(state).await;
        let mut ws = connect(&addr).await;
        for request in [initialize_request(0), load_request(1, &sid)] {
            ws.send(Message::Text(request.to_string().into()))
                .await
                .unwrap();
        }

        let mut replayed: Vec<(String, String)> = Vec::new();
        loop {
            let Message::Text(t) = next_frame(&mut ws).await else {
                continue;
            };
            let v: serde_json::Value = serde_json::from_str(&t).unwrap();
            if v["method"] == "session/update" {
                let u = &v["params"]["update"];
                replayed.push((
                    u["sessionUpdate"].as_str().unwrap().to_string(),
                    u["content"]["text"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string(),
                ));
            } else if v["id"] == 1 {
                assert!(v["error"].is_null(), "load failed: {v}");
                break;
            }
        }

        assert_eq!(
            replayed,
            vec![
                ("user_message_chunk".to_string(), "first".to_string()),
                ("agent_message_chunk".to_string(), "second".to_string()),
            ],
            "the replay must arrive in order, before the reply"
        );
    }

    /// The boundary that filtering the list cannot provide: `load` takes
    /// an id directly, so a session that never appears in any list is
    /// still reachable by anyone who learns its id.
    #[tokio::test]
    async fn load_refuses_another_namespaces_session() {
        let state = ServeState::for_test_scripted(true, Vec::new());
        let store = Arc::clone(&state.acp_session_store);
        let theirs = "theirs".to_string();
        store.create(&theirs, "someone-else", "/p").unwrap();

        let addr = spawn(state).await;
        let replies =
            conversation(&addr, vec![initialize_request(0), load_request(1, &theirs)]).await;
        assert_eq!(replies[1]["error"]["code"], -32602, "got {}", replies[1]);
    }

    /// The two refusals must be indistinguishable. If "not yours" reads
    /// differently from "no such session", the pair enumerates ids.
    #[tokio::test]
    async fn an_unknown_and_a_forbidden_session_look_the_same() {
        let state = ServeState::for_test_scripted(true, Vec::new());
        let store = Arc::clone(&state.acp_session_store);
        let theirs = "theirs".to_string();
        store.create(&theirs, "someone-else", "/p").unwrap();

        let addr = spawn(state).await;
        let replies = conversation(
            &addr,
            vec![
                initialize_request(0),
                load_request(1, &theirs),
                load_request(2, "01900000-0000-7000-8000-000000000000"),
            ],
        )
        .await;

        assert_eq!(
            replies[1]["error"], replies[2]["error"],
            "the two refusals differ"
        );
    }

    /// `session/list` excludes a closed session so it drops out of the
    /// picker, but `session/load` took an id directly. If load still
    /// honoured it, an archived conversation would be gone from the list
    /// yet reachable and appendable by anyone who kept its id — and the
    /// two refusal paths would no longer be the only way in, breaking the
    /// "cannot enumerate ids" property the identical wording exists for.
    #[tokio::test]
    async fn load_refuses_a_closed_session() {
        let state = ServeState::for_test_scripted(true, Vec::new());
        let store = Arc::clone(&state.acp_session_store);
        let ours = state
            .config
            .namespace_for_room_profile("developer")
            .to_string();
        let sid = "sess-closed".to_string();
        store.create(&sid, &ours, "/p").unwrap();
        store.close(&sid).unwrap();

        let addr = spawn(state).await;
        let replies = conversation(
            &addr,
            vec![
                initialize_request(0),
                load_request(1, &sid),
                load_request(2, "01900000-0000-7000-8000-000000000000"),
            ],
        )
        .await;

        assert_eq!(
            replies[1]["error"], replies[2]["error"],
            "a closed session must refuse the same way an unknown one does, got {}",
            replies[1]
        );
    }

    /// A loaded session is a real session: prompting it continues the
    /// conversation rather than starting a new one. This is what proves
    /// the adapter registered the *existing* id, not a fresh one.
    #[tokio::test]
    async fn a_loaded_session_continues_its_history() {
        let state = ServeState::for_test_scripted(
            true,
            vec![crate::provider::ChatResponse {
                prompt_usage: None,
                text: Some("third".to_string()),
                tool_calls: Vec::new(),
                stop_reason: None,
            }],
        );
        let store = Arc::clone(&state.acp_session_store);
        let ours = state
            .config
            .namespace_for_room_profile("developer")
            .to_string();
        let sid = "sess-continues".to_string();
        store.create(&sid, &ours, "/p").unwrap();
        store
            .append_message(&sid, &crate::provider::ChatMessage::user("first"))
            .unwrap();

        let addr = spawn(state).await;
        let mut ws = connect(&addr).await;
        for request in [
            initialize_request(0),
            load_request(1, &sid),
            prompt_request(2, &sid, text_prompt("second")),
        ] {
            ws.send(Message::Text(request.to_string().into()))
                .await
                .unwrap();
        }
        loop {
            let Message::Text(t) = next_frame(&mut ws).await else {
                continue;
            };
            let v: serde_json::Value = serde_json::from_str(&t).unwrap();
            if v["id"] == 2 {
                assert_eq!(v["result"]["stopReason"], "end_turn", "got {v}");
                break;
            }
        }

        let history = store.history(&sid).expect("the session still exists");
        let texts: Vec<String> = history
            .iter()
            .flat_map(|m| &m.parts)
            .filter_map(|p| match p {
                crate::provider::ContentPart::Text(t) => Some(t.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(texts, vec!["first", "second", "third"], "got {texts:?}");
    }

    #[tokio::test]
    async fn initialize_advertises_loading() {
        let state = ServeState::for_test_scripted(true, Vec::new());
        let addr = spawn(state).await;
        let reply = roundtrip(&addr, initialize_request(0)).await;
        assert_eq!(reply["result"]["agentCapabilities"]["loadSession"], true);
    }

    fn resume_request(id: i64, session_id: &str) -> serde_json::Value {
        serde_json::json!({
            "jsonrpc": "2.0", "id": id, "method": "session/resume",
            "params": { "sessionId": session_id, "cwd": test_cwd(), "mcpServers": [] }
        })
    }

    /// `resume` is `load` without the replay — for a client that wants
    /// the session back without paying to redraw a long conversation.
    /// It must still continue the history.
    #[tokio::test]
    async fn resume_continues_without_replaying() {
        let state = ServeState::for_test_scripted(
            true,
            vec![crate::provider::ChatResponse {
                prompt_usage: None,
                text: Some("second".to_string()),
                tool_calls: Vec::new(),
                stop_reason: None,
            }],
        );
        let store = Arc::clone(&state.acp_session_store);
        let ours = state
            .config
            .namespace_for_room_profile("developer")
            .to_string();
        let sid = "sess-resume".to_string();
        store.create(&sid, &ours, "/p").unwrap();
        store
            .append_message(&sid, &crate::provider::ChatMessage::user("first"))
            .unwrap();

        let addr = spawn(state).await;
        let mut ws = connect(&addr).await;
        for request in [initialize_request(0), resume_request(1, &sid)] {
            ws.send(Message::Text(request.to_string().into()))
                .await
                .unwrap();
        }

        let mut updates_before_reply = 0usize;
        loop {
            let Message::Text(t) = next_frame(&mut ws).await else {
                continue;
            };
            let v: serde_json::Value = serde_json::from_str(&t).unwrap();
            if v["method"] == "session/update" {
                updates_before_reply += 1;
            } else if v["id"] == 1 {
                assert!(v["error"].is_null(), "resume failed: {v}");
                break;
            }
        }
        assert_eq!(updates_before_reply, 0, "resume must not replay");

        // ...and the history is still there for the next turn.
        ws.send(Message::Text(
            prompt_request(2, &sid, text_prompt("x")).to_string().into(),
        ))
        .await
        .unwrap();
        loop {
            let Message::Text(t) = next_frame(&mut ws).await else {
                continue;
            };
            let v: serde_json::Value = serde_json::from_str(&t).unwrap();
            if v["id"] == 2 {
                assert_eq!(v["result"]["stopReason"], "end_turn", "got {v}");
                break;
            }
        }
        let history = store.history(&sid).unwrap();
        assert!(history.len() >= 3, "the resumed session kept its history");
    }

    #[tokio::test]
    async fn initialize_advertises_resume() {
        let state = ServeState::for_test_scripted(true, Vec::new());
        let addr = spawn(state).await;
        let reply = roundtrip(&addr, initialize_request(0)).await;
        assert!(
            !reply["result"]["agentCapabilities"]["sessionCapabilities"]["resume"].is_null(),
            "got {reply}"
        );
    }

    /// A header line too broken to say whose it is refuses the same way an
    /// unknown or another namespace's session does. `session/list` already
    /// omits these (`list_omits_sessions_with_an_unparseable_header`);
    /// `load` must not let a client reach one directly by id either — an
    /// unknown owner is not the same as "mine".
    #[tokio::test]
    async fn load_refuses_a_session_with_an_unparseable_header() {
        let state = ServeState::for_test_scripted(true, Vec::new());
        let store = Arc::clone(&state.acp_session_store);
        let ours = state
            .config
            .namespace_for_room_profile("developer")
            .to_string();
        let mine = "mine".to_string();
        store.create(&mine, &ours, "/p").unwrap();

        // Same technique as `list_omits_sessions_with_an_unparseable_header`:
        // a "header" line missing the `namespace` field `SessionHeader`
        // requires, so `summary()` cannot see it at all.
        let dir = store
            .absolute_path_for(&mine)
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf();
        let broken_id = "broken";
        std::fs::write(
            dir.join(format!("{broken_id}.jsonl")),
            format!(
                "{}\n",
                serde_json::json!({
                    "kind": "header",
                    "session_id": broken_id,
                    "cwd": "/p",
                    "created_at": "2020-01-01T00:00:00Z"
                })
            ),
        )
        .unwrap();

        let addr = spawn(state).await;
        let replies = conversation(
            &addr,
            vec![initialize_request(0), load_request(1, broken_id)],
        )
        .await;
        assert_eq!(replies[1]["error"]["code"], -32602, "got {}", replies[1]);
    }

    /// Two connections can now hold the same session, which makes the
    /// history race in `run_llm_turn` reachable across connections
    /// rather than only within one. The race is not fixed here — this
    /// only makes hitting it observable, because a corrupted transcript
    /// with no log line is undebuggable.
    #[tokio::test]
    async fn opening_a_session_twice_is_counted() {
        let state = ServeState::for_test_scripted(true, Vec::new());
        let store = Arc::clone(&state.acp_session_store);
        let counts = Arc::clone(&state.open_acp_sessions);
        let ours = state
            .config
            .namespace_for_room_profile("developer")
            .to_string();
        let sid = "sess-twice".to_string();
        store.create(&sid, &ours, "/p").unwrap();

        let addr = spawn(state).await;
        let mut a = connect(&addr).await;
        let mut b = connect(&addr).await;
        for ws in [&mut a, &mut b] {
            for request in [initialize_request(0), load_request(1, &sid)] {
                ws.send(Message::Text(request.to_string().into()))
                    .await
                    .unwrap();
            }
            loop {
                let Message::Text(t) = next_frame(ws).await else {
                    continue;
                };
                let v: serde_json::Value = serde_json::from_str(&t).unwrap();
                if v["id"] == 1 {
                    assert!(v["error"].is_null(), "load failed: {v}");
                    break;
                }
            }
        }

        assert_eq!(
            counts.lock().await.get(&sid).copied(),
            Some(2),
            "both connections should be counted as holding the session"
        );
    }

    /// The bug the review caught: re-adopting a session this connection
    /// already holds — `session/load` then `session/resume` on the same
    /// socket, the realistic shape of a client retry — must not count as
    /// a second holder. `sessions.inner.insert` overwrites the existing
    /// entry rather than adding one, so if the increment ran
    /// unconditionally the count would drift to 2 while only one
    /// connection actually holds it, and that phantom entry would
    /// outlive the connection: teardown only removes one holder per
    /// entry still in the map, leaving 1 forever.
    #[tokio::test]
    async fn readopting_a_session_on_the_same_connection_is_not_double_counted() {
        let state = ServeState::for_test_scripted(true, Vec::new());
        let store = Arc::clone(&state.acp_session_store);
        let counts = Arc::clone(&state.open_acp_sessions);
        let ours = state
            .config
            .namespace_for_room_profile("developer")
            .to_string();
        let sid = "sess-readopt".to_string();
        store.create(&sid, &ours, "/p").unwrap();

        let addr = spawn(state).await;
        let mut ws = connect(&addr).await;
        for request in [
            initialize_request(0),
            load_request(1, &sid),
            resume_request(2, &sid),
        ] {
            ws.send(Message::Text(request.to_string().into()))
                .await
                .unwrap();
            loop {
                let Message::Text(t) = next_frame(&mut ws).await else {
                    continue;
                };
                let v: serde_json::Value = serde_json::from_str(&t).unwrap();
                if v["id"] == request["id"] {
                    assert!(v["error"].is_null(), "adopting failed: {v}");
                    break;
                }
            }
        }

        assert_eq!(
            counts.lock().await.get(&sid).copied(),
            Some(1),
            "the same connection re-adopting its own session must not be \
             counted as a second holder"
        );

        ws.send(Message::Close(None)).await.unwrap();
        drop(ws);

        let released = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                if counts.lock().await.get(&sid).is_none() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await;
        assert!(
            released.is_ok(),
            "timed out waiting for the closed connection to release its session — a \
             phantom holder left the entry stuck above 0"
        );
    }

    /// The other half of the counter: a connection that goes away must
    /// release every session it held. Skipping this would turn
    /// `open_acp_sessions` into a leak — the next `session/load` for a
    /// session whose real owner disconnected long ago would find a
    /// stale count still above one and warn about a collision that no
    /// longer exists.
    #[tokio::test]
    async fn closing_a_connection_releases_its_sessions() {
        let state = ServeState::for_test_scripted(true, Vec::new());
        let store = Arc::clone(&state.acp_session_store);
        let counts = Arc::clone(&state.open_acp_sessions);
        let ours = state
            .config
            .namespace_for_room_profile("developer")
            .to_string();
        let sid = "sess-close-conn".to_string();
        store.create(&sid, &ours, "/p").unwrap();

        let addr = spawn(state).await;
        let mut ws = connect(&addr).await;
        for request in [initialize_request(0), load_request(1, &sid)] {
            ws.send(Message::Text(request.to_string().into()))
                .await
                .unwrap();
        }
        loop {
            let Message::Text(t) = next_frame(&mut ws).await else {
                continue;
            };
            let v: serde_json::Value = serde_json::from_str(&t).unwrap();
            if v["id"] == 1 {
                assert!(v["error"].is_null(), "load failed: {v}");
                break;
            }
        }
        assert_eq!(
            counts.lock().await.get(&sid).copied(),
            Some(1),
            "load should have registered the session as open"
        );

        ws.send(Message::Close(None)).await.unwrap();
        drop(ws);

        let released = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                if counts.lock().await.get(&sid).is_none() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await;
        assert!(
            released.is_ok(),
            "timed out waiting for the closed connection to release its session"
        );
    }

    /// A non-ACP turn must not find a client. `/rpc`, Matrix, Discord
    /// and voice have no editor on the other end.
    #[test]
    fn the_default_turn_host_offers_no_client() {
        assert!(NullProgress.acp_client().is_none());
    }

    /// `fs_caps_from` must read the recorded flags — not a constant,
    /// and not the wrong field. Asymmetric input in both directions is
    /// the point: identical flags cannot catch a read/write swap, and
    /// a single direction cannot catch a constant `(true, true)` or
    /// `(false, false)`.
    #[test]
    fn fs_caps_from_reads_the_recorded_flags_independently() {
        let read_only =
            ClientCapabilities::new().fs(FileSystemCapabilities::new().read_text_file(true));
        assert_eq!(fs_caps_from(&read_only), (true, false));

        let write_only =
            ClientCapabilities::new().fs(FileSystemCapabilities::new().write_text_file(true));
        assert_eq!(fs_caps_from(&write_only), (false, true));
    }

    /// `terminal_cap_from` must read the recorded flag, not return a
    /// constant.
    #[test]
    fn terminal_cap_from_reads_the_recorded_flag() {
        assert!(terminal_cap_from(&ClientCapabilities::new().terminal(true)));
        assert!(!terminal_cap_from(
            &ClientCapabilities::new().terminal(false)
        ));
    }

    /// The log line has to distinguish all three flags, or reading it
    /// back during an incident answers the wrong question. Asymmetric
    /// input for the same reason as `fs_caps_from`'s test above: a
    /// rendering that printed one flag three times would pass on
    /// uniform input.
    #[test]
    fn describe_capabilities_names_each_flag_separately() {
        let caps = ClientCapabilities::new()
            .fs(FileSystemCapabilities::new().read_text_file(true))
            .terminal(true);
        assert_eq!(
            describe_capabilities(&caps),
            "fs.read=true fs.write=false terminal=true"
        );

        assert_eq!(
            describe_capabilities(&ClientCapabilities::new()),
            "fs.read=false fs.write=false terminal=false"
        );
    }

    /// `client_info` is optional in ACP v1, and a client that omits it
    /// must still produce a legible line — an empty name would read as
    /// this code being broken rather than as the client saying nothing.
    #[test]
    fn describe_client_falls_back_when_the_client_says_nothing() {
        assert_eq!(describe_client(None), "an unidentified client");
    }

    /// `title` is the human-facing name and wins when the client sends
    /// one; `name` is the programmatic id the schema documents as the
    /// fallback.
    #[test]
    fn describe_client_prefers_the_title_over_the_programmatic_name() {
        let bare = Implementation::new("some-editor".to_string(), "1.2.3".to_string());
        assert_eq!(describe_client(Some(&bare)), "some-editor 1.2.3");

        let titled = Implementation::new("some-editor".to_string(), "1.2.3".to_string())
            .title("Some Editor".to_string());
        assert_eq!(describe_client(Some(&titled)), "Some Editor 1.2.3");
    }

    /// `effective_cwd` picks between two genuinely different paths —
    /// not two spellings of the same one — so an implementation that
    /// swapped which side wins would fail loudly instead of passing by
    /// coincidence.
    #[test]
    fn effective_cwd_prefers_the_requested_path_and_falls_back_to_the_session() {
        let session_cwd = Path::new(if cfg!(windows) {
            "C:\\session\\root"
        } else {
            "/session/root"
        });
        let requested = if cfg!(windows) {
            "C:\\model\\chosen"
        } else {
            "/model/chosen"
        };

        assert_eq!(
            effective_cwd(Some(requested), session_cwd),
            PathBuf::from(requested),
            "an explicit cwd from the model must win"
        );
        assert_eq!(
            effective_cwd(None, session_cwd),
            session_cwd.to_path_buf(),
            "absent that, the session's cwd is the fallback"
        );
    }

    // `initialize_records_the_clients_capabilities` — deliberately not
    // written here. The plan anticipated this: `AcpSession` and
    // `AcpSessions` (where `client_capabilities` lands, see the field
    // doc on each) live entirely inside `serve_connection`'s spawned
    // task, with no accessor a test outside that task can reach — the
    // only view this test module has onto a live connection is the
    // wire, and nothing on the wire echoes capabilities back. What
    // Task 4 added instead: `fs_caps_from` (above) proves the two
    // recorded flags are read out correctly and independently, and
    // `serve::tests::the_two_fs_tools_follow_their_own_capability_flags`
    // proves `visible_tool_predicate` turns them into the right tool
    // list — between the two, a client's declared capability is
    // observable end to end even though no single test crosses the
    // wire. See task-3-report.md and task-4-report.md for the record
    // of this decision.
}
