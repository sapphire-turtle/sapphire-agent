//! Client library for `sapphire-agent`'s local chat.
//!
//! The agent's clients on its own host — `sapphire-agent chat` and
//! `sapphire-agent-desktop` — reach the running agent over the
//! sapphire-framework app server's IPC endpoint, the way every
//! sapphire-framework app's CLI and GUI reach their server. This crate holds
//! the method names both ends agree on and a client for them; the server
//! side lives in the agent's `serve::chat_ipc`.
//!
//! - [`OPEN`] `{session?}` resumes the session with that public id, or
//!   reserves a new one: `{session_id, public_id?, is_new, room_profile}`.
//! - [`SEND`] `{session_id, content}` runs one turn and answers
//!   `{content}` once it is done. While it runs, [`PROGRESS`]
//!   `{session_id, kind, id, name}` notifies each tool call starting
//!   (`kind: "tool_start"`) and ending (`"tool_end"`).
//! - [`HISTORY`] `{session_id}` answers `{messages}`.
//! - [`SESSIONS`] answers `{sessions}`.
//!
//! There is no token: the IPC endpoint admits only the OS user that runs
//! the server, and the agent's `[cli].room_profile` names the room profile.

use anyhow::{Context as _, Result, anyhow, bail};
use sapphire_framework::ipc::{Client, ClientInfo, Notification};
use serde_json::{Value, json};

/// The app name the agent's IPC endpoint is registered under.
pub const APP: &str = "sapphire-agent";

/// The version of the agent's own IPC methods — these and its
/// `sapphire-agent.acp.*` — apart from the framework's. The handshake
/// refuses a server that speaks another, so a client never talks to an
/// agent that would read its calls differently, or lacks them.
///
/// 2: added `sapphire-agent.chat.*`.
pub const APP_API: u32 = 2;

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

/// The running agent, reached over its IPC endpoint.
pub struct ChatClient {
    client: Client,
}

/// A session [`ChatClient::open`] opened.
#[derive(Debug, Clone)]
pub struct Opened {
    /// The internal id later calls name it by.
    pub session_id: String,
    /// The id shown to people.
    pub display_id: String,
    pub is_new: bool,
}

/// One tool call starting or ending, from [`PROGRESS`].
#[derive(Debug, Clone)]
pub struct ToolEvent {
    pub started: bool,
    pub id: String,
    pub name: String,
}

impl ChatClient {
    /// Dial the agent running on this host. Starts none.
    ///
    /// `kind` names the client in the server's logs (`"chat"`,
    /// `"desktop"`); `version` is the client's own.
    pub async fn connect(kind: &str, version: &str) -> Result<Self> {
        let endpoint = sapphire_framework::ipc::Endpoint::for_app(APP)?;
        let info = ClientInfo {
            kind: kind.to_owned(),
            version: version.to_owned(),
            api: sapphire_framework_server_api::API_VERSION,
            app_api: Some(APP_API),
            pid: std::process::id(),
        };
        let Some((client, _)) = sapphire_framework::ipc::connect_or_absent(&endpoint, APP, info)
            .await
            .context("connecting to the running agent")?
        else {
            bail!(
                "{APP} is not running on this host; its clients talk to a running agent and \
                 start none. Start it with `{APP} serve`, or install it as a service with \
                 `{APP} service install`"
            );
        };
        Ok(Self { client })
    }

    /// Wrap a connection that has already been handshaken.
    pub fn from_client(client: Client) -> Self {
        Self { client }
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
        let mut handle = |event: Notification| {
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
