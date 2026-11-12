use crate::attachment_store::{AttachmentOwner, AttachmentStore};
use crate::channel::{
    Channel, IncomingMessage, MAX_DOWNLOAD_BYTES, OutgoingMessage, RoomInfo,
    cache_and_prepare_attachment, dropped_attachment_note, with_attachment_notes,
};
use crate::config::MatrixConfig;
use anyhow::{Context, Result};
use async_trait::async_trait;
use matrix_sdk::{
    Client, SessionMeta, SessionTokens,
    authentication::matrix::MatrixSession,
    config::SyncSettings,
    media::{MediaFormat, MediaRequestParameters},
    ruma::{
        OwnedEventId, OwnedRoomId, OwnedUserId,
        events::relation::{InReplyTo, Reply, Thread},
        events::room::MediaSource,
        events::room::message::{
            MessageType, OriginalSyncRoomMessageEvent, Relation, RoomMessageEventContent,
            RoomMessageEventContentWithoutRelation,
        },
    },
};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{OnceCell, mpsc};
use tracing::{debug, info, warn};

pub struct MatrixChannel {
    attachment_store: Option<Arc<AttachmentStore>>,
    homeserver: String,
    access_token: String,
    user_id: String,
    device_id: String,
    room_ids: HashSet<String>,
    allowed_users: HashSet<String>,
    recovery_key: Option<String>,
    state_dir: PathBuf,
    client: Arc<OnceCell<Client>>,
}

impl std::fmt::Debug for MatrixChannel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MatrixChannel")
            .field("homeserver", &self.homeserver)
            .field("user_id", &self.user_id)
            .field("room_ids", &self.room_ids)
            .finish_non_exhaustive()
    }
}

impl MatrixChannel {
    pub fn new(cfg: &MatrixConfig) -> Self {
        Self {
            attachment_store: None,
            homeserver: cfg.homeserver.clone(),
            access_token: cfg.access_token.clone(),
            user_id: cfg.user_id.clone(),
            device_id: cfg.device_id.clone(),
            room_ids: cfg.room_ids.iter().cloned().collect(),
            allowed_users: cfg.allowed_users.iter().cloned().collect(),
            recovery_key: cfg.recovery_key.clone(),
            state_dir: cfg.resolved_state_dir(),
            client: Arc::new(OnceCell::new()),
        }
    }

    pub fn with_attachment_store(mut self, store: Option<Arc<AttachmentStore>>) -> Self {
        self.attachment_store = store;
        self
    }

    async fn get_or_init_client(&self) -> Result<&Client> {
        self.client
            .get_or_try_init(|| async { self.build_client().await })
            .await
    }

    async fn build_client(&self) -> Result<Client> {
        std::fs::create_dir_all(&self.state_dir)
            .with_context(|| format!("Failed to create state dir: {}", self.state_dir.display()))?;

        let client = Client::builder()
            .homeserver_url(&self.homeserver)
            .sqlite_store(&self.state_dir, None)
            .build()
            .await
            .context("Failed to build Matrix client")?;

        let user_id: OwnedUserId = self.user_id.parse().context("Invalid user_id in config")?;

        let session = MatrixSession {
            meta: SessionMeta {
                user_id,
                device_id: self.device_id.as_str().into(),
            },
            tokens: SessionTokens {
                access_token: self.access_token.clone(),
                refresh_token: None,
            },
        };
        client
            .restore_session(session)
            .await
            .context("Failed to restore Matrix session")?;

        if let Some(key) = &self.recovery_key {
            if let Err(e) = client.encryption().recovery().recover(key).await {
                warn!("E2EE recovery failed (key may already be active): {e}");
            } else {
                info!("E2EE recovery key applied successfully");
            }
        }

        info!(
            "Matrix client initialized for {} on {}",
            self.user_id, self.homeserver
        );
        Ok(client)
    }

    async fn get_room(&self, room_id_str: &str) -> Result<matrix_sdk::Room> {
        let client = self.get_or_init_client().await?;
        let room_id: OwnedRoomId = room_id_str.parse().context("Invalid room_id")?;
        client.get_room(&room_id).context("Room not found")
    }
}

/// The transport source is transient and never passed to the attachment store.
struct MatrixMedia {
    filename: String,
    caption: String,
    source: MediaSource,
    mimetype: Option<String>,
    size: Option<u64>,
}

fn matrix_media(msgtype: &MessageType) -> Option<MatrixMedia> {
    macro_rules! extract {
        ($media:expr) => {{
            let media = $media;
            MatrixMedia {
                filename: media.filename().to_owned(),
                caption: media.caption().unwrap_or_default().to_owned(),
                source: media.source.clone(),
                mimetype: media.info.as_ref().and_then(|info| info.mimetype.clone()),
                size: media
                    .info
                    .as_ref()
                    .and_then(|info| info.size)
                    .map(u64::from),
            }
        }};
    }
    Some(match msgtype {
        MessageType::Image(media) => extract!(media),
        MessageType::File(media) => extract!(media),
        MessageType::Audio(media) => extract!(media),
        MessageType::Video(media) => extract!(media),
        _ => return None,
    })
}

fn check_matrix_media_size(size: u64) -> Result<(), String> {
    if size > MAX_DOWNLOAD_BYTES as u64 {
        return Err("over the 32 MiB download limit".into());
    }
    Ok(())
}

async fn download_matrix_media(
    client: &Client,
    source: MediaSource,
    size: Option<u64>,
) -> Result<Vec<u8>, String> {
    if let Some(size) = size {
        check_matrix_media_size(size)?;
    }
    let request = MediaRequestParameters {
        source,
        format: MediaFormat::File,
    };
    // One event has one attachment, so the 32 MiB per-file bound also meets
    // the 64 MiB message bound. The SDK buffers the entire decrypted Vec:
    // these checks do NOT impose a streaming memory limit inside the SDK.
    let bytes = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        client.media().get_media_content(&request, false),
    )
    .await
    .map_err(|_| "downloading it timed out".to_string())?
    .map_err(|e| format!("downloading it failed ({e})"))?;
    check_matrix_media_size(bytes.len() as u64)?;
    Ok(bytes)
}

#[async_trait]
impl Channel for MatrixChannel {
    fn name(&self) -> &str {
        "matrix"
    }

    async fn send(&self, message: &OutgoingMessage) -> Result<Vec<String>> {
        let room = self.get_room(&message.room_id).await?;

        let mut content = RoomMessageEventContent::text_markdown(&message.content);

        if let Some(thread_id) = &message.thread_id {
            let thread_root: OwnedEventId = thread_id.parse().context("Invalid thread_id")?;
            content.relates_to = Some(Relation::Thread(Thread::plain(
                thread_root.clone(),
                thread_root,
            )));
        }

        // A reply wins over a thread relation.
        if let Some(reply_to) = &message.reply_to {
            let target: OwnedEventId = reply_to.parse().context("Invalid reply_to")?;
            content.relates_to = Some(Relation::Reply(Reply::new(InReplyTo::new(target))));
        }

        let response = room.send(content).await.context("Failed to send message")?;
        Ok(vec![response.response.event_id.to_string()])
    }

    fn supports_replies(&self) -> bool {
        true
    }

    async fn listen(&self, tx: mpsc::Sender<IncomingMessage>) -> Result<()> {
        let client = self.get_or_init_client().await?;
        let allowed_rooms = self.room_ids.clone();
        let allowed_users = self.allowed_users.clone();
        let bot_user_id = self.user_id.clone();
        let attachment_store = self.attachment_store.clone();

        client.add_event_handler({
            let tx = tx.clone();
            move |event: OriginalSyncRoomMessageEvent, room: matrix_sdk::Room| {
                let tx = tx.clone();
                let allowed_rooms = allowed_rooms.clone();
                let allowed_users = allowed_users.clone();
                let bot_user_id = bot_user_id.clone();
                let attachment_store = attachment_store.clone();

                async move {
                    let room_id_str = room.room_id().as_str().to_string();
                    if !allowed_rooms.contains(&room_id_str) {
                        return;
                    }
                    if event.sender.as_str() == bot_user_id {
                        return;
                    }
                    if !allowed_users.is_empty() && !allowed_users.contains(event.sender.as_str()) {
                        debug!("Ignoring message from non-allowed user: {}", event.sender);
                        return;
                    }

                    // Resolve thread context early — needed both for routing and
                    // for attaching a thread relation to any error replies.
                    let thread_id = event.content.relates_to.as_ref().and_then(|rel| {
                        if let Relation::Thread(thread) = rel {
                            Some(thread.event_id.to_string())
                        } else {
                            None
                        }
                    });

                    let reply_to = reply_target(event.content.relates_to.as_ref());

                    let (content, attachments) = match &event.content.msgtype {
                        MessageType::Text(text_content) => {
                            let body = if reply_to.is_some() {
                                strip_reply_fallback(&text_content.body)
                            } else {
                                text_content.body.clone()
                            };
                            (body, Vec::new())
                        }
                        media_type => {
                            let Some(media) = matrix_media(media_type) else {
                                return;
                            };
                            let (inline, notes) = match download_matrix_media(
                                &room.client(),
                                media.source,
                                media.size,
                            )
                            .await
                            {
                                Ok(bytes) => {
                                    cache_and_prepare_attachment(
                                        attachment_store,
                                        AttachmentOwner {
                                            platform: "matrix".into(),
                                            room_id: room_id_str.clone(),
                                            thread_id: thread_id.clone(),
                                        },
                                        event.event_id.to_string(),
                                        media.filename.clone(),
                                        media.mimetype,
                                        bytes,
                                    )
                                    .await
                                }
                                Err(reason) => (
                                    None,
                                    vec![dropped_attachment_note(&media.filename, &reason)],
                                ),
                            };
                            (
                                with_attachment_notes(media.caption, &notes),
                                inline.into_iter().collect(),
                            )
                        }
                    };

                    let msg = IncomingMessage {
                        id: event.event_id.to_string(),
                        sender: event.sender.to_string(),
                        content,
                        room_id: room_id_str,
                        timestamp: 0,
                        thread_id,
                        attachments,
                        reply_to,
                    };

                    if let Err(e) = tx.send(msg).await {
                        warn!("Failed to forward Matrix message: {e}");
                    }
                }
            }
        });

        info!("Starting Matrix sync loop...");
        let sync_settings = SyncSettings::default().timeout(std::time::Duration::from_secs(30));
        let min_backoff = std::time::Duration::from_secs(1);
        let max_backoff = std::time::Duration::from_secs(300);
        let stable_threshold = std::time::Duration::from_secs(60);
        let mut backoff = min_backoff;
        loop {
            let started = std::time::Instant::now();
            match client.sync(sync_settings.clone()).await {
                Ok(()) => {
                    warn!("Matrix sync loop exited without error; reconnecting in {backoff:?}");
                }
                Err(e) => {
                    warn!("Matrix sync loop exited with error: {e}; reconnecting in {backoff:?}");
                }
            }
            tokio::time::sleep(backoff).await;
            if started.elapsed() >= stable_threshold {
                backoff = min_backoff;
            } else {
                backoff = (backoff * 2).min(max_backoff);
            }
            info!("Reconnecting Matrix sync loop...");
        }
    }

    async fn start_typing(&self, room_id: &str) -> Result<()> {
        let room = self.get_room(room_id).await?;
        room.typing_notice(true)
            .await
            .context("Failed to send typing notice")?;
        Ok(())
    }

    async fn stop_typing(&self, room_id: &str) -> Result<()> {
        let room = self.get_room(room_id).await?;
        room.typing_notice(false)
            .await
            .context("Failed to stop typing notice")?;
        Ok(())
    }

    async fn room_info(&self, room_id: &str) -> Option<RoomInfo> {
        let room = self.get_room(room_id).await.ok()?;
        let name = room.name().unwrap_or_else(|| room_id.to_string());
        let description = room.topic();
        Some(RoomInfo {
            name,
            description,
            kind: "matrix".to_string(),
        })
    }
}

/// The event ID a message replies to, if it is a reply. A thread message
/// counts only when its `in_reply_to` is a real reply rather than the
/// fallback pointer clients add for threads-unaware readers.
pub fn reply_target(
    rel: Option<&Relation<RoomMessageEventContentWithoutRelation>>,
) -> Option<String> {
    match rel? {
        Relation::Reply(reply) => Some(reply.in_reply_to.event_id.to_string()),
        // A reply inside a thread names its target; otherwise ("Reply in
        // thread" on a message, or a fallback reply) it answers the root.
        Relation::Thread(thread) => match &thread.in_reply_to {
            Some(r) if !thread.is_falling_back => Some(r.event_id.to_string()),
            _ => Some(thread.event_id.to_string()),
        },
        _ => None,
    }
}

/// Drop the `> ` quoted fallback block that older clients prepend to replies.
pub fn strip_reply_fallback(body: &str) -> String {
    if !body.starts_with('>') {
        return body.to_string();
    }
    let rest = body
        .split_inclusive('\n')
        .skip_while(|l| l.starts_with('>'))
        .collect::<String>();
    rest.trim_start_matches(['\n', '\r']).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use matrix_sdk::ruma::EventId;

    #[test]
    fn extracts_all_media_types_and_caption_fallbacks() {
        for msgtype in ["m.image", "m.file", "m.audio", "m.video"] {
            let mut value = serde_json::json!({
                "msgtype": msgtype, "body": "look at this", "filename": "original.bin",
                "url": "mxc://example.org/source-sentinel",
                "info": {"mimetype": "application/octet-stream", "size": 123}
            });
            let content: RoomMessageEventContent = serde_json::from_value(value.clone()).unwrap();
            let media = matrix_media(&content.msgtype).unwrap();
            assert_eq!(media.filename, "original.bin");
            assert_eq!(media.caption, "look at this");
            assert_eq!(media.mimetype.as_deref(), Some("application/octet-stream"));
            assert_eq!(media.size, Some(123));
            assert!(
                matches!(media.source, MediaSource::Plain(ref uri) if uri.as_str() == "mxc://example.org/source-sentinel")
            );
            value.as_object_mut().unwrap().remove("filename");
            value.as_object_mut().unwrap().remove("info");
            let content: RoomMessageEventContent = serde_json::from_value(value).unwrap();
            let media = matrix_media(&content.msgtype).unwrap();
            assert_eq!(media.filename, "look at this");
            assert!(media.caption.is_empty());
            assert!(media.size.is_none());
            assert!(media.mimetype.is_none());
        }
        assert!(matrix_media(&MessageType::text_plain("text")).is_none());
    }

    #[test]
    fn size_check_never_truncates_u64() {
        assert!(check_matrix_media_size(MAX_DOWNLOAD_BYTES as u64).is_ok());
        for size in [MAX_DOWNLOAD_BYTES as u64 + 1, u32::MAX as u64 + 1, u64::MAX] {
            assert!(check_matrix_media_size(size).is_err());
        }
    }

    #[tokio::test]
    async fn encrypted_source_keys_never_reach_persisted_metadata() {
        let key = "bWF0cml4LWtleS1zZW50aW5lbCEhISEhISEhISEhISE";
        let content: RoomMessageEventContent = serde_json::from_value(serde_json::json!({
            "msgtype": "m.file", "body": "caption", "filename": "original.bin",
            "file": {
                "url": "mxc://example.org/source-sentinel", "v": "v2",
                "key": {"kty": "oct", "key_ops": ["encrypt", "decrypt"], "alg": "A256CTR", "k": key, "ext": true},
                "iv": "AAAAAAAAAAAAAAAAAAAAAA", "hashes": {"sha256": "aGFzaC1zZW50aW5lbCEhISEhISEhISEhISEhISEhISE"}
            }
        })).unwrap();
        let media = matrix_media(&content.msgtype).unwrap();
        assert!(matches!(media.source, MediaSource::Encrypted(_)));
        let dir = tempfile::tempdir().unwrap();
        let store = AttachmentStore::open(dir.path().to_owned()).unwrap();
        let (_, notes) = cache_and_prepare_attachment(
            Some(store),
            AttachmentOwner {
                platform: "matrix".into(),
                room_id: "!room:example.org".into(),
                thread_id: Some("$thread:example.org".into()),
            },
            "$event:example.org".into(),
            media.filename,
            media.mimetype,
            b"original".to_vec(),
        )
        .await;
        assert!(notes[0].contains("Use attachment_save"));
        for entry in std::fs::read_dir(dir.path()).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_some_and(|e| e == "json") {
                let json = std::fs::read_to_string(path).unwrap();
                for secret in [
                    key,
                    "matrix-key-sentinel",
                    "source-sentinel",
                    "aGFzaC1zZW50aW5lbCEhISEhISEhISEhISEhISEhISE",
                    "AAAAAAAAAAAAAAAAAAAAAA",
                ] {
                    assert!(!json.contains(secret));
                }
                let meta: crate::attachment_store::StoredAttachment =
                    serde_json::from_str(&json).unwrap();
                assert_eq!(meta.owner.platform, "matrix");
                assert_eq!(meta.owner.thread_id.as_deref(), Some("$thread:example.org"));
            }
        }
    }

    #[test]
    fn builder_accepts_optional_store() {
        let cfg: crate::config::MatrixConfig = toml::from_str(
            r#"
            homeserver = "http://localhost"
            access_token = "unused"
            user_id = "@bot:example.org"
            device_id = "unused"
            room_ids = []
        "#,
        )
        .unwrap();
        assert!(
            MatrixChannel::new(&cfg)
                .with_attachment_store(None)
                .attachment_store
                .is_none()
        );
    }

    fn eid(s: &str) -> OwnedEventId {
        EventId::parse(s).unwrap()
    }

    fn thread(fallback: bool) -> Relation<RoomMessageEventContentWithoutRelation> {
        let mut t = Thread::plain(eid("$root:x"), eid("$e2:x"));
        t.in_reply_to = Some(InReplyTo::new(eid("$e2:x")));
        t.is_falling_back = fallback;
        Relation::Thread(t)
    }

    #[test]
    fn reply_relation_is_a_reply() {
        let rel = Relation::Reply(Reply::new(InReplyTo::new(eid("$e1:x"))));
        assert_eq!(reply_target(Some(&rel)), Some("$e1:x".to_string()));
    }

    #[test]
    fn thread_reply_is_a_reply() {
        assert_eq!(
            reply_target(Some(&thread(false))),
            Some("$e2:x".to_string())
        );
    }

    #[test]
    fn thread_fallback_targets_the_root() {
        // Element's "Reply in thread" on the question: the question is the
        // thread root and the reply relation is only a fallback.
        assert_eq!(
            reply_target(Some(&thread(true))),
            Some("$root:x".to_string())
        );
        let mut bare = Thread::plain(eid("$root:x"), eid("$root:x"));
        bare.in_reply_to = None;
        assert_eq!(
            reply_target(Some(&Relation::Thread(bare))),
            Some("$root:x".to_string())
        );
    }

    #[test]
    fn no_relation_is_not_a_reply() {
        assert_eq!(reply_target(None), None);
    }

    #[test]
    fn strips_reply_fallback() {
        assert_eq!(
            strip_reply_fallback("> <@a:x> q\n> more\n\nanswer"),
            "answer"
        );
        assert_eq!(strip_reply_fallback("answer"), "answer");
        assert_eq!(strip_reply_fallback("> quoted only"), "");
        assert_eq!(
            strip_reply_fallback("line\n> not leading"),
            "line\n> not leading"
        );
    }
}
