pub mod discord;
pub mod matrix;

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{RwLock, mpsc};
use tracing::{error, warn};

/// Maximum size of a single image attachment forwarded to the LLM.
/// Anthropic's documented limit is 5 MB per image; a larger image is shrunk
/// to fit by [`prepare_image`].
pub const MAX_ATTACHMENT_BYTES: usize = 5 * 1024 * 1024;

/// Largest image a channel downloads. Anything bigger is turned away before
/// it is fetched; anything between this and [`MAX_ATTACHMENT_BYTES`] is
/// shrunk by [`prepare_image`] rather than dropped.
pub const MAX_DOWNLOAD_BYTES: usize = 32 * 1024 * 1024;

/// Longest side, in pixels, an oversized image is scaled down to. Anthropic
/// scales anything larger down to about this itself, so the model loses
/// nothing it would have seen.
const DOWNSCALE_LONG_SIDE: u32 = 1568;

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

/// Turn downloaded image bytes into an [`Attachment`] the provider accepts.
///
/// The MIME type is taken from the bytes, not from what the platform
/// declared (see [`sniff_image_mime`]). An image within
/// [`MAX_ATTACHMENT_BYTES`] passes through untouched; a larger one is scaled
/// to [`DOWNSCALE_LONG_SIDE`] and re-encoded as JPEG, at falling quality
/// until it fits. `Err` carries why the image cannot be passed on, worded to
/// finish the sentence of [`dropped_attachment_note`].
///
/// Decoding and encoding are CPU-bound: call it from `spawn_blocking`.
pub(crate) fn prepare_image(bytes: Vec<u8>) -> Result<Attachment, String> {
    let Some(media_type) = sniff_image_mime(&bytes) else {
        return Err("its format is not one of JPEG, PNG, GIF or WebP".to_string());
    };
    if bytes.len() <= MAX_ATTACHMENT_BYTES {
        return Ok(Attachment {
            media_type: media_type.to_string(),
            data: bytes,
        });
    }
    let data = shrink_image(&bytes)?;
    Ok(Attachment {
        media_type: "image/jpeg".to_string(),
        data,
    })
}

fn shrink_image(bytes: &[u8]) -> Result<Vec<u8>, String> {
    use image::codecs::jpeg::JpegEncoder;
    use image::imageops::FilterType;

    let original = bytes.len();
    let img = image::load_from_memory(bytes).map_err(|e| {
        format!(
            "it is {} (over the {} limit) and could not be decoded to shrink it ({e})",
            format_mb(original),
            format_mb(MAX_ATTACHMENT_BYTES)
        )
    })?;
    let img = if img.width().max(img.height()) > DOWNSCALE_LONG_SIDE {
        img.resize(
            DOWNSCALE_LONG_SIDE,
            DOWNSCALE_LONG_SIDE,
            FilterType::Triangle,
        )
    } else {
        img
    };
    // JPEG has no alpha: lay the image over white so transparent areas do
    // not turn into whatever colour their invisible pixels happen to hold.
    let rgba = img.to_rgba8();
    let rgb = image::RgbImage::from_fn(rgba.width(), rgba.height(), |x, y| {
        let [r, g, b, a] = rgba.get_pixel(x, y).0;
        let over_white = |c: u8| ((c as u16 * a as u16 + 255 * (255 - a as u16)) / 255) as u8;
        image::Rgb([over_white(r), over_white(g), over_white(b)])
    });
    for quality in [85, 70, 50] {
        let mut out = Vec::new();
        JpegEncoder::new_with_quality(&mut out, quality)
            .encode_image(&rgb)
            .map_err(|e| format!("shrinking it failed ({e})"))?;
        if out.len() <= MAX_ATTACHMENT_BYTES {
            tracing::info!(
                "Shrank a {} image to a {} JPEG ({}x{}, quality {quality})",
                format_mb(original),
                format_mb(out.len()),
                rgb.width(),
                rgb.height()
            );
            return Ok(out);
        }
    }
    Err(format!(
        "it is {} and is still over the {} limit after shrinking",
        format_mb(original),
        format_mb(MAX_ATTACHMENT_BYTES)
    ))
}

/// `5_539_085` → `"5.3 MB"` (MiB, as the limits are).
pub(crate) fn format_mb(bytes: usize) -> String {
    format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
}

/// The line that tells the model an attachment arrived but was not passed
/// on, and why. Without it the model sees only the text and can do no
/// better than tell the user it sees no image.
pub(crate) fn dropped_attachment_note(filename: &str, reason: &str) -> String {
    format!(
        "[Attachment '{filename}' was sent but not passed on to you: {reason}. \
         Tell the user if it matters.]"
    )
}

/// `content` followed by `notes`, a blank line between. The text is left
/// alone when there are no notes.
pub(crate) fn with_attachment_notes(content: String, notes: &[String]) -> String {
    if notes.is_empty() {
        return content;
    }
    let notes = notes.join("\n");
    if content.is_empty() {
        notes
    } else {
        format!("{content}\n\n{notes}")
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
    /// ID of the message this one replies to, when the platform says so.
    pub reply_to: Option<String>,
}

/// A message to send through a channel.
#[derive(Debug, Clone)]
pub struct OutgoingMessage {
    pub content: String,
    /// Room/conversation to send to.
    pub room_id: String,
    /// Thread identifier for threaded replies.
    pub thread_id: Option<String>,
    /// ID of a message to post this one as a reply to (first chunk only
    /// when the platform splits it).
    pub reply_to: Option<String>,
}

impl OutgoingMessage {
    pub fn new(content: impl Into<String>, room_id: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            room_id: room_id.into(),
            thread_id: None,
            reply_to: None,
        }
    }

    /// Post this message as a reply to the message with platform ID `id`.
    pub fn replying_to(mut self, id: impl Into<String>) -> Self {
        self.reply_to = Some(id.into());
        self
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

    /// Post `message`; returns the IDs of the posted messages in order.
    async fn send(&self, message: &OutgoingMessage) -> anyhow::Result<Vec<String>>;

    /// Whether `send` honours `OutgoingMessage::reply_to` and incoming
    /// messages carry `reply_to`. Default: `false`.
    fn supports_replies(&self) -> bool {
        false
    }

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

    /// Route `room_id` to the channel named `channel`, as if a message had
    /// arrived there. For a room the seed from `Config` can't know, such as
    /// a Discord consult room when `channel_ids` is empty (listen to all).
    pub async fn seed_room(&self, room_id: &str, channel: &str) {
        self.routing
            .write()
            .await
            .insert(room_id.to_string(), channel.to_string());
    }

    /// Whether a channel serves `room_id` (no single-channel fallback).
    pub async fn serves(&self, room_id: &str) -> bool {
        self.channel_for_room(room_id).await.is_some()
    }

    /// Whether the channel serving `room_id` supports replies; `None`
    /// when no channel serves the room.
    pub async fn supports_replies(&self, room_id: &str) -> Option<bool> {
        self.channel_for_room(room_id)
            .await
            .map(|c| c.supports_replies())
    }

    async fn channel_for_room(&self, room_id: &str) -> Option<Arc<dyn Channel>> {
        let name = self.channel_name_for_room(room_id).await?;
        self.channel_by_name(&name).map(Arc::clone)
    }

    pub async fn send(&self, msg: &OutgoingMessage) -> Result<Vec<String>> {
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
    use super::*;

    struct FakeChannel;

    #[async_trait]
    impl Channel for FakeChannel {
        fn name(&self) -> &str {
            "fake"
        }
        async fn send(&self, _message: &OutgoingMessage) -> anyhow::Result<Vec<String>> {
            Ok(vec!["a".to_string(), "b".to_string()])
        }
        fn supports_replies(&self) -> bool {
            true
        }
        async fn listen(&self, _tx: mpsc::Sender<IncomingMessage>) -> anyhow::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn channels_report_reply_support_per_room() {
        let ch: Arc<dyn Channel> = Arc::new(FakeChannel);
        let seed = HashMap::from([("r1".to_string(), "fake".to_string())]);
        let channels = Channels::new(vec![("fake".to_string(), ch)], seed);
        assert_eq!(channels.supports_replies("r1").await, Some(true));
        assert_eq!(channels.supports_replies("r2").await, None);
        assert!(channels.serves("r1").await);
        assert!(!channels.serves("r2").await);
        let ids = channels
            .send(&OutgoingMessage::new("hi", "r1").replying_to("9"))
            .await
            .unwrap();
        assert_eq!(ids, vec!["a", "b"]);
    }

    #[tokio::test]
    async fn a_seeded_room_is_served() {
        let ch: Arc<dyn Channel> = Arc::new(FakeChannel);
        let channels = Channels::new(vec![("fake".to_string(), ch)], HashMap::new());
        assert!(!channels.serves("42").await);
        channels.seed_room("42", "fake").await;
        assert!(channels.serves("42").await);
        assert_eq!(channels.supports_replies("42").await, Some(true));
    }

    /// A PNG of pseudo-random noise: it does not compress, so its size is
    /// close to `w * h * 3` bytes.
    fn noise_png(w: u32, h: u32) -> Vec<u8> {
        let mut state: u32 = 0x1234_5678;
        let img = image::RgbImage::from_fn(w, h, |_, _| {
            let mut px = [0u8; 3];
            for c in &mut px {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                *c = (state >> 24) as u8;
            }
            image::Rgb(px)
        });
        let mut out = std::io::Cursor::new(Vec::new());
        img.write_to(&mut out, image::ImageFormat::Png).unwrap();
        out.into_inner()
    }

    #[test]
    fn a_small_image_passes_through_untouched() {
        let png = noise_png(32, 32);
        let att = prepare_image(png.clone()).unwrap();
        assert_eq!(att.media_type, "image/png");
        assert_eq!(att.data, png);
    }

    #[test]
    fn an_oversized_image_is_shrunk_to_a_jpeg_under_the_limit() {
        let png = noise_png(2000, 1500);
        assert!(
            png.len() > MAX_ATTACHMENT_BYTES,
            "fixture must be oversized"
        );
        let att = prepare_image(png).unwrap();
        assert_eq!(att.media_type, "image/jpeg");
        assert!(att.data.len() <= MAX_ATTACHMENT_BYTES);
        let shrunk = image::load_from_memory(&att.data).unwrap();
        assert_eq!(shrunk.width(), DOWNSCALE_LONG_SIDE);
        assert_eq!(shrunk.height(), 1176);
    }

    #[test]
    fn unrecognised_bytes_are_refused_with_a_reason() {
        let reason = prepare_image(b"not an image".to_vec()).unwrap_err();
        assert!(reason.contains("JPEG, PNG, GIF or WebP"), "{reason}");
    }

    #[test]
    fn a_dropped_attachment_is_noted_after_the_text() {
        let notes = vec![dropped_attachment_note("portrait.png", "it is broken")];
        let content = with_attachment_notes("look at this".to_string(), &notes);
        assert!(content.starts_with("look at this\n\n[Attachment 'portrait.png'"));
        assert!(content.contains("it is broken"));
        // No text: the note alone.
        let content = with_attachment_notes(String::new(), &notes);
        assert!(content.starts_with("[Attachment 'portrait.png'"));
        // No notes: the text unchanged.
        assert_eq!(with_attachment_notes("hi".to_string(), &[]), "hi");
    }

    #[test]
    fn sizes_read_in_megabytes() {
        assert_eq!(format_mb(5_539_085), "5.3 MB");
    }

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
