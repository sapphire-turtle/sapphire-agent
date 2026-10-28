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
//! - [`SEND`] `{session_id, content, attachments?, client?}` runs one turn
//!   and answers `{content}` once it is done. `attachments` are
//!   [`Attachment`]s (`{mime, data}`, base64) sent with the message, and
//!   `client` is the [`ClientCaps`] the client declares for this turn; both
//!   are left out when empty. While the turn runs, [`PROGRESS`]
//!   `{session_id, kind, ..}` notifies each tool call starting
//!   (`kind: "tool_start"`, `"tool_end"`, with `id` and `name`), and the
//!   avatar cues the reply carries (`"expression"` with `expression` and
//!   `intensity?`, `"action"` with `action`); see [`parse_progress`].
//! - [`HISTORY`] `{session_id}` answers `{messages}`.
//! - [`SESSIONS`] answers `{sessions}`.
//!
//! There is no token: the IPC endpoint admits only the OS user that runs
//! the server, and the agent's `[cli].room_profile` names the room profile.

use anyhow::{Context as _, Result, anyhow, bail};
use base64::Engine as _;
use sapphire_framework::ipc::{Client, ClientInfo, Notification};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// The app name the agent's IPC endpoint is registered under.
pub const APP: &str = "sapphire-agent";

/// The version of the agent's own IPC methods — these and its
/// `sapphire-agent.acp.*` — apart from the framework's. The handshake
/// refuses a server that speaks another, so a client never talks to an
/// agent that would read its calls differently, or lacks them.
///
/// 2: added `sapphire-agent.chat.*`.
/// 3: `chat.send` takes `attachments` and `client` (the client's declared
/// capabilities); `chat.progress` carries `expression` and `action`.
pub const APP_API: u32 = 3;

/// Open (resume or reserve) a session.
pub const OPEN: &str = "sapphire-agent.chat.open";
/// Run one turn; answers with the reply.
pub const SEND: &str = "sapphire-agent.chat.send";
/// A session's stored messages.
pub const HISTORY: &str = "sapphire-agent.chat.history";
/// The client sessions.
pub const SESSIONS: &str = "sapphire-agent.chat.sessions";
/// Notification: progress in a [`SEND`] turn (a tool call, an avatar cue).
pub const PROGRESS: &str = "sapphire-agent.chat.progress";

/// [`PROGRESS`] `kind`: a tool call started.
pub const KIND_TOOL_START: &str = "tool_start";
/// [`PROGRESS`] `kind`: a tool call ended.
pub const KIND_TOOL_END: &str = "tool_end";
/// [`PROGRESS`] `kind`: the reply sets the avatar's expression.
pub const KIND_EXPRESSION: &str = "expression";
/// [`PROGRESS`] `kind`: the reply makes the avatar perform an action.
pub const KIND_ACTION: &str = "action";

/// What a client can do, declared on each [`SEND`] turn.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct ClientCaps {
    /// Present when the client shows an avatar.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avatar: Option<AvatarCaps>,
}

/// The cues an avatar can play.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct AvatarCaps {
    pub expressions: Vec<String>,
    #[serde(default)]
    pub actions: Vec<String>,
}

/// A file sent with a message. `data` is base64, as on the wire.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Attachment {
    pub mime: String,
    pub data: String,
}

impl Attachment {
    pub fn from_bytes(mime: impl Into<String>, bytes: &[u8]) -> Self {
        Self {
            mime: mime.into(),
            data: base64::engine::general_purpose::STANDARD.encode(bytes),
        }
    }
}

static NO_CAPS: ClientCaps = ClientCaps { avatar: None };

/// One turn to [`ChatClient::send`].
pub struct Turn<'a> {
    pub content: &'a str,
    pub attachments: &'a [Attachment],
    pub client: &'a ClientCaps,
}

impl<'a> Turn<'a> {
    /// A text-only turn from a client declaring nothing.
    pub fn text(content: &'a str) -> Turn<'a> {
        Turn {
            content,
            attachments: &[],
            client: &NO_CAPS,
        }
    }
}

/// The [`SEND`] params for `turn`. Empty `attachments` and a default
/// `client` are left out, so a plain turn is the same JSON as in API 2.
pub(crate) fn send_params(session_id: &str, turn: &Turn) -> Value {
    let mut params = json!({ "session_id": session_id, "content": turn.content });
    if !turn.attachments.is_empty() {
        params["attachments"] = json!(turn.attachments);
    }
    if *turn.client != ClientCaps::default() {
        params["client"] = json!(turn.client);
    }
    params
}

/// Progress in a [`SEND`] turn, from [`PROGRESS`].
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq)]
pub enum ChatEvent {
    Tool(ToolEvent),
    Expression { name: String, intensity: f32 },
    Action { name: String },
}

/// Read a [`PROGRESS`] notification's `params`. `None` for another
/// session's, or for a `kind` this client does not know.
pub fn parse_progress(session_id: &str, params: &Value) -> Option<ChatEvent> {
    if params["session_id"].as_str() != Some(session_id) {
        return None;
    }
    let tool = |started| {
        ChatEvent::Tool(ToolEvent {
            started,
            id: params["id"].as_str().unwrap_or_default().to_owned(),
            name: params["name"].as_str().unwrap_or("?").to_owned(),
        })
    };
    match params["kind"].as_str()? {
        KIND_TOOL_START => Some(tool(true)),
        KIND_TOOL_END => Some(tool(false)),
        KIND_EXPRESSION => Some(ChatEvent::Expression {
            name: params["expression"].as_str()?.to_owned(),
            intensity: params["intensity"].as_f64().map_or(1.0, |i| i as f32),
        }),
        KIND_ACTION => Some(ChatEvent::Action {
            name: params["action"].as_str()?.to_owned(),
        }),
        _ => None,
    }
}

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
#[derive(Debug, Clone, PartialEq)]
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

    /// Run one turn in `session_id`, handing each [`ChatEvent`] to
    /// `on_event` as it arrives, and return the reply.
    pub async fn send(
        &self,
        session_id: &str,
        turn: Turn<'_>,
        mut on_event: impl FnMut(ChatEvent),
    ) -> Result<String> {
        // Subscribed before the call, so no notification can be missed.
        let mut events = self.client.notifications();
        let call = self
            .client
            .call::<_, Value>(SEND, send_params(session_id, &turn));
        tokio::pin!(call);
        let mut handle = |event: Notification| {
            if event.method != PROGRESS {
                return;
            }
            if let Some(event) = parse_progress(session_id, &event.params) {
                on_event(event);
            }
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

    #[test]
    fn progress_kinds_parse_into_events() {
        let s = "s1";
        assert_eq!(
            parse_progress(
                s,
                &json!({"session_id":"s1","kind":"tool_start","id":"c1","name":"risky"})
            ),
            Some(ChatEvent::Tool(ToolEvent {
                started: true,
                id: "c1".into(),
                name: "risky".into()
            }))
        );
        assert_eq!(
            parse_progress(
                s,
                &json!({"session_id":"s1","kind":"expression","expression":"happy","intensity":0.5})
            ),
            Some(ChatEvent::Expression {
                name: "happy".into(),
                intensity: 0.5
            })
        );
        assert_eq!(
            parse_progress(
                s,
                &json!({"session_id":"s1","kind":"action","action":"peace"})
            ),
            Some(ChatEvent::Action {
                name: "peace".into()
            })
        );
    }

    #[test]
    fn progress_for_another_session_or_kind_is_ignored() {
        assert_eq!(
            parse_progress(
                "s1",
                &json!({"session_id":"s2","kind":"action","action":"peace"})
            ),
            None
        );
        assert_eq!(
            parse_progress("s1", &json!({"session_id":"s1","kind":"audio"})),
            None
        );
    }

    #[test]
    fn an_expression_without_intensity_is_full() {
        assert_eq!(
            parse_progress(
                "s1",
                &json!({"session_id":"s1","kind":"expression","expression":"sad"})
            ),
            Some(ChatEvent::Expression {
                name: "sad".into(),
                intensity: 1.0
            })
        );
    }

    #[test]
    fn a_plain_turn_serialises_like_api_2() {
        assert_eq!(
            send_params("s1", &Turn::text("hi")),
            json!({"session_id":"s1","content":"hi"})
        );
    }

    #[test]
    fn attachments_and_caps_are_sent_when_present() {
        let attachments = [Attachment::from_bytes("image/png", b"abc")];
        let client = ClientCaps {
            avatar: Some(AvatarCaps {
                expressions: vec!["happy".into()],
                actions: vec![],
            }),
        };
        let turn = Turn {
            content: "hi",
            attachments: &attachments,
            client: &client,
        };
        assert_eq!(
            send_params("s1", &turn),
            json!({
                "session_id": "s1",
                "content": "hi",
                "attachments": [{"mime": "image/png", "data": "YWJj"}],
                "client": {"avatar": {"expressions": ["happy"], "actions": []}},
            })
        );
    }
}
