use crate::channel::{
    Attachment, Channel, IncomingMessage, MAX_ATTACHMENT_BYTES, OutgoingMessage, RoomInfo,
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
        events::relation::Thread,
        events::room::message::{
            ImageMessageEventContent, MessageType, OriginalSyncRoomMessageEvent, Relation,
            RoomMessageEventContent,
        },
    },
};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{OnceCell, mpsc};
use tracing::{debug, info, warn};

pub struct MatrixChannel {
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

/// Result of attempting to download a Matrix image attachment.
enum ImageDownload {
    /// Successfully downloaded and format is supported.
    Ok(Attachment),
    /// Format could not be identified as one the Anthropic API supports.
    /// The user should be notified so they can resend in a supported format.
    UnsupportedFormat,
    /// Silently skipped (too large, download failure, etc.).
    Skipped,
}

/// Download the bytes for a Matrix `m.image` event via the SDK's authenticated
/// media endpoint.
async fn download_matrix_image(client: &Client, image: &ImageMessageEventContent) -> ImageDownload {
    if let Some(size) = image.info.as_ref().and_then(|info| info.size) {
        let size: u64 = size.into();
        if size as usize > MAX_ATTACHMENT_BYTES {
            warn!(
                "Matrix image '{}' is {} bytes (>5MB); skipping",
                image.body, size
            );
            return ImageDownload::Skipped;
        }
    }

    let request = MediaRequestParameters {
        source: image.source.clone(),
        format: MediaFormat::File,
    };

    match client.media().get_media_content(&request, true).await {
        Ok(bytes) if bytes.len() <= MAX_ATTACHMENT_BYTES => {
            // Always derive the MIME from the actual bytes. Anthropic rejects
            // requests whose declared `media_type` doesn't match the bytes
            // (400 invalid_request_error), so the declared MIME is only used
            // for diagnostic warnings on mismatch.
            let declared = image
                .info
                .as_ref()
                .and_then(|info| info.mimetype.as_deref());
            let media_type = match super::sniff_image_mime(&bytes) {
                Some(t) => {
                    if let Some(d) = declared
                        && d != t
                    {
                        warn!(
                            "Matrix image '{}' declared MIME '{}' differs from \
                             detected '{}'; using detected",
                            image.body, d, t
                        );
                    }
                    t.to_string()
                }
                None => {
                    warn!(
                        "Matrix image '{}' has unrecognised format (declared: {:?})",
                        image.body, declared
                    );
                    return ImageDownload::UnsupportedFormat;
                }
            };
            ImageDownload::Ok(Attachment {
                media_type,
                data: bytes,
            })
        }
        Ok(bytes) => {
            warn!(
                "Matrix image '{}' decoded to {} bytes (>5MB); skipping",
                image.body,
                bytes.len()
            );
            ImageDownload::Skipped
        }
        Err(e) => {
            warn!("Failed to download Matrix image '{}': {e}", image.body);
            ImageDownload::Skipped
        }
    }
}

#[async_trait]
impl Channel for MatrixChannel {
    fn name(&self) -> &str {
        "matrix"
    }

    async fn send(&self, message: &OutgoingMessage) -> Result<()> {
        let room = self.get_room(&message.room_id).await?;

        let mut content = RoomMessageEventContent::text_markdown(&message.content);

        if let Some(thread_id) = &message.thread_id {
            let thread_root: OwnedEventId = thread_id.parse().context("Invalid thread_id")?;
            content.relates_to = Some(Relation::Thread(Thread::plain(
                thread_root.clone(),
                thread_root,
            )));
        }

        room.send(content).await.context("Failed to send message")?;
        Ok(())
    }

    async fn listen(&self, tx: mpsc::Sender<IncomingMessage>) -> Result<()> {
        let client = self.get_or_init_client().await?;
        let allowed_rooms = self.room_ids.clone();
        let allowed_users = self.allowed_users.clone();
        let bot_user_id = self.user_id.clone();

        client.add_event_handler({
            let tx = tx.clone();
            move |event: OriginalSyncRoomMessageEvent, room: matrix_sdk::Room| {
                let tx = tx.clone();
                let allowed_rooms = allowed_rooms.clone();
                let allowed_users = allowed_users.clone();
                let bot_user_id = bot_user_id.clone();

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

                    let (content, attachments) = match &event.content.msgtype {
                        MessageType::Text(text_content) => (text_content.body.clone(), Vec::new()),
                        MessageType::Image(image_content) => {
                            let result =
                                download_matrix_image(&room.client(), image_content).await;
                            // Use the body as a caption fallback (filename or user-supplied text).
                            let caption = image_content
                                .caption()
                                .map(str::to_string)
                                .unwrap_or_default();
                            match result {
                                ImageDownload::Ok(att) => (caption, vec![att]),
                                ImageDownload::UnsupportedFormat => {
                                    // Forward a synthetic prompt to the agent so it generates
                                    // a natural-language reply asking the user to resend in a
                                    // supported format (JPEG / PNG / GIF / WebP).
                                    let msg = IncomingMessage {
                                        id: event.event_id.to_string(),
                                        sender: event.sender.to_string(),
                                        content: "[system] An image was received in an unsupported format. \
                                                  Please inform the user and ask them to resend it \
                                                  as JPEG, PNG, GIF, or WebP."
                                            .to_string(),
                                        room_id: room_id_str,
                                        timestamp: 0,
                                        thread_id,
                                        attachments: Vec::new(),
                                    };
                                    if let Err(e) = tx.send(msg).await {
                                        warn!("Failed to forward unsupported-format event: {e}");
                                    }
                                    return;
                                }
                                ImageDownload::Skipped => return,
                            }
                        }
                        _ => return,
                    };

                    let msg = IncomingMessage {
                        id: event.event_id.to_string(),
                        sender: event.sender.to_string(),
                        content,
                        room_id: room_id_str,
                        timestamp: 0,
                        thread_id,
                        attachments,
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
