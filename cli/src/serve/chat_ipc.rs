//! Chat over IPC: `sapphire-agent chat`, carried to the running agent over
//! the app server's IPC endpoint.
//!
//! The REPL is a client of the agent already running on this host, the way
//! `sapphire-agent acp` is — and the way every sapphire-framework app's CLI
//! reaches its own server — rather than an HTTP client of `/rpc`:
//!
//! ```text
//! terminal ──▶ sapphire-agent chat ──IPC──▶ sapphire-agent serve
//! ```
//!
//! The methods mirror `/rpc`'s client surface, and share its handlers'
//! bodies (see [`open_session`]):
//!
//! - [`OPEN`] `{session?}` resumes the session with that public id, or
//!   reserves a new one, and answers like `initialize`.
//! - [`SEND`] `{session_id, content}` runs one turn and answers
//!   `{content}` once it is done. While it runs, [`PROGRESS`]
//!   `{session_id, kind, id, name}` notifies each tool call starting
//!   (`kind: "tool_start"`) and ending (`"tool_end"`).
//! - [`HISTORY`] `{session_id}` answers `{messages}`, like `get_session`.
//! - [`SESSIONS`] answers `{sessions}`, like `list_sessions`.
//!
//! There is no token on this path. The IPC endpoint admits only the OS user
//! that runs the server, and the room profile a token would have picked is
//! named by `[cli].room_profile` instead.

use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{Context as _, Result, anyhow, bail};
use sapphire_framework::ipc::{
    Client, ClientInfo, PeerHandle, RequestCtx, Router, RpcError, codes,
};
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::{info, warn};

use super::{
    ServeState, TurnHost, open_session, run_llm_turn, session_history, session_list,
    spawn_session_title,
};
use crate::provider::ChatMessage;

/// Open (resume or reserve) a session.
pub const OPEN: &str = "sapphire-agent.chat.open";
/// Run one turn; answers with the reply.
pub const SEND: &str = "sapphire-agent.chat.send";
/// A session's stored messages.
pub const HISTORY: &str = "sapphire-agent.chat.history";
/// The client sessions.
pub const SESSIONS: &str = "sapphire-agent.chat.sessions";
/// Notification: a tool call in a [`SEND`] turn started or ended.
pub const PROGRESS: &str = "sapphire-agent.chat.progress";

/// The server's side: the methods above, registered on the app server's
/// router.
///
/// Built before the agent, like [`super::acp_tunnel::AcpTunnel`], so the
/// [`ServeState`] arrives later through [`attach`](Self::attach); until then
/// every method says the agent is still starting.
#[derive(Clone, Default)]
pub struct ChatTunnel {
    state: Arc<OnceLock<Arc<ServeState>>>,
}

#[derive(Deserialize)]
struct OpenParams {
    #[serde(default)]
    session: Option<String>,
}

#[derive(Deserialize)]
struct SendParams {
    session_id: String,
    content: String,
}

#[derive(Deserialize)]
struct HistoryParams {
    session_id: String,
}

fn invalid(message: impl Into<String>) -> RpcError {
    RpcError {
        code: codes::INVALID_PARAMS,
        message: message.into(),
        data: None,
    }
}

fn failed(message: impl Into<String>) -> RpcError {
    RpcError {
        code: codes::INTERNAL_ERROR,
        message: message.into(),
        data: None,
    }
}

impl ChatTunnel {
    /// Start answering, with the agent now built.
    pub fn attach(&self, state: Arc<ServeState>) {
        if self.state.set(state).is_err() {
            warn!("chat tunnel: attached twice; keeping the first agent");
        }
    }

    /// Register the methods on `router`.
    pub fn router(&self, router: Router) -> Router {
        let open = self.clone();
        let send = self.clone();
        let history = self.clone();
        let sessions = self.clone();
        router
            .method(OPEN, move |ctx| {
                let tunnel = open.clone();
                async move { tunnel.open(ctx).await }
            })
            .method(SEND, move |ctx| {
                let tunnel = send.clone();
                async move { tunnel.send(ctx).await }
            })
            .method(HISTORY, move |ctx| {
                let tunnel = history.clone();
                async move { tunnel.history(ctx).await }
            })
            .method(SESSIONS, move |_ctx| {
                let tunnel = sessions.clone();
                async move { tunnel.sessions().await }
            })
    }

    fn agent(&self) -> std::result::Result<Arc<ServeState>, RpcError> {
        self.state.get().cloned().ok_or_else(|| {
            failed(
                "the agent is still starting; try again once `sapphire-agent status` shows it \
                 serving",
            )
        })
    }

    async fn open(&self, ctx: RequestCtx) -> std::result::Result<Value, RpcError> {
        let state = self.agent()?;
        let params: OpenParams = serde_json::from_value(ctx.params)
            .map_err(|e| invalid(format!("bad {OPEN} parameters: {e}")))?;
        let Some(profile) = state
            .config
            .cli
            .as_ref()
            .and_then(|c| c.room_profile.clone())
        else {
            return Err(failed(
                "`sapphire-agent chat` needs a room profile: set [cli].room_profile in this \
                 host's config.toml",
            ));
        };
        let opened = open_session(&state, None, params.session.as_deref(), profile, None)
            .await
            .map_err(|(code, message)| RpcError {
                code,
                message: message.to_owned(),
                data: None,
            })?;
        info!(
            "chat: session {} opened for room profile '{}' (pid {})",
            opened.public_id.as_deref().unwrap_or(&opened.session_id),
            opened.room_profile,
            ctx.peer.client().pid
        );
        Ok(opened.to_json())
    }

    async fn send(&self, ctx: RequestCtx) -> std::result::Result<Value, RpcError> {
        let state = self.agent()?;
        let params: SendParams = serde_json::from_value(ctx.params)
            .map_err(|e| invalid(format!("bad {SEND} parameters: {e}")))?;
        let content = params.content.trim();
        if content.is_empty() {
            return Err(invalid("nothing to send"));
        }
        // Only a session `open` pinned to a room profile: a stale id (from
        // before a restart, say) would otherwise run under no profile.
        if !state
            .session_room_profiles
            .lock()
            .await
            .contains_key(&params.session_id)
        {
            return Err(invalid(format!(
                "no open session {}; open it first",
                params.session_id
            )));
        }

        let host = Arc::new(IpcProgress {
            peer: ctx.peer,
            session_id: params.session_id.clone(),
            error: Mutex::new(None),
        });
        let outcome = run_llm_turn(
            Arc::clone(&state),
            params.session_id.clone(),
            ChatMessage::user(content),
            Arc::clone(&host) as Arc<dyn TurnHost>,
            None,
        )
        .await;

        let Some(text) = outcome.text else {
            let message = host
                .error
                .lock()
                .unwrap()
                .take()
                .unwrap_or_else(|| "No response generated".to_owned());
            return Err(failed(message));
        };
        if outcome.was_first_turn {
            spawn_session_title(state, params.session_id, content.to_owned(), text.clone());
        }
        Ok(json!({ "content": text }))
    }

    async fn history(&self, ctx: RequestCtx) -> std::result::Result<Value, RpcError> {
        let state = self.agent()?;
        let params: HistoryParams = serde_json::from_value(ctx.params)
            .map_err(|e| invalid(format!("bad {HISTORY} parameters: {e}")))?;
        Ok(json!({ "messages": session_history(&state, &params.session_id) }))
    }

    async fn sessions(&self) -> std::result::Result<Value, RpcError> {
        let state = self.agent()?;
        Ok(json!({ "sessions": session_list(&state) }))
    }
}

/// A [`SEND`] turn's progress, as [`PROGRESS`] notifications to the client
/// that sent it.
struct IpcProgress {
    peer: PeerHandle,
    session_id: String,
    /// What [`TurnHost::turn_error`] reported, for the answer to carry.
    error: Mutex<Option<String>>,
}

impl IpcProgress {
    async fn tool(&self, kind: &str, id: &str, name: &str) {
        // Best effort: a client that has gone needs no word.
        let _ = self
            .peer
            .notify(
                PROGRESS,
                json!({ "session_id": self.session_id, "kind": kind, "id": id, "name": name }),
            )
            .await;
    }
}

#[async_trait::async_trait]
impl TurnHost for IpcProgress {
    async fn tool_start(&self, id: &str, name: &str, _input: &Value) {
        self.tool("tool_start", id, name).await;
    }

    async fn tool_end(&self, id: &str, name: &str) {
        self.tool("tool_end", id, name).await;
    }

    async fn turn_error(&self, message: &str) {
        *self.error.lock().unwrap() = Some(message.to_owned());
    }
}

/// The client's side: the running agent, reached over its IPC endpoint.
pub struct ChatClient {
    client: Client,
}

/// A session [`ChatClient::open`] opened.
#[derive(Debug)]
pub struct Opened {
    /// The internal id later calls name it by.
    pub session_id: String,
    /// The id shown to people.
    pub display_id: String,
    pub is_new: bool,
}

/// One tool call starting or ending, from [`PROGRESS`].
pub struct ToolEvent {
    pub started: bool,
    pub id: String,
    pub name: String,
}

impl ChatClient {
    /// Dial the agent running on this host. Starts none.
    pub async fn connect() -> Result<Self> {
        let app = crate::APP_CTX.app_name;
        let endpoint = sapphire_framework::ipc::Endpoint::for_app(app)?;
        let info = ClientInfo {
            kind: "chat".to_owned(),
            version: crate::app_server::VERSION.to_owned(),
            api: sapphire_framework_server_api::API_VERSION,
            app_api: Some(super::acp_tunnel::APP_API),
            pid: std::process::id(),
        };
        let Some((client, _)) = sapphire_framework::ipc::connect_or_absent(&endpoint, app, info)
            .await
            .context("connecting to the running agent")?
        else {
            bail!(
                "{app} is not running on this host; `{app} chat` talks to a running agent and \
                 starts none. Start it with `{app} serve`, or install it as a service with \
                 `{app} service install`"
            );
        };
        Ok(Self { client })
    }

    /// Resume the session with public id `session`, or reserve a new one.
    pub async fn open(&self, session: Option<&str>) -> Result<Opened> {
        let result: Value = self
            .client
            .call(OPEN, json!({ "session": session }))
            .await
            .map_err(|e| anyhow!(e))?;
        let session_id = result["session_id"]
            .as_str()
            .ok_or_else(|| anyhow!("{OPEN} answered no session id"))?
            .to_owned();
        let display_id = result["public_id"]
            .as_str()
            .unwrap_or(&session_id)
            .to_owned();
        Ok(Opened {
            session_id,
            display_id,
            is_new: result["is_new"].as_bool().unwrap_or(true),
        })
    }

    /// Run one turn in `session_id`, handing each tool event to `on_tool`
    /// as it arrives, and return the reply.
    pub async fn send(
        &self,
        session_id: &str,
        content: &str,
        mut on_tool: impl FnMut(ToolEvent),
    ) -> Result<String> {
        // Subscribed before the call, so no notification can be missed.
        let mut events = self.client.notifications();
        let call = self.client.call::<_, Value>(
            SEND,
            json!({ "session_id": session_id, "content": content }),
        );
        tokio::pin!(call);
        let mut handle = |event: sapphire_framework::ipc::Notification| {
            if event.method != PROGRESS || event.params["session_id"].as_str() != Some(session_id) {
                return;
            }
            let started = match event.params["kind"].as_str() {
                Some("tool_start") => true,
                Some("tool_end") => false,
                _ => return,
            };
            on_tool(ToolEvent {
                started,
                id: event.params["id"].as_str().unwrap_or_default().to_owned(),
                name: event.params["name"].as_str().unwrap_or("?").to_owned(),
            });
        };
        let answered = loop {
            tokio::select! {
                // Notifications first: the server sends each before its
                // answer, so one already here belongs before the reply.
                biased;
                event = events.recv() => {
                    if let Ok(event) = event {
                        handle(event);
                    }
                }
                answered = &mut call => break answered,
            }
        };
        // Whatever arrived with the answer.
        while let Ok(event) = events.try_recv() {
            handle(event);
        }
        let result = answered.map_err(|e| anyhow!(e))?;
        Ok(result["content"].as_str().unwrap_or_default().to_owned())
    }

    /// `session_id`'s stored messages.
    pub async fn history(&self, session_id: &str) -> Result<Vec<Value>> {
        let result: Value = self
            .client
            .call(HISTORY, json!({ "session_id": session_id }))
            .await
            .map_err(|e| anyhow!(e))?;
        Ok(result["messages"].as_array().cloned().unwrap_or_default())
    }

    /// The client sessions.
    pub async fn sessions(&self) -> Result<Vec<Value>> {
        let result: Value = self
            .client
            .call(SESSIONS, json!({}))
            .await
            .map_err(|e| anyhow!(e))?;
        Ok(result["sessions"].as_array().cloned().unwrap_or_default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{ChatResponse, ToolCall};
    use sapphire_framework::ipc::{Connection, ManagedBy, ServerInfo, serve};

    const APP: &str = "sapphire-agent";

    /// A tunnel served on one end of an in-process IPC connection, and a
    /// chat client handshaken on the other.
    async fn connected(tunnel: &ChatTunnel) -> ChatClient {
        let (server_end, client_end) = Connection::pair();
        let router = Arc::new(tunnel.router(Router::new()));
        let server_info = ServerInfo {
            version: "test".into(),
            api: 1,
            app_api: Some(super::super::acp_tunnel::APP_API),
            pid: std::process::id(),
            managed_by: ManagedBy::Service,
        };
        tokio::spawn(async move { serve(server_end, router, APP, server_info).await });
        let client_info = ClientInfo {
            kind: "chat".into(),
            version: "test".into(),
            api: 1,
            app_api: Some(super::super::acp_tunnel::APP_API),
            pid: std::process::id(),
        };
        let (client, _) = Client::handshake(client_end, APP, client_info)
            .await
            .unwrap();
        ChatClient { client }
    }

    fn reply_text(text: &str) -> ChatResponse {
        ChatResponse {
            prompt_usage: None,
            text: Some(text.to_string()),
            tool_calls: Vec::new(),
            stop_reason: None,
        }
    }

    /// open → send → history → sessions, over IPC, under `[cli].room_profile`.
    #[tokio::test]
    async fn a_turn_round_trips_over_ipc() {
        let tunnel = ChatTunnel::default();
        let state = ServeState::for_test_scripted(false, vec![reply_text("hello back")]);
        tunnel.attach(Arc::clone(&state));
        let chat = connected(&tunnel).await;

        let opened = chat.open(None).await.unwrap();
        assert!(opened.is_new);
        assert_eq!(
            opened.display_id.len(),
            7,
            "a grain-id is reserved up front"
        );
        assert_eq!(
            state
                .session_room_profiles
                .lock()
                .await
                .get(&opened.session_id)
                .map(String::as_str),
            Some("developer"),
            "the session runs under [cli].room_profile"
        );

        let reply = chat
            .send(&opened.session_id, "hello", |_| {})
            .await
            .unwrap();
        assert_eq!(reply, "hello back");

        let history = chat.history(&opened.session_id).await.unwrap();
        let texts: Vec<_> = history
            .iter()
            .flat_map(|m| m["parts"].as_array().cloned().unwrap_or_default())
            .filter_map(|p| p["text"].as_str().map(str::to_owned))
            .collect();
        assert_eq!(texts, ["hello", "hello back"]);

        let sessions = chat.sessions().await.unwrap();
        assert!(
            sessions
                .iter()
                .any(|s| s["public_id"].as_str() == Some(opened.display_id.as_str())),
            "{sessions:?}"
        );

        // Resuming by the public id finds the same session.
        let resumed = chat.open(Some(&opened.display_id)).await.unwrap();
        assert_eq!(resumed.session_id, opened.session_id);
        assert!(!resumed.is_new);
    }

    /// Each tool call in the turn reaches the client as it starts and ends.
    #[tokio::test]
    async fn tool_calls_are_reported_while_the_turn_runs() {
        let state = ServeState::for_test_scripted(
            false,
            vec![
                ChatResponse {
                    prompt_usage: None,
                    text: None,
                    tool_calls: vec![ToolCall {
                        id: "call-1".to_string(),
                        name: "risky".to_string(),
                        input: json!({}),
                    }],
                    stop_reason: None,
                },
                reply_text("done"),
            ],
        );
        state
            .tools
            .register_tool(Box::new(crate::serve::RiskyTool::new()))
            .await;
        let tunnel = ChatTunnel::default();
        tunnel.attach(state);
        let chat = connected(&tunnel).await;

        let opened = chat.open(None).await.unwrap();
        let mut events = Vec::new();
        let reply = chat
            .send(&opened.session_id, "go", |e| {
                events.push((e.started, e.id, e.name))
            })
            .await
            .unwrap();
        assert_eq!(reply, "done");
        assert_eq!(
            events,
            [
                (true, "call-1".to_owned(), "risky".to_owned()),
                (false, "call-1".to_owned(), "risky".to_owned()),
            ]
        );
    }

    /// A session no `open` pinned is refused, not run under no profile.
    #[tokio::test]
    async fn sending_to_an_unopened_session_is_refused() {
        let tunnel = ChatTunnel::default();
        tunnel.attach(ServeState::for_test_scripted(false, vec![reply_text("x")]));
        let chat = connected(&tunnel).await;

        let err = chat.send("not-a-session", "hi", |_| {}).await.unwrap_err();
        assert!(format!("{err:#}").contains("no open session"), "{err:#}");
    }

    /// Until the agent is attached, every method says it is still starting.
    #[tokio::test]
    async fn a_tunnel_without_an_agent_says_it_is_starting() {
        let chat = connected(&ChatTunnel::default()).await;
        let err = chat.open(None).await.unwrap_err();
        assert!(format!("{err:#}").contains("still starting"), "{err:#}");
    }
}
