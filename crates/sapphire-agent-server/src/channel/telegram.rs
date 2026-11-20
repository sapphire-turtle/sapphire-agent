//! Telegram Bot API channel.
//!
//! Bot API `getUpdates` never echoes the bot's own messages, so there is no
//! bot/self filter here, unlike Discord and Matrix. A forum topic's
//! `message_thread_id` is part of its composite room id; it never becomes an
//! [`IncomingMessage::thread_id`].

use crate::channel::{Channel, IncomingMessage, OutgoingMessage, with_attachment_notes};
use crate::config::{RoomBlock, TelegramConfig};
use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use indexmap::IndexMap;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};
use telers::methods::GetUpdates;
use telers::types::{Message, Update};
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
    allowed_users: HashSet<String>,
    rooms: IndexMap<String, RoomBlock>,
}

impl TelegramHandler {
    /// Forward a message, or do nothing when routing filters it out. An update
    /// is acknowledged only after this returns `Ok`, so a closed receiver
    /// cannot silently discard it.
    async fn message(&self, msg: &Message) -> Result<()> {
        let Some(incoming) = route_update(msg, &self.allowed_users, &self.rooms).await else {
            return Ok(());
        };
        self.tx
            .send(incoming)
            .await
            .map_err(|e| anyhow!("Failed to forward Telegram message: {e}"))
    }
}

// ---------------------------------------------------------------------------
// TelegramChannel
// ---------------------------------------------------------------------------

pub struct TelegramChannel {
    token: String,
    rooms: IndexMap<String, RoomBlock>,
    allowed_users: HashSet<String>,
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
    // Registered in the channel list by the later integration task.
    #[allow(dead_code)]
    pub fn new(cfg: &TelegramConfig) -> Result<Self> {
        for key in cfg.rooms.keys() {
            split_room_id(key)?;
        }
        Ok(Self {
            token: cfg.bot_token.clone(),
            rooms: cfg.rooms.clone(),
            allowed_users: cfg.allowed_users.iter().cloned().collect(),
            client: Arc::new(OnceCell::new()),
        })
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

    async fn send(&self, _message: &OutgoingMessage) -> Result<Vec<String>> {
        // Outbound chunking, replies and typing are the next implementation task.
        let _ = self.get_client_or_new();
        bail!("Telegram send is not implemented yet")
    }

    /// Long-running: long-poll Bot API updates and forward incoming messages
    /// through `tx`. Reconnects with exponential backoff on polling errors.
    async fn listen(&self, tx: mpsc::Sender<IncomingMessage>) -> Result<()> {
        let handler = TelegramHandler {
            tx,
            allowed_users: self.allowed_users.clone(),
            rooms: self.rooms.clone(),
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
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

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
                handler.message(&message.message).await?;
            }
            // Filtered messages are handled too. An unfiltered message reaches
            // here only after its send into the agent's queue succeeded.
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
/// `rooms` is the listen list: an empty map accepts every visible room. The
/// attachment path is filled in by the attachment task; for now text-only
/// messages are routed and messages without text are ignored.
pub async fn route_update(
    msg: &Message,
    allowed_users: &HashSet<String>,
    rooms: &IndexMap<String, RoomBlock>,
) -> Option<IncomingMessage> {
    let from = msg.from()?;
    let sender = from.id.to_string();
    if !allowed_users.is_empty() && !allowed_users.contains(&sender) {
        debug!("Ignoring message from user {sender} (not in allowed list)");
        return None;
    }

    let room_id = room_id(msg.chat().id(), msg.message_thread_id());
    if !rooms.is_empty() && !rooms.contains_key(&room_id) {
        debug!("Ignoring message from room {room_id} (not in allowed list)");
        return None;
    }

    let content = with_attachment_notes(msg.text().unwrap_or_default().trim().to_owned(), &[]);
    if content.is_empty() {
        return None;
    }

    Some(IncomingMessage {
        id: msg.message_id().to_string(),
        sender,
        content,
        room_id,
        timestamp: msg.date() as u64 * 1000,
        thread_id: None,
        attachments: vec![],
        reply_to: reply_target(msg),
    })
}

/// The configured room id for a chat, including its forum topic when present.
pub fn room_id(chat_id: i64, thread: Option<i64>) -> String {
    match thread {
        Some(thread) => format!("{chat_id}:{thread}"),
        None => chat_id.to_string(),
    }
}

/// Split `<chat_id>` or `<chat_id>:<message_thread_id>` into signed 64-bit ids.
// Used by the constructor and the later outbound implementation.
#[allow(dead_code)]
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
    use indexmap::IndexMap;
    use std::collections::HashSet;
    use telers::types::Message;

    const CHAT: i64 = -1_002_000_333_444_555;

    fn block(
        name: Option<&str>,
        description: Option<&str>,
        profile: Option<&str>,
    ) -> crate::config::RoomBlock {
        crate::config::RoomBlock {
            name: name.map(str::to_owned),
            description: description.map(str::to_owned),
            profile: profile.map(str::to_owned),
        }
    }

    fn rooms(
        entries: &[(&str, crate::config::RoomBlock)],
    ) -> IndexMap<String, crate::config::RoomBlock> {
        entries
            .iter()
            .map(|(id, block)| ((*id).to_owned(), block.clone()))
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
        debug_assert_eq!(msg.message_thread_id(), thread_id);
        debug_assert_eq!(msg.chat().id(), chat_id);
        let round_trip: Message =
            serde_json::from_value(serde_json::to_value(&msg).unwrap()).unwrap();
        debug_assert_eq!(round_trip.message_thread_id(), thread_id);
        debug_assert_eq!(round_trip.chat().id(), chat_id);
        debug_assert_eq!(round_trip.from().unwrap().id, from_id);
        msg
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
        let incoming = route_update(&msg, &HashSet::new(), &rooms).await.unwrap();
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
            &IndexMap::new(),
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
        )
        .await;
        assert!(
            missed.is_none(),
            "rooms non-empty and the room is not a key"
        );
        let empty = route_update(
            &fixture_message(CHAT, Some(42), "9", "hi", None),
            &HashSet::from(["1".to_string()]),
            &IndexMap::new(),
        )
        .await;
        assert!(empty.is_none(), "allowed_users non-empty miss");
    }

    #[tokio::test]
    async fn an_empty_message_with_no_attachment_is_ignored() {
        assert!(
            route_update(
                &fixture_message(CHAT, Some(42), "1", "", None),
                &HashSet::new(),
                &IndexMap::new(),
            )
            .await
            .is_none()
        );
    }

    #[test]
    fn room_ids_split_back_into_their_halves() {
        assert_eq!(split_room_id("-1001").unwrap(), (-1001, None));
        assert_eq!(split_room_id("-1001:42").unwrap(), (-1001, Some(42)));
        assert!(split_room_id("-1001:x").is_err());
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
            rooms: IndexMap::new(),
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
