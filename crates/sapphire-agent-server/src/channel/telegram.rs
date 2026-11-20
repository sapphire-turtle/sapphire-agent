//! Telegram Bot API channel.
//!
//! A forum topic's `message_thread_id` is part of its composite room id; it
//! never becomes an [`IncomingMessage::thread_id`].

use crate::attachment_store::{AttachmentOwner, AttachmentStore};
use crate::channel::{
    Attachment, Channel, IncomingMessage, MAX_DOWNLOAD_BYTES, MAX_MESSAGE_ATTACHMENT_BYTES,
    MAX_RECEIVED_ATTACHMENTS, OutgoingMessage, RoomInfo, cache_and_prepare_attachment,
    dropped_attachment_note, format_mb, with_attachment_notes,
};
use crate::config::{RoomConfig, TelegramConfig};
use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use futures_util::TryStreamExt;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};
use telers::methods::{GetUpdates, SendChatAction, SendMessage};
use telers::types::{Message, ReplyParameters, Update};
use telers::{Bot, Either};
use tokio::sync::{OnceCell, mpsc};
use tracing::{debug, info, warn};

const MIN_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(300);
const STABLE_THRESHOLD: Duration = Duration::from_secs(60);
const LONG_POLL_SECONDS: i64 = 30;
// telers' default request timeout is 60 seconds. Leave time for Telegram to
// answer a long poll rather than racing the request timeout at the same instant.
const LONG_POLL_REQUEST_TIMEOUT_SECONDS: f32 = LONG_POLL_SECONDS as f32 + 10.0;

// ---------------------------------------------------------------------------
// Internal update handler
// ---------------------------------------------------------------------------

struct TelegramHandler {
    tx: mpsc::Sender<IncomingMessage>,
    allowed_users: HashSet<i64>,
    rooms: Vec<RoomConfig>,
    store: Option<Arc<AttachmentStore>>,
}

impl TelegramHandler {
    /// Forward a routed message. A closed queue is logged without retrying
    /// the update, since this sender cannot recover.
    async fn message(&self, client: &Bot, msg: &Message) {
        let Some(incoming) = route_update(
            msg,
            &self.allowed_users,
            &self.rooms,
            self.store.clone(),
            |id, remaining| async move { download_file(client, &id, remaining).await },
        )
        .await
        else {
            return;
        };
        if let Err(e) = self.tx.send(incoming).await {
            warn!("Failed to forward Telegram message: {e}");
        }
    }
}

// ---------------------------------------------------------------------------
// TelegramChannel
// ---------------------------------------------------------------------------

pub struct TelegramChannel {
    token: String,
    rooms: Vec<RoomConfig>,
    store: Option<Arc<AttachmentStore>>,
    allowed_users: HashSet<i64>,
    /// Filled by `listen()` once the client exists; used by `send()`.
    client: Arc<OnceCell<Arc<Bot>>>,
}

impl std::fmt::Debug for TelegramChannel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TelegramChannel")
            .field("rooms", &self.rooms)
            .field("allowed_users", &self.allowed_users)
            .finish_non_exhaustive()
    }
}

impl TelegramChannel {
    pub fn new(cfg: &TelegramConfig) -> Result<Self> {
        if !telers::utils::token::validate(&cfg.bot_token) {
            bail!("Invalid Telegram bot token");
        }
        for room in &cfg.rooms {
            split_room_id(&room.room_id)?;
        }
        let allowed_users = cfg
            .allowed_users
            .iter()
            .map(|s| {
                s.parse::<i64>()
                    .context("Invalid Telegram user ID in allowed_users")
            })
            .collect::<Result<HashSet<_>>>()?;

        Ok(Self {
            token: cfg.bot_token.clone(),
            rooms: cfg.rooms.clone(),
            allowed_users,
            store: None,
            client: Arc::new(OnceCell::new()),
        })
    }

    pub fn with_attachment_store(mut self, store: Option<Arc<AttachmentStore>>) -> Self {
        self.store = store;
        self
    }

    fn get_client_or_new(&self) -> Arc<Bot> {
        self.client
            .get()
            .cloned()
            .unwrap_or_else(|| Arc::new(Bot::new(&self.token)))
    }
}

// ---------------------------------------------------------------------------
// Channel impl
// ---------------------------------------------------------------------------

#[async_trait]
impl Channel for TelegramChannel {
    fn name(&self) -> &str {
        "telegram"
    }

    fn supports_replies(&self) -> bool {
        true
    }

    async fn send(&self, message: &OutgoingMessage) -> Result<Vec<String>> {
        let requests = outbound_requests(message)?;
        let client = self.get_client_or_new();
        let mut ids = Vec::with_capacity(requests.len());
        for request in requests {
            let sent = client
                .send(request)
                .await
                .context("Failed to send Telegram message")?;
            ids.push(sent.message_id().to_string());
        }
        Ok(ids)
    }

    /// Long-running: long-poll Bot API updates and forward incoming messages
    /// through `tx`. Reconnects with exponential backoff on polling errors.
    async fn listen(&self, tx: mpsc::Sender<IncomingMessage>) -> Result<()> {
        let handler = TelegramHandler {
            tx,
            allowed_users: self.allowed_users.clone(),
            rooms: self.rooms.clone(),
            store: self.store.clone(),
        };
        let client = self
            .client
            .get_or_init(|| async { Arc::new(Bot::new(&self.token)) })
            .await;
        let mut offset = None;
        let mut backoff = MIN_BACKOFF;

        loop {
            info!("Starting Telegram long polling...");
            let started = Instant::now();
            if let Err(e) = receive_updates(client, &handler, &mut offset).await {
                warn!("Telegram polling exited with error: {e:#}; reconnecting in {backoff:?}");
            } else {
                warn!("Telegram polling exited without error; reconnecting in {backoff:?}");
            }
            tokio::time::sleep(backoff).await;
            if started.elapsed() >= STABLE_THRESHOLD {
                backoff = MIN_BACKOFF;
            } else {
                backoff = (backoff * 2).min(MAX_BACKOFF);
            }
            info!("Reconnecting Telegram long polling...");
        }
    }
    async fn start_typing(&self, room_id: &str) -> Result<()> {
        let request = typing_request(room_id)?;
        self.get_client_or_new()
            .send(request)
            .await
            .context("Failed to send Telegram typing indicator")?;
        Ok(())
    }

    // stop_typing is a no-op: Telegram typing expires automatically.

    async fn room_info(&self, room_id: &str) -> Option<RoomInfo> {
        let block = self.rooms.iter().find(|r| r.room_id == room_id);
        let description = block.and_then(|block| block.description.clone());
        let name = block
            .and_then(|block| block.name.clone())
            .or_else(|| description.clone())
            .unwrap_or_else(|| room_id.to_owned());
        Some(RoomInfo {
            name,
            description,
            kind: "telegram".to_owned(),
        })
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Split into at most 4096 Unicode characters, preferring newline boundaries.
fn split_for_telegram(content: &str) -> Vec<String> {
    const LIMIT: usize = 4096;
    let mut chunks = Vec::new();
    let mut remaining = content;
    while let Some((end, _)) = remaining.char_indices().nth(LIMIT) {
        let split = remaining[..end].rfind('\n').map_or(end, |nl| nl + 1);
        chunks.push(remaining[..split].to_owned());
        remaining = remaining[split..].trim_start_matches('\n');
    }
    if !remaining.is_empty() || chunks.is_empty() {
        chunks.push(remaining.to_owned());
    }
    chunks
}

/// Prepare each request before sending, validating IDs before any network call.
fn outbound_requests(message: &OutgoingMessage) -> Result<Vec<SendMessage>> {
    let (chat, thread) = split_room_id(&message.room_id)?;
    let reply = message
        .reply_to
        .as_deref()
        .map(|id| {
            id.parse::<i64>()
                .context("Telegram reply_to is not a message ID")
        })
        .transpose()?;
    Ok(split_for_telegram(&message.content)
        .into_iter()
        .enumerate()
        .map(|(index, chunk)| {
            let mut request = SendMessage::new(chat, chunk).message_thread_id_option(thread);
            if index == 0
                && let Some(reply) = reply
            {
                request = request.reply_parameters(ReplyParameters::new().message_id(reply));
            }
            request
        })
        .collect())
}

fn typing_request(room_id: &str) -> Result<SendChatAction> {
    let (chat, thread) = split_room_id(room_id)?;
    Ok(SendChatAction::new(chat, "typing").message_thread_id_option(thread))
}

async fn receive_updates(
    client: &Bot,
    handler: &TelegramHandler,
    offset: &mut Option<i64>,
) -> Result<()> {
    loop {
        let updates = client
            .send_with_timeout(
                GetUpdates::new()
                    .offset_option(*offset)
                    .timeout(LONG_POLL_SECONDS)
                    .allowed_update("message"),
                LONG_POLL_REQUEST_TIMEOUT_SECONDS,
            )
            .await
            .context("Failed to get Telegram updates")?;

        for update in updates {
            let update = match update {
                Either::Left(update) => update,
                Either::Right(unparsed) => {
                    warn!("Ignoring unparsed Telegram update {}", unparsed.update_id);
                    *offset =
                        Some(unparsed.update_id.checked_add(1).context(
                            "Telegram update id overflow while acknowledging an update",
                        )?);
                    continue;
                }
            };
            let update_id = update.update_id();
            if let Update::Message(message) = update {
                handler.message(client, &message.message).await;
            }
            // Advance even for filtered messages or a closed agent queue.
            *offset = Some(
                update_id
                    .checked_add(1)
                    .context("Telegram update id overflow while acknowledging an update")?,
            );
        }
    }
}

/// Route one Telegram message into the common inbound representation.
///
/// `rooms` is the listen list: an empty list accepts every visible room.
async fn route_update<F>(
    msg: &Message,
    allowed_users: &HashSet<i64>,
    rooms: &[RoomConfig],
    store: Option<Arc<AttachmentStore>>,
    download: impl FnMut(String, usize) -> F,
) -> Option<IncomingMessage>
where
    F: Future<Output = Result<Vec<u8>, String>> + Send,
{
    let from = msg.from()?;
    if from.is_bot {
        return None;
    }
    let sender = from.id.to_string();
    if !allowed_users.is_empty() && !allowed_users.contains(&from.id) {
        debug!("Ignoring message from user {sender} (not in allowed list)");
        return None;
    }

    let room_id = room_id(msg.chat().id(), msg.message_thread_id());
    if !rooms.is_empty() && !rooms.iter().any(|r| r.room_id == room_id) {
        debug!("Ignoring message from room {room_id} (not in allowed list)");
        return None;
    }

    let owner = AttachmentOwner {
        platform: "telegram".into(),
        room_id: room_id.clone(),
        thread_id: None,
    };
    let (attachments, notes) = download_attachments(msg, owner.clone(), store, download).await;
    let text = msg.caption().or_else(|| msg.text());
    let content = with_attachment_notes(text.unwrap_or_default().trim().to_owned(), &notes);
    if content.is_empty() && attachments.is_empty() {
        return None;
    }

    Some(IncomingMessage {
        id: msg.message_id().to_string(),
        sender,
        content,
        room_id,
        timestamp: msg.date() as u64 * 1000,
        thread_id: None,
        attachments,
        attachment_owner: Some(owner),
        reply_to: reply_target(msg),
    })
}

/// Telegram photos are alternative sizes of one image; fetch only the largest.
fn largest_photo(sizes: &[telers::types::PhotoSize]) -> Option<&telers::types::PhotoSize> {
    sizes
        .iter()
        .max_by_key(|p| p.width.saturating_mul(p.height))
}

async fn download_attachments<F>(
    msg: &Message,
    owner: AttachmentOwner,
    store: Option<Arc<AttachmentStore>>,
    mut download: impl FnMut(String, usize) -> F,
) -> (Vec<Attachment>, Vec<String>)
where
    F: Future<Output = Result<Vec<u8>, String>> + Send,
{
    let mut files = Vec::new();
    let mut notes = Vec::new();
    macro_rules! named_media {
        ($media:expr, $name:expr) => {{
            let media = $media;
            (
                media.file_id.as_ref(),
                media.file_name.as_deref().unwrap_or($name),
                media.mime_type.as_deref(),
                media.file_size,
            )
        }};
    }
    if let Some(photo) = msg.photo().and_then(largest_photo) {
        files.push((
            photo.file_id.as_ref(),
            "Telegram photo",
            None,
            photo.file_size,
        ));
    } else if let Some(doc) = msg.document() {
        files.push(named_media!(doc, "Telegram document"));
    } else if let Some(animation) = msg.animation() {
        files.push(named_media!(animation, "Telegram animation"));
    } else if let Some(video) = msg.video() {
        files.push(named_media!(video, "Telegram video"));
    } else if let Some(audio) = msg.audio() {
        files.push(named_media!(audio, "Telegram audio"));
    } else if let Some(voice) = msg.voice() {
        files.push((
            voice.file_id.as_ref(),
            "Telegram voice",
            voice.mime_type.as_deref(),
            voice.file_size,
        ));
    } else if let Some(paid) = msg.paid_media() {
        for media in paid.paid_media.iter() {
            if let Some(photo) = media.photo().and_then(largest_photo) {
                files.push((
                    photo.file_id.as_ref(),
                    "Telegram paid photo",
                    None,
                    photo.file_size,
                ));
            } else if let Some(id) = media.file_id() {
                files.push((
                    id,
                    media.file_name().unwrap_or("Telegram paid media"),
                    media.mime_type(),
                    media.file_size(),
                ));
            } else {
                notes.push(dropped_attachment_note(
                    "Telegram paid media",
                    "Telegram did not provide a downloadable file",
                ));
            }
        }
    }
    let mut out = Vec::new();
    let mut total = 0;
    for (index, (id, name, mime, size)) in files.into_iter().enumerate() {
        let dropped = |reason: &str| dropped_attachment_note(name, reason);
        if index >= MAX_RECEIVED_ATTACHMENTS {
            notes.push(dropped("over the 8 attachments per message limit"));
            continue;
        }
        let size = size
            .and_then(|s| usize::try_from(s).ok())
            .unwrap_or_default();
        if let Err(reason) = screen_attachment(size) {
            notes.push(dropped(&reason));
            continue;
        }
        let remaining = MAX_MESSAGE_ATTACHMENT_BYTES - total;
        if size > remaining {
            notes.push(dropped("over the remaining message download limit"));
            continue;
        }
        let bytes = match download(id.to_owned(), remaining).await {
            Ok(bytes) if bytes.len() <= remaining.min(MAX_DOWNLOAD_BYTES) => bytes,
            Ok(_) => {
                notes.push(dropped("over the file or remaining message download limit"));
                continue;
            }
            Err(reason) => {
                notes.push(dropped(&reason));
                continue;
            }
        };
        total += bytes.len();
        let (inline, received_notes) = cache_and_prepare_attachment(
            store.clone(),
            owner.clone(),
            msg.message_id().to_string(),
            name.to_owned(),
            mime.map(str::to_owned),
            bytes,
        )
        .await;
        out.extend(inline);
        notes.extend(received_notes);
    }
    (out, notes)
}

fn screen_attachment(size: usize) -> Result<(), String> {
    if size > MAX_DOWNLOAD_BYTES {
        return Err(format!(
            "it is {}, over the {} download limit",
            format_mb(size),
            format_mb(MAX_DOWNLOAD_BYTES)
        ));
    }
    Ok(())
}

async fn download_file(client: &Bot, id: &str, remaining: usize) -> Result<Vec<u8>, String> {
    // The library error can include the request URL, which carries the bot token.
    let mut stream = client
        .download(id)
        .await
        .map_err(|_| "downloading it from Telegram failed".to_owned())?;
    let mut bytes = Vec::new();
    // File sizes may be absent or inaccurate. Cap the actual bytes too.
    while let Some(chunk) = stream
        .try_next()
        .await
        .map_err(|_| "reading the Telegram download failed".to_owned())?
    {
        if chunk.len() > remaining.min(MAX_DOWNLOAD_BYTES) - bytes.len() {
            return Err(format!(
                "the Telegram download exceeded the {} limit",
                format_mb(remaining.min(MAX_DOWNLOAD_BYTES))
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

/// The configured room id for a chat, including its forum topic when present.
pub fn room_id(chat_id: i64, thread: Option<i64>) -> String {
    match thread {
        Some(thread) => format!("{chat_id}:{thread}"),
        None => chat_id.to_string(),
    }
}

/// Split `<chat_id>` or `<chat_id>:<message_thread_id>` into signed 64-bit ids.
pub fn split_room_id(room_id: &str) -> Result<(i64, Option<i64>)> {
    let invalid = || anyhow!("Invalid Telegram room id {room_id}");
    let (chat, thread) = match room_id.split_once(':') {
        Some((chat, thread)) if !thread.contains(':') => (chat, Some(thread)),
        Some(_) => return Err(invalid()),
        None => (room_id, None),
    };
    let chat = chat.parse::<i64>().map_err(|_| invalid())?;
    let thread = thread
        .map(|id| id.parse::<i64>().map_err(|_| invalid()))
        .transpose()?;
    Ok((chat, thread))
}

/// The id of the message `msg` replies to, if it is a reply.
pub fn reply_target(msg: &Message) -> Option<String> {
    msg.reply_to_message()
        .map(|reply| reply.message_id().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use telers::types::Message;

    async fn unexpected_download(id: String, _remaining: usize) -> Result<Vec<u8>, String> {
        panic!("unexpected file download: {id}");
    }

    const CHAT: i64 = -1_002_000_333_444_555;

    fn block(
        name: Option<&str>,
        description: Option<&str>,
        profile: Option<&str>,
    ) -> crate::config::RoomConfig {
        crate::config::RoomConfig {
            room_id: String::new(),
            voice_pipeline: None,
            name: name.map(str::to_owned),
            description: description.map(str::to_owned),
            profile: profile.unwrap_or("default").to_owned(),
        }
    }

    fn rooms(entries: &[(&str, crate::config::RoomConfig)]) -> Vec<RoomConfig> {
        entries
            .iter()
            .map(|(id, block)| RoomConfig {
                room_id: (*id).to_owned(),
                ..block.clone()
            })
            .collect()
    }

    fn fixture_message(
        chat_id: i64,
        thread_id: Option<i64>,
        from: &str,
        text: &str,
        reply_to_message_id: Option<&str>,
    ) -> Message {
        let from_id: i64 = from.parse().unwrap();
        let mut value = serde_json::json!({
            "message_id": 500,
            "message_thread_id": thread_id,
            "from": {
                "id": from_id,
                "is_bot": false,
                "first_name": "Fixture"
            },
            "date": 1_767_225_600,
            "chat": {"id": chat_id, "type": "supergroup", "title": "Fixture"},
            "text": text,
        });
        if let Some(reply_id) = reply_to_message_id {
            value["reply_to_message"] = serde_json::json!({
                "message_id": reply_id.parse::<i64>().unwrap(),
                "date": 1_767_225_599,
                "chat": {"id": chat_id, "type": "supergroup", "title": "Fixture"},
                "text": "original"
            });
        }
        let msg: Message = serde_json::from_value(value).unwrap();
        assert_eq!(msg.message_thread_id(), thread_id);
        assert_eq!(msg.chat().id(), chat_id);
        let round_trip: Message =
            serde_json::from_value(serde_json::to_value(&msg).unwrap()).unwrap();
        assert_eq!(round_trip.message_thread_id(), thread_id);
        assert_eq!(round_trip.chat().id(), chat_id);
        assert_eq!(round_trip.from().unwrap().id, from_id);
        msg
    }

    fn channel_with(rooms: Vec<RoomConfig>) -> TelegramChannel {
        TelegramChannel::new(&TelegramConfig {
            bot_token: "123456:test".to_string(),
            rooms,
            allowed_users: vec![],
        })
        .unwrap()
    }

    #[test]
    fn a_split_counts_characters_not_bytes() {
        let one = "あ".repeat(3000);
        assert_eq!(split_for_telegram(&one), vec![one.clone()]);
        assert!(one.len() > 4096, "fixture must exceed 4096 bytes");
        for len in 4091..=4096 {
            assert_eq!(split_for_telegram(&"あ".repeat(len)).len(), 1);
        }
        let over = "あ".repeat(4097);
        let chunks = split_for_telegram(&over);
        assert_eq!(
            chunks.iter().map(|s| s.chars().count()).collect::<Vec<_>>(),
            vec![4096, 1]
        );
        assert_eq!(chunks.concat(), over);
        assert_eq!(split_for_telegram(""), vec![""]);
    }

    #[test]
    fn splits_prefer_a_newline_boundary() {
        let text = format!("{}\n{}", "あ".repeat(3999), "界".repeat(1000));
        let chunks = split_for_telegram(&text);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].chars().count(), 4000);
        assert!(chunks[0].ends_with('\n'));
        assert_eq!(chunks.concat(), text);
        let text = format!("{}\n\n{}", "a".repeat(4095), "b".repeat(10));
        let chunks = split_for_telegram(&text);
        assert_eq!(chunks[0].chars().count(), 4096);
        assert_eq!(chunks[1], "b".repeat(10));
    }

    #[tokio::test]
    async fn the_block_display_fields_render_into_room_info() {
        let ch = channel_with(rooms(&[
            (
                "-1001:42",
                block(Some("開発"), Some("自律セーョョン"), None),
            ),
            ("-1001:43", block(None, Some("description"), None)),
            ("-1001:44", block(None, None, None)),
        ]));
        let info = ch.room_info("-1001:42").await.unwrap();
        assert_eq!(info.name, "開発");
        assert_eq!(info.description.as_deref(), Some("自律セーョョン"));
        assert_eq!(info.kind, "telegram");
        let info = ch.room_info("-1001:43").await.unwrap();
        assert_eq!(info.name, "description");
        assert_eq!(info.description.as_deref(), Some("description"));
        let info = ch.room_info("-1001:44").await.unwrap();
        assert_eq!(info.name, "-1001:44");
        assert_eq!(info.description, None);
    }

    #[tokio::test]
    async fn a_room_seen_without_a_block_shows_its_raw_id() {
        let ch = channel_with(Vec::new());
        let info = ch.room_info("-1001:77").await.unwrap();
        assert_eq!(info.name, "-1001:77");
        assert_eq!(info.description, None);
        assert_eq!(info.kind, "telegram");
    }

    #[test]
    fn outbound_requests_carry_topic_and_only_the_first_reply() {
        let msg = OutgoingMessage::new("あ".repeat(4097), "-1001:42").replying_to("123");
        let requests = outbound_requests(&msg).unwrap();
        assert_eq!(requests.len(), 2);
        let first = serde_json::to_value(&requests[0]).unwrap();
        let second = serde_json::to_value(&requests[1]).unwrap();
        assert_eq!(first["chat_id"], -1001);
        assert_eq!(first["message_thread_id"], 42);
        assert_eq!(first["reply_parameters"]["message_id"], 123);
        assert_eq!(first["text"].as_str().unwrap().chars().count(), 4096);
        assert_eq!(second["message_thread_id"], 42);
        assert!(second.get("reply_parameters").is_none());
        assert_eq!(second["text"], "あ");
        let plain = outbound_requests(&OutgoingMessage::new("hi", "-1001")).unwrap();
        let plain = serde_json::to_value(&plain[0]).unwrap();
        assert!(plain.get("message_thread_id").is_none());
        assert!(plain.get("reply_parameters").is_none());
        assert!(outbound_requests(&OutgoingMessage::new("hi", "bad")).is_err());
        assert!(
            outbound_requests(&OutgoingMessage::new("hi", "-1001").replying_to("bad")).is_err()
        );
    }

    #[test]
    fn typing_request_carries_topic_when_present() {
        let topic = serde_json::to_value(typing_request("-1001:42").unwrap()).unwrap();
        assert_eq!(topic["chat_id"], -1001);
        assert_eq!(topic["message_thread_id"], 42);
        assert_eq!(topic["action"], "typing");
        let plain = serde_json::to_value(typing_request("-1001").unwrap()).unwrap();
        assert!(plain.get("message_thread_id").is_none());
        assert!(typing_request("bad").is_err());
    }

    #[tokio::test]
    async fn stopping_typing_needs_no_network() {
        channel_with(Vec::new())
            .stop_typing("-1001:42")
            .await
            .unwrap();
    }

    #[test]
    fn fixture_message_thread_id_survives_a_serde_round_trip() {
        let msg = fixture_message(CHAT, Some(42), "1", "hello", Some("9"));
        assert_eq!(msg.message_thread_id(), Some(42));
        assert_eq!(msg.reply_to_message().unwrap().message_id(), 9);
    }

    #[tokio::test]
    async fn an_inbound_topic_message_is_the_composite_room() {
        let rooms = rooms(&[("-1002000333444555:42", block(None, None, None))]);
        let msg = fixture_message(CHAT, Some(42), "1", "hello", Some("9"));
        let incoming = route_update(&msg, &HashSet::new(), &rooms, None, unexpected_download)
            .await
            .unwrap();
        assert_eq!(incoming.room_id, "-1002000333444555:42");
        assert!(
            incoming.thread_id.is_none(),
            "the composite id is the room; no thread"
        );
        assert_eq!(incoming.id, "500");
        assert_eq!(incoming.reply_to.as_deref(), Some("9"));
    }

    #[tokio::test]
    async fn a_plain_chat_carries_no_colon_half() {
        let incoming = route_update(
            &fixture_message(CHAT, None, "1", "hi", None),
            &HashSet::new(),
            &[],
            None,
            unexpected_download,
        )
        .await
        .unwrap();
        assert_eq!(incoming.room_id, "-1002000333444555");
    }

    #[tokio::test]
    async fn an_unlisted_room_or_user_is_ignored() {
        let rooms = rooms(&[("-1002000333444555:42", block(None, None, None))]);
        let missed = route_update(
            &fixture_message(-100999, Some(42), "1", "hi", None),
            &HashSet::new(),
            &rooms,
            None,
            unexpected_download,
        )
        .await;
        assert!(
            missed.is_none(),
            "rooms non-empty and the room is not listed"
        );
        let empty = route_update(
            &fixture_message(CHAT, Some(42), "9", "hi", None),
            &HashSet::from([1]),
            &[],
            None,
            unexpected_download,
        )
        .await;
        assert!(empty.is_none(), "allowed_users non-empty miss");
        let accepted = route_update(
            &fixture_message(CHAT, Some(42), "1", "hi", None),
            &HashSet::from([1]),
            &[],
            None,
            unexpected_download,
        )
        .await;
        assert_eq!(accepted.unwrap().sender, "1");
    }

    #[tokio::test]
    async fn an_empty_message_with_no_attachment_is_ignored() {
        assert!(
            route_update(
                &fixture_message(CHAT, Some(42), "1", "", None),
                &HashSet::new(),
                &[],
                None,
                unexpected_download,
            )
            .await
            .is_none()
        );
    }

    fn document_message(mime: &str, size: usize, caption: Option<&str>) -> Message {
        serde_json::from_value(serde_json::json!({
            "message_id": 500,
            "from": {"id": 1, "is_bot": false, "first_name": "Fixture"},
            "date": 1_767_225_600,
            "chat": {"id": CHAT, "type": "supergroup", "title": "Fixture"},
            "caption": caption,
            "document": {
                "file_id": "file", "file_unique_id": "unique",
                "file_name": "fixture.png", "mime_type": mime, "file_size": size
            }
        }))
        .unwrap()
    }

    #[tokio::test]
    async fn a_telegram_image_over_the_download_limit_is_turned_away_before_it_is_fetched() {
        let msg = document_message("image/png", MAX_DOWNLOAD_BYTES + 1, Some("Look"));
        let incoming = route_update(&msg, &HashSet::new(), &[], None, unexpected_download)
            .await
            .unwrap();
        assert!(incoming.attachments.is_empty());
        assert!(
            incoming
                .content
                .starts_with("Look\n\n[Attachment 'fixture.png'")
        );
        assert!(incoming.content.contains("over the 32.0 MB download limit"));
    }

    #[tokio::test]
    async fn a_captioned_video_is_cached_without_inline_content() {
        let msg: Message = serde_json::from_value(serde_json::json!({
            "message_id": 500,
            "from": {"id": 1, "is_bot": false, "first_name": "Fixture"},
            "date": 1_767_225_600,
            "chat": {"id": CHAT, "type": "supergroup", "title": "Fixture"},
            "caption": "Look at this video",
            "video": {
                "file_id": "file", "file_unique_id": "unique",
                "width": 640, "height": 480, "duration": 5,
                "file_name": "fixture.mp4", "mime_type": "video/mp4"
            }
        }))
        .unwrap();
        assert!(matches!(msg, Message::Video(_)));
        let dir = tempfile::tempdir().unwrap();
        let store = AttachmentStore::open(dir.path().to_owned()).unwrap();
        let incoming = route_update(
            &msg,
            &HashSet::new(),
            &[],
            Some(store),
            |id, remaining| async move {
                assert_eq!(id, "file");
                assert_eq!(remaining, MAX_MESSAGE_ATTACHMENT_BYTES);
                Ok(b"video original".to_vec())
            },
        )
        .await
        .unwrap();
        assert!(incoming.attachments.is_empty());
        assert!(incoming.content.starts_with("Look at this video"));
        assert!(
            incoming
                .content
                .contains("cached original, not viewed inline")
        );
        assert_eq!(incoming.attachment_owner.unwrap().room_id, CHAT.to_string());
    }

    #[tokio::test]
    async fn captions_on_other_media_and_polls_are_not_lost() {
        for (key, media, note_name) in [
            (
                "voice",
                serde_json::json!({"file_id":"f", "file_unique_id":"u", "duration":1}),
                Some("Telegram voice"),
            ),
            (
                "audio",
                serde_json::json!({"file_id":"f", "file_unique_id":"u", "duration":1}),
                Some("Telegram audio"),
            ),
            (
                "paid_media",
                serde_json::json!({"star_count":1, "paid_media":[]}),
                None,
            ),
            (
                "poll",
                serde_json::json!({"type":"regular", "id":"p", "question":"Q", "options":[], "total_voter_count":0, "is_closed":false, "is_anonymous":true, "allows_multiple_answers":false, "allows_revoting":false}),
                None,
            ),
        ] {
            let mut fixture = serde_json::json!({
                "message_id":500, "from":{"id":1, "is_bot":false, "first_name":"Fixture"},
                "date":1_767_225_600, "chat":{"id":CHAT, "type":"supergroup", "title":"Fixture"},
                "caption":"Caption"
            });
            fixture[key] = media;
            let msg: Message = serde_json::from_value(fixture).unwrap();
            let incoming = route_update(&msg, &HashSet::new(), &[], None, |_, _| async {
                Ok(b"original".to_vec())
            })
            .await
            .expect("caption should be routed");
            assert!(incoming.attachments.is_empty());
            match note_name {
                Some(name) => assert!(
                    incoming
                        .content
                        .starts_with(&format!("Caption\n\n[Attachment '{name}'"))
                ),
                None => assert_eq!(incoming.content, "Caption"),
            }
        }
    }

    fn photo_message(size: usize) -> Message {
        serde_json::from_value(serde_json::json!({
            "message_id": 500,
            "from": {"id": 1, "is_bot": false, "first_name": "Fixture"},
            "date": 1_767_225_600,
            "chat": {"id": CHAT, "type": "supergroup", "title": "Fixture"},
            "photo": [
                {"file_id": "large", "file_unique_id": "l", "width": 1500, "height": 1230, "file_size": size},
                {"file_id": "small", "file_unique_id": "s", "width": 48, "height": 39}
            ]
        }))
        .unwrap()
    }

    #[tokio::test]
    async fn a_5_point_3_mb_telegram_photo_is_fetched_and_shrunk_without_a_caption() {
        let png = super::super::tests::noise_png(1500, 1230);
        assert_eq!(format_mb(png.len()), "5.3 MB");
        let msg = photo_message(png.len());
        let dir = tempfile::tempdir().unwrap();
        let store = AttachmentStore::open(dir.path().to_owned()).unwrap();
        let incoming = route_update(&msg, &HashSet::new(), &[], Some(store), |id, _| {
            let png = &png;
            async move {
                assert_eq!(id, "large");
                Ok(png.clone())
            }
        })
        .await
        .unwrap();
        assert!(
            incoming
                .content
                .contains("cached original, image shown inline")
        );
        let [attachment] = incoming.attachments.as_slice() else {
            panic!("expected one attachment")
        };
        assert_eq!(attachment.media_type, "image/jpeg");
        assert!(attachment.data.len() <= super::super::MAX_ATTACHMENT_BYTES);
        assert!(image::load_from_memory(&attachment.data).is_ok());
    }

    #[tokio::test]
    async fn a_telegram_document_uses_the_mime_of_its_bytes_not_its_label() {
        let png = super::super::tests::noise_png(32, 32);
        let msg = document_message("image/webp", png.len(), Some("Look"));
        let incoming = route_update(&msg, &HashSet::new(), &[], None, |_, _| async {
            Ok(png.clone())
        })
        .await
        .unwrap();
        assert!(incoming.content.starts_with("Look"));
        let [attachment] = incoming.attachments.as_slice() else {
            panic!("expected one attachment")
        };
        assert_eq!(attachment.media_type, "image/png");
        assert_eq!(attachment.data, png);

        let incoming = route_update(&msg, &HashSet::new(), &[], None, |_, _| async {
            Ok(b"not an image".to_vec())
        })
        .await
        .unwrap();
        assert!(incoming.attachments.is_empty());
        assert!(incoming.content.contains("original not cached"));
    }

    #[test]
    fn room_ids_split_back_into_their_halves() {
        assert_eq!(split_room_id("-1001").unwrap(), (-1001, None));
        assert_eq!(split_room_id("-1001:42").unwrap(), (-1001, Some(42)));
        assert!(split_room_id("-1001:x").is_err());
    }

    #[test]
    fn invalid_configured_bot_tokens_are_rejected_loudly() {
        let cfg = TelegramConfig {
            bot_token: "missing-separator".to_string(),
            rooms: Vec::new(),
            allowed_users: vec![],
        };
        let error = TelegramChannel::new(&cfg).unwrap_err().to_string();
        assert_eq!(error, "Invalid Telegram bot token");
    }

    #[test]
    fn invalid_configured_user_ids_are_rejected_loudly() {
        let cfg = TelegramConfig {
            bot_token: "123456:test".to_string(),
            rooms: Vec::new(),
            allowed_users: vec!["not-a-user-id".to_string()],
        };
        let error = TelegramChannel::new(&cfg).unwrap_err().to_string();
        assert_eq!(error, "Invalid Telegram user ID in allowed_users");
    }

    #[test]
    fn invalid_configured_room_ids_are_rejected_loudly() {
        let cfg = TelegramConfig {
            bot_token: "123456:test".to_string(),
            rooms: rooms(&[("-1001:not-a-thread", block(None, None, None))]),
            allowed_users: vec![],
        };
        let error = TelegramChannel::new(&cfg).unwrap_err().to_string();
        assert!(error.contains("Invalid Telegram room id -1001:not-a-thread"));
    }

    #[test]
    fn channel_metadata_and_backoff_match_the_other_channels() {
        let channel = TelegramChannel::new(&TelegramConfig {
            bot_token: "123456:test".to_string(),
            rooms: Vec::new(),
            allowed_users: vec![],
        })
        .unwrap();
        assert_eq!(channel.name(), "telegram");
        assert!(channel.supports_replies());
        assert_eq!(MIN_BACKOFF, Duration::from_secs(1));
        assert_eq!(MAX_BACKOFF, Duration::from_secs(300));
        assert_eq!(STABLE_THRESHOLD, Duration::from_secs(60));
    }
}
