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
//! - The avatar's files, kept in the workspace's `avatar/` folder, move in
//!   [`AVATAR_CHUNK`]-sized pieces: [`AVATAR_MANIFEST`] answers an
//!   [`AvatarManifest`]; [`AVATAR_READ`] `{path, offset, len}` answers
//!   `{data, eof}` (base64); [`AVATAR_WRITE_BEGIN`] `{path, size, sha256}`
//!   answers `{upload_id}`, then [`AVATAR_WRITE_CHUNK`]
//!   `{upload_id, offset, data}` per piece and [`AVATAR_WRITE_COMMIT`]
//!   `{upload_id}` to put the file in place; [`AVATAR_SET_MANIFEST`] takes an
//!   [`AvatarConfig`] (`{vrm?, sprite?}`); [`AVATAR_REMOVE`] `{path}` deletes
//!   a file.
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
/// capabilities); `chat.progress` carries `expression` and `action`;
/// added `sapphire-agent.avatar.*`, whose config is `{vrm?, sprite?}`.
pub const APP_API: u32 = 3;

/// Open (resume or reserve) a session.
pub const OPEN: &str = "sapphire-agent.chat.open";
/// Run one turn; answers with the reply.
pub const SEND: &str = "sapphire-agent.chat.send";
/// A session's stored messages.
pub const HISTORY: &str = "sapphire-agent.chat.history";
/// The client sessions.
pub const SESSIONS: &str = "sapphire-agent.chat.sessions";
/// The avatar's config and file list.
pub const AVATAR_MANIFEST: &str = "sapphire-agent.avatar.manifest";
/// One piece of an avatar file.
pub const AVATAR_READ: &str = "sapphire-agent.avatar.read";
/// Start uploading an avatar file.
pub const AVATAR_WRITE_BEGIN: &str = "sapphire-agent.avatar.write_begin";
/// One piece of an upload.
pub const AVATAR_WRITE_CHUNK: &str = "sapphire-agent.avatar.write_chunk";
/// Finish an upload and put the file in place.
pub const AVATAR_WRITE_COMMIT: &str = "sapphire-agent.avatar.write_commit";
/// Replace the avatar config.
pub const AVATAR_SET_MANIFEST: &str = "sapphire-agent.avatar.set_manifest";
/// Delete an avatar file.
pub const AVATAR_REMOVE: &str = "sapphire-agent.avatar.remove";
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

    /// The avatar's config and every file in it.
    pub async fn avatar_manifest(&self) -> Result<AvatarManifest> {
        let result: Value = self
            .client
            .call(AVATAR_MANIFEST, json!({}))
            .await
            .map_err(|e| anyhow!(e))?;
        Ok(serde_json::from_value(result)?)
    }

    /// A whole avatar file, read [`AVATAR_CHUNK`] at a time.
    pub async fn avatar_download(&self, path: &str) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        loop {
            let result: Value = self
                .client
                .call(
                    AVATAR_READ,
                    json!({ "path": path, "offset": out.len(), "len": AVATAR_CHUNK }),
                )
                .await
                .map_err(|e| anyhow!(e))?;
            let data = result["data"].as_str().unwrap_or_default();
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(data)
                .context("avatar chunk is not valid base64")?;
            let eof = result["eof"].as_bool().unwrap_or(true);
            if bytes.is_empty() && !eof {
                bail!("the agent sent an empty avatar chunk before the end of {path}");
            }
            out.extend_from_slice(&bytes);
            if eof {
                return Ok(out);
            }
        }
    }

    /// Put `bytes` at `path` in the avatar folder.
    pub async fn avatar_upload(&self, path: &str, bytes: &[u8]) -> Result<()> {
        use sha2::{Digest as _, Sha256};
        let sha256: String = Sha256::digest(bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let begun: Value = self
            .client
            .call(
                AVATAR_WRITE_BEGIN,
                json!({ "path": path, "size": bytes.len(), "sha256": sha256 }),
            )
            .await
            .map_err(|e| anyhow!(e))?;
        let upload_id = begun["upload_id"]
            .as_str()
            .context("the agent answered no upload_id")?;
        let mut offset = 0usize;
        for chunk in bytes.chunks(AVATAR_CHUNK) {
            let _: Value = self
                .client
                .call(
                    AVATAR_WRITE_CHUNK,
                    json!({
                        "upload_id": upload_id,
                        "offset": offset,
                        "data": base64::engine::general_purpose::STANDARD.encode(chunk),
                    }),
                )
                .await
                .map_err(|e| anyhow!(e))?;
            offset += chunk.len();
        }
        let _: Value = self
            .client
            .call(AVATAR_WRITE_COMMIT, json!({ "upload_id": upload_id }))
            .await
            .map_err(|e| anyhow!(e))?;
        Ok(())
    }

    /// Replace the avatar config.
    pub async fn avatar_set_config(&self, config: &AvatarConfig) -> Result<()> {
        let _: Value = self
            .client
            .call(AVATAR_SET_MANIFEST, serde_json::to_value(config)?)
            .await
            .map_err(|e| anyhow!(e))?;
        Ok(())
    }

    /// Delete the avatar file at `path`.
    pub async fn avatar_remove(&self, path: &str) -> Result<()> {
        let _: Value = self
            .client
            .call(AVATAR_REMOVE, json!({ "path": path }))
            .await
            .map_err(|e| anyhow!(e))?;
        Ok(())
    }
}

/// The most one avatar file may weigh, in bytes.
pub const AVATAR_MAX_FILE: u64 = 256 * 1024 * 1024;
/// The most one `avatar.read` / `avatar.write_chunk` carries, in bytes.
pub const AVATAR_CHUNK: usize = 1024 * 1024;

/// The expressions a sprite pose may name an image for, besides its `base`
/// (the neutral face). They are the VRM preset expressions an avatar cue can
/// carry.
pub const SPRITE_EXPRESSIONS: [&str; 5] = ["happy", "angry", "sad", "relaxed", "surprised"];

/// What `<workspace>/avatar/avatar.toml` says. `[vrm]` names a 3D model and
/// its gestures, `[sprite]` a set of still images; either may be absent, and
/// a companion shows whichever it can. Every path is `/`-separated and
/// relative to `avatar/`.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct AvatarConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vrm: Option<VrmConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sprite: Option<SpriteConfig>,
}

/// The `[vrm]` section: the model, its idle loop, and the file that plays
/// each named action.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct VrmConfig {
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle: Option<String>,
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub actions: std::collections::BTreeMap<String, String>,
}

/// The `[sprite]` section: named poses, one of which shows by default.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct SpriteConfig {
    pub default_pose: String,
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub poses: std::collections::BTreeMap<String, SpritePose>,
}

/// One sprite pose: the neutral image and an image per expression.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct SpritePose {
    pub base: String,
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub expressions: std::collections::BTreeMap<String, String>,
}

impl AvatarConfig {
    /// Every file the config references.
    pub fn paths(&self) -> Vec<&str> {
        let mut out = Vec::new();
        if let Some(vrm) = &self.vrm {
            out.push(vrm.model.as_str());
            out.extend(vrm.idle.as_deref());
            out.extend(vrm.actions.values().map(String::as_str));
        }
        if let Some(sprite) = &self.sprite {
            for pose in sprite.poses.values() {
                out.push(pose.base.as_str());
                out.extend(pose.expressions.values().map(String::as_str));
            }
        }
        out
    }

    /// The structural rules, without looking at any file: the error names the
    /// offending key.
    pub fn check(&self) -> Result<(), String> {
        if let Some(vrm) = &self.vrm
            && vrm.actions.keys().any(String::is_empty)
        {
            return Err("vrm.actions has an empty action name".into());
        }
        let Some(sprite) = &self.sprite else {
            return Ok(());
        };
        if sprite.poses.is_empty() {
            return Err("sprite.poses is empty".into());
        }
        if !sprite.poses.contains_key(&sprite.default_pose) {
            return Err(format!(
                "sprite.default_pose {:?} is not one of sprite.poses",
                sprite.default_pose
            ));
        }
        for (name, pose) in &sprite.poses {
            if name.is_empty() {
                return Err("sprite.poses has an empty pose name".into());
            }
            for key in pose.expressions.keys() {
                if key.is_empty() {
                    return Err(format!(
                        "sprite.poses.{name}.expressions has an empty expression name"
                    ));
                }
                if !SPRITE_EXPRESSIONS.contains(&key.as_str()) {
                    return Err(format!(
                        "sprite.poses.{name}.expressions.{key} is not one of {}",
                        SPRITE_EXPRESSIONS.join(", ")
                    ));
                }
            }
        }
        Ok(())
    }
}

/// One file under `avatar/`, with the hash a client compares to decide
/// whether its copy is current.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct AvatarFile {
    pub path: String,
    pub size: u64,
    pub sha256: String,
}

/// The avatar config plus every file in `avatar/` except `avatar.toml`,
/// ordered by path.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct AvatarManifest {
    #[serde(flatten)]
    pub config: AvatarConfig,
    pub files: Vec<AvatarFile>,
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

    #[test]
    fn avatar_toml_round_trips_both_sections() {
        let text = r#"
[vrm]
model = "vrm/model.vrm"
[vrm.actions]
peace = "vrm/peace.vrma"
[sprite]
default_pose = "normal"
[sprite.poses.normal]
base = "sprite/normal.png"
[sprite.poses.normal.expressions]
happy = "sprite/normal-happy.png"
"#;
        let cfg: AvatarConfig = toml::from_str(text).unwrap();
        assert_eq!(cfg.vrm.as_ref().unwrap().model, "vrm/model.vrm");
        assert_eq!(
            cfg.sprite.as_ref().unwrap().poses["normal"].expressions["happy"],
            "sprite/normal-happy.png"
        );
        let mut paths = cfg.paths();
        paths.sort();
        assert_eq!(
            paths,
            [
                "sprite/normal-happy.png",
                "sprite/normal.png",
                "vrm/model.vrm",
                "vrm/peace.vrma"
            ]
        );
        assert_eq!(
            toml::from_str::<AvatarConfig>(&toml::to_string(&cfg).unwrap()).unwrap(),
            cfg
        );
        assert_eq!(cfg.check(), Ok(()));
    }

    #[test]
    fn sprite_rules_are_checked() {
        let pose = |base: &str, ex: &[(&str, &str)]| SpritePose {
            base: base.into(),
            expressions: ex
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        };
        let sprite = |default: &str, poses: Vec<(&str, SpritePose)>| AvatarConfig {
            vrm: None,
            sprite: Some(SpriteConfig {
                default_pose: default.into(),
                poses: poses.into_iter().map(|(k, p)| (k.to_string(), p)).collect(),
            }),
        };
        assert!(
            sprite("normal", vec![])
                .check()
                .unwrap_err()
                .contains("poses")
        );
        assert!(
            sprite("missing", vec![("normal", pose("a.png", &[]))])
                .check()
                .unwrap_err()
                .contains("missing")
        );
        assert!(
            sprite(
                "normal",
                vec![("normal", pose("a.png", &[("neutral", "b.png")]))]
            )
            .check()
            .unwrap_err()
            .contains("neutral")
        );
        assert!(
            sprite(
                "normal",
                vec![("normal", pose("a.png", &[("smug", "b.png")]))]
            )
            .check()
            .unwrap_err()
            .contains("smug")
        );
        assert!(sprite("", vec![("", pose("a.png", &[]))]).check().is_err());
        assert_eq!(AvatarConfig::default().check(), Ok(()));
    }
}
