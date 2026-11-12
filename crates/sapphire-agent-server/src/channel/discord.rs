use crate::attachment_store::{AttachmentOwner, AttachmentStore};
use crate::channel::{
    Attachment, Channel, IncomingMessage, MAX_DOWNLOAD_BYTES, MAX_MESSAGE_ATTACHMENT_BYTES,
    MAX_RECEIVED_ATTACHMENTS, OutgoingMessage, RoomInfo, cache_and_prepare_attachment,
    dropped_attachment_note, format_mb, with_attachment_notes,
};
use crate::config::DiscordConfig;
use anyhow::{Context, Result};
use async_trait::async_trait;
use serenity::Client;
use serenity::all::{
    ChannelId, Context as SerenityCtx, CreateMessage, EventHandler, GatewayIntents, Message,
    MessageId, Ready,
};
use serenity::http::Http;
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::{OnceCell, mpsc};
use tracing::{debug, info, warn};

// ---------------------------------------------------------------------------
// Internal event handler
// ---------------------------------------------------------------------------

struct DiscordHandler {
    tx: mpsc::Sender<IncomingMessage>,
    allowed_channel_ids: HashSet<u64>,
    allowed_user_ids: HashSet<u64>,
    attachment_store: Option<Arc<AttachmentStore>>,
    download_client: reqwest::Client,
}

#[serenity::async_trait]
impl EventHandler for DiscordHandler {
    async fn ready(&self, _ctx: SerenityCtx, ready: Ready) {
        info!("Discord bot connected as {}", ready.user.name);
    }

    async fn message(&self, _ctx: SerenityCtx, msg: Message) {
        // Ignore bots (includes self)
        if msg.author.bot {
            return;
        }

        let channel_id = msg.channel_id.get();
        if !self.allowed_channel_ids.is_empty() && !self.allowed_channel_ids.contains(&channel_id) {
            debug!("Ignoring message from channel {channel_id} (not in allowed list)");
            return;
        }

        let user_id = msg.author.id.get();
        if !self.allowed_user_ids.is_empty() && !self.allowed_user_ids.contains(&user_id) {
            debug!("Ignoring message from user {user_id} (not in allowed list)");
            return;
        }

        let (attachments, notes) =
            download_attachments(&msg, &self.download_client, self.attachment_store.clone()).await;
        let content = with_attachment_notes(msg.content.trim().to_string(), &notes);

        // Skip messages with neither text nor any attachment, usable or not.
        if content.is_empty() && attachments.is_empty() {
            return;
        }

        let incoming = IncomingMessage {
            id: msg.id.to_string(),
            sender: msg.author.id.to_string(),
            content,
            room_id: msg.channel_id.to_string(),
            timestamp: msg.timestamp.unix_timestamp() as u64 * 1000,
            thread_id: None,
            attachments,
            attachment_owner: Some(AttachmentOwner {
                platform: "discord".into(),
                room_id: msg.channel_id.to_string(),
                thread_id: None,
            }),
            reply_to: reply_target(&msg),
        };

        if let Err(e) = self.tx.send(incoming).await {
            warn!("Failed to forward Discord message: {e}");
        }
    }
}

// ---------------------------------------------------------------------------
// DiscordChannel
// ---------------------------------------------------------------------------

pub struct DiscordChannel {
    attachment_store: Option<Arc<AttachmentStore>>,
    token: String,
    channel_ids: HashSet<u64>,
    allowed_user_ids: HashSet<u64>,
    /// Filled by `listen()` once the gateway client is built; used by `send()`.
    http: Arc<OnceCell<Arc<Http>>>,
}

impl std::fmt::Debug for DiscordChannel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiscordChannel")
            .field("channel_ids", &self.channel_ids)
            .finish_non_exhaustive()
    }
}

impl DiscordChannel {
    pub fn new(cfg: &DiscordConfig) -> Result<Self> {
        let channel_ids = cfg
            .channel_ids
            .iter()
            .map(|s| s.parse::<u64>().context("Invalid Discord channel_id"))
            .collect::<Result<HashSet<_>>>()?;

        let allowed_user_ids = cfg
            .allowed_users
            .iter()
            .map(|s| {
                s.parse::<u64>()
                    .context("Invalid Discord user ID in allowed_users")
            })
            .collect::<Result<HashSet<_>>>()?;

        Ok(Self {
            attachment_store: None,
            token: cfg.bot_token.clone(),
            channel_ids,
            allowed_user_ids,
            http: Arc::new(OnceCell::new()),
        })
    }

    pub fn with_attachment_store(mut self, store: Option<Arc<AttachmentStore>>) -> Self {
        self.attachment_store = store;
        self
    }

    fn get_http_or_new(&self) -> Arc<Http> {
        if let Some(http) = self.http.get() {
            Arc::clone(http)
        } else {
            // listen() hasn't started yet; create a standalone HTTP client.
            Arc::new(Http::new(&self.token))
        }
    }
}

// ---------------------------------------------------------------------------
// Channel impl
// ---------------------------------------------------------------------------

#[async_trait]
impl Channel for DiscordChannel {
    fn name(&self) -> &str {
        "discord"
    }

    fn supports_replies(&self) -> bool {
        true
    }

    async fn send(&self, message: &OutgoingMessage) -> Result<Vec<String>> {
        let http = self.get_http_or_new();
        let channel_id: u64 = message
            .room_id
            .parse()
            .context("Discord room_id is not a valid channel ID")?;
        let channel_id = ChannelId::new(channel_id);

        let reply_to = match &message.reply_to {
            Some(id) => Some(MessageId::new(
                id.parse::<u64>()
                    .context("Discord reply_to is not a message ID")?,
            )),
            None => None,
        };

        let mut ids = Vec::new();
        for (i, chunk) in split_for_discord(&message.content).into_iter().enumerate() {
            let mut builder = CreateMessage::new().content(chunk);
            if i == 0
                && let Some(target) = reply_to
            {
                builder = builder.reference_message((channel_id, target));
            }
            let sent = channel_id
                .send_message(http.as_ref(), builder)
                .await
                .context("Failed to send Discord message")?;
            ids.push(sent.id.to_string());
        }
        Ok(ids)
    }

    /// Long-running: connects to the Discord gateway and forwards incoming
    /// messages through `tx`. Reconnects with exponential backoff on disconnect.
    async fn listen(&self, tx: mpsc::Sender<IncomingMessage>) -> Result<()> {
        // MESSAGE_CONTENT is a privileged intent — enable it in Discord Developer Portal
        // (Bot → Privileged Gateway Intents → Message Content Intent).
        let intents = GatewayIntents::GUILD_MESSAGES
            | GatewayIntents::DIRECT_MESSAGES
            | GatewayIntents::MESSAGE_CONTENT;

        let min_backoff = std::time::Duration::from_secs(1);
        let max_backoff = std::time::Duration::from_secs(300);
        let stable_threshold = std::time::Duration::from_secs(60);
        let mut backoff = min_backoff;

        let download_client = reqwest::Client::new();
        loop {
            let handler = DiscordHandler {
                tx: tx.clone(),
                attachment_store: self.attachment_store.clone(),
                download_client: download_client.clone(),
                allowed_channel_ids: self.channel_ids.clone(),
                allowed_user_ids: self.allowed_user_ids.clone(),
            };

            let mut client = Client::builder(&self.token, intents)
                .event_handler(handler)
                .await
                .context("Failed to build Discord client")?;

            // Share the HTTP client so send() can use it immediately.
            let _ = self.http.set(Arc::clone(&client.http));

            info!("Starting Discord gateway...");
            let started = std::time::Instant::now();
            match client.start().await {
                Ok(()) => {
                    warn!("Discord gateway exited without error; reconnecting in {backoff:?}");
                }
                Err(e) => {
                    warn!("Discord gateway exited with error: {e}; reconnecting in {backoff:?}");
                }
            }
            tokio::time::sleep(backoff).await;
            if started.elapsed() >= stable_threshold {
                backoff = min_backoff;
            } else {
                backoff = (backoff * 2).min(max_backoff);
            }
            info!("Reconnecting Discord gateway...");
        }
    }

    async fn start_typing(&self, room_id: &str) -> Result<()> {
        let http = self.get_http_or_new();
        let channel_id: u64 = room_id.parse().context("Invalid Discord channel ID")?;
        // broadcast_typing lasts ~10 seconds and auto-expires; no explicit stop needed.
        ChannelId::new(channel_id)
            .broadcast_typing(http.as_ref())
            .await
            .context("Failed to send typing indicator")?;
        Ok(())
    }

    // stop_typing is a no-op: Discord typing expires automatically.

    async fn room_info(&self, room_id: &str) -> Option<RoomInfo> {
        use serenity::all::{Channel as SerenityChannel, ChannelType};
        let channel_id: u64 = room_id.parse().ok()?;
        let http = self.get_http_or_new();
        let channel = ChannelId::new(channel_id)
            .to_channel(http.as_ref())
            .await
            .ok()?;
        match channel {
            SerenityChannel::Guild(gc) => {
                let kind = match gc.kind {
                    ChannelType::Voice | ChannelType::Stage => "discord-voice",
                    _ => "discord",
                };
                Some(RoomInfo {
                    name: gc.name.clone(),
                    description: gc.topic.clone().filter(|t| !t.is_empty()),
                    kind: kind.to_string(),
                })
            }
            SerenityChannel::Private(pc) => Some(RoomInfo {
                name: format!("DM with {}", pc.recipient.name),
                description: None,
                kind: "discord-dm".to_string(),
            }),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Fetch only the event's attachment URLs, in order. Failed entries still
/// consume one of the first eight slots, but not the successful-byte budget.
async fn download_attachments(
    msg: &Message,
    client: &reqwest::Client,
    store: Option<Arc<AttachmentStore>>,
) -> (Vec<Attachment>, Vec<String>) {
    let mut out = Vec::new();
    let mut notes = Vec::new();
    let mut total = 0usize;
    for (index, att) in msg.attachments.iter().enumerate() {
        let dropped = |reason: String| dropped_attachment_note(&att.filename, &reason);
        if index >= MAX_RECEIVED_ATTACHMENTS {
            notes.push(dropped("over the 8 attachments per message limit".into()));
            continue;
        }
        if let Err(reason) = screen_attachment(att.content_type.as_deref(), att.size as usize) {
            notes.push(dropped(reason));
            continue;
        }
        let remaining = MAX_MESSAGE_ATTACHMENT_BYTES - total;
        if att.size as usize > remaining {
            notes.push(dropped("over the remaining message download limit".into()));
            continue;
        }
        let bytes = match download_attachment_bytes(
            client,
            &att.url,
            remaining,
            std::time::Duration::from_secs(60),
        )
        .await
        {
            Ok(bytes) => bytes,
            Err(reason) => {
                notes.push(dropped(reason));
                continue;
            }
        };
        // The downloader already checked both limits with checked_add.
        total = total
            .checked_add(bytes.len())
            .expect("bounded message bytes");
        let owner = AttachmentOwner {
            platform: "discord".into(),
            room_id: msg.channel_id.to_string(),
            thread_id: None,
        };
        let (inline, received_notes) = cache_and_prepare_attachment(
            store.clone(),
            owner,
            msg.id.to_string(),
            att.filename.clone(),
            att.content_type.clone(),
            bytes,
        )
        .await;
        out.extend(inline);
        notes.extend(received_notes);
    }
    (out, notes)
}

/// Bound the whole request (headers and every body chunk), not just connect.
async fn download_attachment_bytes(
    client: &reqwest::Client,
    url: &str,
    remaining: usize,
    timeout: std::time::Duration,
) -> Result<Vec<u8>, String> {
    tokio::time::timeout(timeout, async {
        let mut response = client
            .get(url)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(|e| format!("downloading it failed ({e})"))?;
        let limit = remaining.min(MAX_DOWNLOAD_BYTES);
        if response
            .content_length()
            .is_some_and(|size| size > limit as u64)
        {
            return Err("over the file or remaining message download limit".into());
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|e| format!("downloading it failed ({e})"))?
        {
            let next = bytes
                .len()
                .checked_add(chunk.len())
                .filter(|&size| size <= limit)
                .ok_or_else(|| "over the file or remaining message download limit".to_string())?;
            bytes.reserve(next - bytes.len());
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
    })
    .await
    .map_err(|_| "downloading it timed out".to_string())?
}

/// MIME is deliberately not a filter: preserve unknown and non-image files.
fn screen_attachment(_content_type: Option<&str>, size: usize) -> Result<(), String> {
    if size > MAX_DOWNLOAD_BYTES {
        return Err(format!(
            "it is {}, over the {} download limit",
            format_mb(size),
            format_mb(MAX_DOWNLOAD_BYTES)
        ));
    }
    Ok(())
}

/// Split content into chunks that fit within Discord's 2000-character limit.
fn split_for_discord(content: &str) -> Vec<String> {
    const LIMIT: usize = 1990; // leave a small margin
    if content.len() <= LIMIT {
        return vec![content.to_owned()];
    }

    let mut chunks = Vec::new();
    let mut remaining = content;

    while remaining.len() > LIMIT {
        // Find a safe UTF-8 boundary at or before LIMIT.
        let mut split = LIMIT;
        while !remaining.is_char_boundary(split) {
            split -= 1;
        }
        // Prefer splitting at a newline to keep code blocks intact.
        if let Some(nl) = remaining[..split].rfind('\n') {
            split = nl + 1;
        }
        chunks.push(remaining[..split].to_owned());
        remaining = remaining[split..].trim_start_matches('\n');
    }

    if !remaining.is_empty() {
        chunks.push(remaining.to_owned());
    }

    chunks
}

/// The ID of the message `msg` replies to, if it is a reply.
pub fn reply_target(msg: &Message) -> Option<String> {
    msg.message_reference
        .as_ref()
        .and_then(|r| r.message_id)
        .map(|id| id.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn local_response(
        response: Vec<u8>,
        delay: std::time::Duration,
    ) -> (String, tokio::task::JoinHandle<()>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0; 4096];
            let _ = socket.read(&mut request).await;
            tokio::time::sleep(delay).await;
            // Rejection may close the connection before the server finishes.
            let _ = socket.write_all(&response).await;
        });
        (format!("http://{addr}/file?signature=sentinel"), task)
    }

    #[tokio::test]
    async fn downloader_checks_status_length_chunks_and_timeout() {
        use std::time::Duration;
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let cases = [
            (b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello".to_vec(), 5, None),
            (b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n".to_vec(), 5, Some("failed")),
            (b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\n\r\n".to_vec(), 5, Some("limit")),
            (b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n3\r\ndef\r\n0\r\n\r\n".to_vec(), 5, Some("limit")),
        ];
        for (response, remaining, error) in cases {
            let (url, server) = local_response(response, Duration::ZERO).await;
            let result =
                download_attachment_bytes(&client, &url, remaining, Duration::from_secs(2)).await;
            if let Some(error) = error {
                assert!(result.unwrap_err().contains(error));
            } else {
                assert_eq!(result.unwrap(), b"hello");
            }
            server.await.unwrap();
        }
        let (url, server) = local_response(Vec::new(), Duration::from_secs(1)).await;
        assert!(
            download_attachment_bytes(&client, &url, 5, Duration::from_millis(20))
                .await
                .unwrap_err()
                .contains("timed out")
        );
        server.abort();
    }

    #[tokio::test]
    async fn downloader_rejects_undeclared_oversized_body() {
        use std::time::Duration;
        let mut response = b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n".to_vec();
        response.resize(response.len() + MAX_DOWNLOAD_BYTES + 1, b'x');
        let (url, server) = local_response(response, Duration::ZERO).await;
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        assert!(
            download_attachment_bytes(
                &client,
                &url,
                MAX_MESSAGE_ATTACHMENT_BYTES,
                Duration::from_secs(5)
            )
            .await
            .unwrap_err()
            .contains("limit")
        );
        server.await.unwrap();
    }

    fn attachment(url: &str, size: usize, filename: &str) -> serenity::all::Attachment {
        serde_json::from_value(serde_json::json!({
            "id": "101", "filename": filename, "size": size,
            "url": url, "proxy_url": url, "content_type": "application/pdf"
        }))
        .unwrap()
    }

    #[tokio::test]
    async fn downloads_only_first_eight_and_preserves_source_free_metadata() {
        use std::time::Duration;
        let (url, server) = local_response(
            b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\n%PDF".to_vec(),
            Duration::ZERO,
        )
        .await;
        let mut msg = message(None);
        // Rejected declarations consume slots, and do not block a later small file.
        msg.attachments = (0..7)
            .map(|_| attachment("http://127.0.0.1:1", MAX_DOWNLOAD_BYTES + 1, "large"))
            .collect();
        msg.attachments.push(attachment(&url, 4, "report.pdf"));
        msg.attachments
            .push(attachment("http://127.0.0.1:1", 1, "ninth"));
        let dir = tempfile::tempdir().unwrap();
        let store = AttachmentStore::open(dir.path().to_owned()).unwrap();
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let (images, notes) = download_attachments(&msg, &client, Some(store)).await;
        assert!(images.is_empty());
        assert_eq!(notes.len(), 9);
        assert!(notes[7].contains("Use attachment_save"));
        assert!(notes[8].contains("8 attachments"));
        let json_path = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().path())
            .find(|p| p.extension().is_some_and(|e| e == "json"))
            .unwrap();
        let json = std::fs::read_to_string(json_path).unwrap();
        assert!(!json.contains(&url));
        assert!(!json.contains("signature=sentinel"));
        let meta: crate::attachment_store::StoredAttachment = serde_json::from_str(&json).unwrap();
        assert_eq!(
            meta.owner,
            AttachmentOwner {
                platform: "discord".into(),
                room_id: "7".into(),
                thread_id: None
            }
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn aggregate_budget_counts_successful_originals() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/file", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            for _ in 0..2 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = [0; 4096];
                socket.read(&mut request).await.unwrap();
                socket
                    .write_all(
                        format!(
                            "HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Length: {}\r\n\r\n",
                            MAX_DOWNLOAD_BYTES
                        )
                        .as_bytes(),
                    )
                    .await
                    .unwrap();
                let block = vec![b'x'; 1024 * 1024];
                for _ in 0..32 {
                    socket.write_all(&block).await.unwrap();
                }
            }
        });
        let mut msg = message(None);
        msg.attachments = vec![
            attachment(&url, MAX_DOWNLOAD_BYTES, "a"),
            attachment(&url, MAX_DOWNLOAD_BYTES, "b"),
            attachment("http://127.0.0.1:1", 1, "c"),
        ];
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let (_, notes) = download_attachments(&msg, &client, None).await;
        assert_eq!(notes.len(), 3);
        assert!(notes[2].contains("remaining message download limit"));
        server.await.unwrap();
    }

    #[test]
    fn builder_accepts_optional_store() {
        let cfg = crate::config::DiscordConfig {
            bot_token: "unused".into(),
            channel_ids: vec![],
            allowed_users: vec![],
        };
        let channel = DiscordChannel::new(&cfg)
            .unwrap()
            .with_attachment_store(None);
        assert!(channel.attachment_store.is_none());
    }

    #[test]
    fn an_image_over_the_provider_limit_is_still_downloaded() {
        // 5.3 MB: over the 5 MB the provider takes, so it is shrunk, not dropped.
        assert_eq!(screen_attachment(Some("image/png"), 5_539_085), Ok(()));
    }

    #[test]
    fn attachments_that_cannot_be_passed_on_say_why() {
        let too_big = screen_attachment(Some("image/png"), MAX_DOWNLOAD_BYTES + 1).unwrap_err();
        assert!(too_big.contains("download limit"), "{too_big}");
        assert_eq!(screen_attachment(Some("application/pdf"), 1000), Ok(()));
        assert_eq!(screen_attachment(None, 1000), Ok(()));
    }

    fn message(reference: Option<&str>) -> Message {
        let mut v = serde_json::json!({
            "id": "100",
            "channel_id": "7",
            "author": {"id": "1", "username": "u", "discriminator": "0", "avatar": null},
            "content": "hi",
            "timestamp": "2026-01-01T00:00:00.000000+00:00",
            "edited_timestamp": null,
            "tts": false,
            "mention_everyone": false,
            "mentions": [],
            "mention_roles": [],
            "attachments": [],
            "embeds": [],
            "pinned": false,
            "type": 0
        });
        if let Some(r) = reference {
            v["message_reference"] = serde_json::json!({"message_id": r, "channel_id": "7"});
        }
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn reply_target_reads_message_reference() {
        assert_eq!(reply_target(&message(Some("42"))), Some("42".to_string()));
        assert_eq!(reply_target(&message(None)), None);
    }
}
