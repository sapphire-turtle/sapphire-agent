pub mod discord;
pub mod matrix;

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{RwLock, mpsc};
use tracing::{error, warn};

/// Maximum size of a single image attachment forwarded to the LLM.
/// Anthropic's documented limit is 5 MB per image; oversized attachments are
/// dropped with a warning (the conversation continues without them).
pub const MAX_ATTACHMENT_BYTES: usize = 5 * 1024 * 1024;

/// MIME types of images forwarded to the LLM. Anthropic accepts these four;
/// anything else is dropped at the channel layer.
pub(crate) const SUPPORTED_IMAGE_MIME: &[&str] =
    &["image/jpeg", "image/png", "image/gif", "image/webp"];

/// Detect the MIME type of an image from its magic bytes. Returns `None`
/// when the bytes don't match one of the four types Anthropic accepts.
///
/// Used in preference to the platform-declared content type because
/// Discord (and some Matrix clients) sometimes label, say, a PNG as
/// `image/webp`. Anthropic strictly validates that the declared
/// `media_type` matches the actual bytes and 400s on mismatch.
pub(crate) fn sniff_image_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if bytes.starts_with(&[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]) {
        Some("image/png")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

/// A binary attachment fetched from a channel (currently images only).
#[derive(Debug, Clone)]
pub struct Attachment {
    /// MIME type, e.g. `image/png`, `image/jpeg`.
    pub media_type: String,
    /// Raw bytes.
    pub data: Vec<u8>,
}

/// A message received from a channel.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct IncomingMessage {
    /// Platform-specific message ID.
    pub id: String,
    /// Sender's user ID (e.g. `@user:example.com`).
    pub sender: String,
    /// Text content.
    pub content: String,
    /// Room/conversation identifier to reply to.
    pub room_id: String,
    /// Unix timestamp (milliseconds).
    pub timestamp: u64,
    /// Thread identifier for threaded replies, if applicable.
    pub thread_id: Option<String>,
    /// Image attachments accompanying the message.
    pub attachments: Vec<Attachment>,
}

/// A message to send through a channel.
#[derive(Debug, Clone)]
pub struct OutgoingMessage {
    pub content: String,
    /// Room/conversation to send to.
    pub room_id: String,
    /// Thread identifier for threaded replies.
    pub thread_id: Option<String>,
}

impl OutgoingMessage {
    pub fn new(content: impl Into<String>, room_id: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            room_id: room_id.into(),
            thread_id: None,
        }
    }
}

/// Channel-side description of a room, surfaced to the agent so the
/// system prompt can tell the model **where it is talking**. Useful for
/// rooms whose communication conventions differ (e.g. a voice channel
/// fed by STT contains transcription errors; a dedicated work-mode room
/// expects different tone).
#[derive(Debug, Clone)]
pub struct RoomInfo {
    /// Display name. For Matrix it's `room.name()` (display name, or DM
    /// recipient); for Discord it's `GuildChannel.name`; for API/voice
    /// it's a server-side template like `"voice channel with <device>"`.
    pub name: String,
    /// Free-form description / topic. `None` when the channel side has
    /// nothing to offer (e.g. Matrix DM with no topic).
    pub description: Option<String>,
    /// Originating channel name: `"matrix"`, `"discord"`, `"rpc"`,
    /// `"device-default"`, or `"voice"`. Lets the system prompt tell
    /// the model whether transcripts may contain STT errors etc.
    pub kind: String,
}

/// Core channel trait.
#[async_trait]
#[allow(dead_code)]
pub trait Channel: Send + Sync {
    fn name(&self) -> &str;

    async fn send(&self, message: &OutgoingMessage) -> anyhow::Result<()>;

    /// Long-running: receive messages and forward them through `tx`.
    async fn listen(&self, tx: tokio::sync::mpsc::Sender<IncomingMessage>) -> anyhow::Result<()>;

    async fn start_typing(&self, _room_id: &str) -> anyhow::Result<()> {
        Ok(())
    }

    async fn stop_typing(&self, _room_id: &str) -> anyhow::Result<()> {
        Ok(())
    }

    /// Display metadata for `room_id`, when the channel can look it up
    /// cheaply (Matrix room state, Discord channel cache). Default: `None`.
    async fn room_info(&self, _room_id: &str) -> Option<RoomInfo> {
        None
    }
}

/// Dispatcher that fronts every configured `Channel` (Matrix + Discord)
/// for the agent. Maintains a `room_id → channel name` routing map so
/// outgoing replies and typing indicators land on the channel the
/// originating message came from.
///
/// The routing map is seeded from `Config` (Matrix.room_ids and
/// Discord.channel_ids) so proactive sends to known rooms work without
/// having received a message first; it's also updated on every
/// incoming message so DMs / unlisted rooms become routable as soon as
/// the user pings the agent there.
pub struct Channels {
    list: Vec<(String, Arc<dyn Channel>)>,
    routing: RwLock<HashMap<String, String>>,
}

impl Channels {
    pub fn new(list: Vec<(String, Arc<dyn Channel>)>, seed: HashMap<String, String>) -> Self {
        Self {
            list,
            routing: RwLock::new(seed),
        }
    }

    /// Names of every configured channel, in the order they were
    /// registered. Useful for diagnostics and bootstrap room filtering.
    pub fn names(&self) -> Vec<&str> {
        self.list.iter().map(|(n, _)| n.as_str()).collect()
    }

    /// True when no channels are registered (e.g. API-only deployment).
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.list.is_empty()
    }

    /// Channel name (as recorded in session metadata) responsible for a
    /// given `room_id`, if known.
    pub async fn channel_name_for_room(&self, room_id: &str) -> Option<String> {
        self.routing.read().await.get(room_id).cloned()
    }

    fn channel_by_name(&self, name: &str) -> Option<&Arc<dyn Channel>> {
        self.list.iter().find(|(n, _)| n == name).map(|(_, c)| c)
    }

    async fn channel_for_room_or_first(&self, room_id: &str) -> Option<Arc<dyn Channel>> {
        if let Some(name) = self.channel_name_for_room(room_id).await
            && let Some(ch) = self.channel_by_name(&name)
        {
            return Some(Arc::clone(ch));
        }
        // Last-ditch fallback: if only one channel is configured, use it.
        if self.list.len() == 1 {
            return Some(Arc::clone(&self.list[0].1));
        }
        None
    }

    pub async fn send(&self, msg: &OutgoingMessage) -> Result<()> {
        let ch = self
            .channel_for_room_or_first(&msg.room_id)
            .await
            .ok_or_else(|| anyhow!("no channel registered for room {}", msg.room_id))?;
        ch.send(msg).await
    }

    pub async fn start_typing(&self, room_id: &str) -> Result<()> {
        if let Some(ch) = self.channel_for_room_or_first(room_id).await {
            ch.start_typing(room_id).await?;
        }
        Ok(())
    }

    /// Resolve `RoomInfo` for `room_id` through whichever channel owns
    /// the room. Returns `None` if no channel is registered for the room
    /// or the channel has nothing to say about it.
    pub async fn room_info(&self, room_id: &str) -> Option<RoomInfo> {
        let ch = self.channel_for_room_or_first(room_id).await?;
        ch.room_info(room_id).await
    }

    pub async fn stop_typing(&self, room_id: &str) -> Result<()> {
        if let Some(ch) = self.channel_for_room_or_first(room_id).await {
            ch.stop_typing(room_id).await?;
        }
        Ok(())
    }

    /// Spawn a `listen` task per registered channel, all forwarding into
    /// the same outgoing `tx`. Each forwarded message updates the
    /// routing map so the originating channel is locked in for any
    /// subsequent outgoing reply.
    pub async fn listen_all(self: Arc<Self>, tx: mpsc::Sender<IncomingMessage>) -> Result<()> {
        if self.list.is_empty() {
            return Err(anyhow!("listen_all called with no channels registered"));
        }
        let mut handles: Vec<tokio::task::JoinHandle<()>> = Vec::new();
        for entry in &self.list {
            let (name, ch) = (entry.0.clone(), Arc::clone(&entry.1));
            let outer_tx = tx.clone();
            let me = Arc::clone(&self);
            let (inner_tx, mut inner_rx) = mpsc::channel::<IncomingMessage>(64);
            let listen_name = name.clone();
            handles.push(tokio::spawn(async move {
                if let Err(e) = ch.listen(inner_tx).await {
                    error!("Channel '{listen_name}' listen error: {e:#}");
                }
            }));
            let forward_name = name.clone();
            handles.push(tokio::spawn(async move {
                while let Some(msg) = inner_rx.recv().await {
                    me.routing
                        .write()
                        .await
                        .insert(msg.room_id.clone(), forward_name.clone());
                    if outer_tx.send(msg).await.is_err() {
                        warn!("Channel '{forward_name}' forwarder: receiver closed");
                        break;
                    }
                }
            }));
        }
        // Wait until any of the listen tasks ends — that's our signal
        // to abort the rest. (Forward tasks exit on tx close after
        // listens drop their senders.)
        for h in handles {
            let _ = h.await;
        }
        Ok(())
    }
}

/// Build the routing seed map from a `Config` so proactive sends
/// (heartbeat cron tasks, etc.) succeed without needing a prior
/// inbound message.
///
/// Lives here rather than `Channels::new` to avoid circular dependency
/// concerns and so test code can build a `Channels` without a `Config`.
pub fn seed_routing_from_config(config: &crate::config::Config) -> HashMap<String, String> {
    let mut seed = HashMap::new();
    if let Some(m) = &config.matrix {
        for r in &m.room_ids {
            seed.insert(r.clone(), "matrix".to_string());
        }
    }
    if let Some(d) = &config.discord {
        for c in &d.channel_ids {
            seed.insert(c.clone(), "discord".to_string());
        }
    }
    seed
}

#[cfg(test)]
mod tests {
    use super::sniff_image_mime;

    #[test]
    fn sniffs_jpeg() {
        assert_eq!(
            sniff_image_mime(&[0xFF, 0xD8, 0xFF, 0xE0, 0, 0]),
            Some("image/jpeg")
        );
    }

    #[test]
    fn sniffs_png() {
        let png = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0, 0];
        assert_eq!(sniff_image_mime(&png), Some("image/png"));
    }

    #[test]
    fn sniffs_gif() {
        assert_eq!(sniff_image_mime(b"GIF87a..."), Some("image/gif"));
        assert_eq!(sniff_image_mime(b"GIF89a..."), Some("image/gif"));
    }

    #[test]
    fn sniffs_webp() {
        let mut webp = Vec::new();
        webp.extend_from_slice(b"RIFF");
        webp.extend_from_slice(&[0; 4]);
        webp.extend_from_slice(b"WEBP");
        webp.extend_from_slice(b"VP8 ");
        assert_eq!(sniff_image_mime(&webp), Some("image/webp"));
    }

    #[test]
    fn returns_none_for_unknown() {
        assert_eq!(sniff_image_mime(&[0, 0, 0, 0]), None);
        assert_eq!(sniff_image_mime(b""), None);
        // RIFF prefix without WEBP marker (e.g. WAV) is not an image.
        let mut wav = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&[0; 4]);
        wav.extend_from_slice(b"WAVE");
        assert_eq!(sniff_image_mime(&wav), None);
    }
}
