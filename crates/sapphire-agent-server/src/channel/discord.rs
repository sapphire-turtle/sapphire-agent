use crate::channel::{
    Attachment, Channel, IncomingMessage, MAX_DOWNLOAD_BYTES, OutgoingMessage, RoomInfo,
    dropped_attachment_note, format_mb, prepare_image, with_attachment_notes,
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

        let (attachments, notes) = download_image_attachments(&msg).await;
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
            token: cfg.bot_token.clone(),
            channel_ids,
            allowed_user_ids,
            http: Arc::new(OnceCell::new()),
        })
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

        loop {
            let handler = DiscordHandler {
                tx: tx.clone(),
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

/// Download the image attachments on `msg`, alongside a note for each
/// attachment that could not be passed on (see [`dropped_attachment_note`]).
///
/// An image over the provider's limit is shrunk by [`prepare_image`] rather
/// than dropped; only one over [`MAX_DOWNLOAD_BYTES`] is turned away before
/// it is fetched. The MIME type of what is passed on comes from the bytes,
/// not from Discord's `content_type`, which is sometimes stale (a PNG
/// labelled `image/webp`, which Anthropic rejects with a 400).
async fn download_image_attachments(msg: &Message) -> (Vec<Attachment>, Vec<String>) {
    let mut out = Vec::new();
    let mut notes = Vec::new();
    for att in &msg.attachments {
        let dropped = |reason: String| {
            warn!(
                "Discord attachment '{}' not passed on: {reason}",
                att.filename
            );
            dropped_attachment_note(&att.filename, &reason)
        };
        if let Err(reason) = screen_attachment(att.content_type.as_deref(), att.size as usize) {
            notes.push(dropped(reason));
            continue;
        }
        let bytes = match att.download().await {
            Ok(bytes) => bytes,
            Err(e) => {
                notes.push(dropped(format!("downloading it failed ({e})")));
                continue;
            }
        };
        match tokio::task::spawn_blocking(move || prepare_image(bytes)).await {
            Ok(Ok(a)) => out.push(a),
            Ok(Err(reason)) => notes.push(dropped(reason)),
            Err(e) => notes.push(dropped(format!("processing it failed ({e})"))),
        }
    }
    (out, notes)
}

/// Whether an attachment is worth downloading, judged by what Discord says
/// of it: an image of a type the provider takes, no larger than
/// [`MAX_DOWNLOAD_BYTES`]. `Err` says why not.
fn screen_attachment(content_type: Option<&str>, size: usize) -> Result<(), String> {
    let only_images = "only images (JPEG, PNG, GIF, WebP) are passed on";
    match content_type {
        None => return Err(format!("its type is unknown, and {only_images}")),
        Some(ct) if !super::SUPPORTED_IMAGE_MIME.contains(&ct) => {
            return Err(format!("it is {ct}, and {only_images}"));
        }
        Some(_) => {}
    }
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

    #[test]
    fn an_image_over_the_provider_limit_is_still_downloaded() {
        // 5.3 MB: over the 5 MB the provider takes, so it is shrunk, not dropped.
        assert_eq!(screen_attachment(Some("image/png"), 5_539_085), Ok(()));
    }

    #[test]
    fn attachments_that_cannot_be_passed_on_say_why() {
        let too_big = screen_attachment(Some("image/png"), MAX_DOWNLOAD_BYTES + 1).unwrap_err();
        assert!(too_big.contains("download limit"), "{too_big}");
        let pdf = screen_attachment(Some("application/pdf"), 1000).unwrap_err();
        assert!(pdf.contains("application/pdf"), "{pdf}");
        let unknown = screen_attachment(None, 1000).unwrap_err();
        assert!(unknown.contains("unknown"), "{unknown}");
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
