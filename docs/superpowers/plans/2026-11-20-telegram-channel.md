# Telegram Channel Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add `telegram` as a third `Channel` implementation, and move per-room configuration out of `[room_profile.<n>].rooms` into per-room blocks keyed by room id under each channel section (`[telegram.rooms."<room_id>"]`, `[matrix.rooms."<room_id>"]`, `[discord.rooms."<room_id>"]`).

**Architecture:** A Telegram room is the composite id `<chat_id>` or `<chat_id>:<message_thread_id>`, carried verbatim as `IncomingMessage.room_id` with `thread_id` always `None` — everything downstream already keys on `room_id` alone. Each channel config owns an ordered map of `RoomBlock`s; that map is simultaneously the listen list (empty = all visible), the routing seed, the display metadata (`name`/`description`), and the room-profile binding (`profile`).

**Tech Stack:** tokio, serde/toml, `telers` (tokio-native Telegram Bot API framework; fall back to `teloxide` with the same shape if telers mishandles forum topics — check in Task 2 Step 1), `indexmap` (order-preserving maps).

**Spec:** `docs/superpowers/specs/2026-11-20-telegram-channel-design.md`

## Global Constraints

- Docs, code comments, test names and commit messages are English. Every commit carries the trailer `Co-authored-by: Saphina <spph12@sapphire-turtle.org>` (origin is `github.com:sapphire-turtle` — the fwaunstp exception does not apply).
- All tests are offline unit tests in the style of the existing channel modules; no live Telegram calls. Run: `cargo test -p sapphire-agent-server <filter>`.
- A new dependency is always `default-features = false` with only the features actually needed.
- Room id string form: `<chat_id>` or `<chat_id>:<message_thread_id>`; each half parses as a signed 64-bit integer; group/channel chat ids look like `-1002000333444555`.
- The channel implementation mirrors the shape of `channel/discord.rs` (struct with an `OnceCell`-shared client handle, exponential backoff: min 1s, max 300s, reset after 60s connected cleanly).
- `room_profile_for` order after the change: a room block's `profile` key > `[room_profile.default]` > `None`. `profile_for`, `namespace_for_room`, `session_policy_for`, `room_profile_name_for_room` keep their bodies unchanged — they all call `room_profile_for`.

---

### Task 1: Config — RoomBlock, per-channel rooms maps, resolution, migration

**Files:**
- Modify: `crates/sapphire-agent-server/src/config.rs`
- Modify: `crates/sapphire-agent-server/Cargo.toml` (add `indexmap`)
- Modify: `crates/sapphire-agent-server/config.example.toml`
- Test: inline `mod tests` in `config.rs`

**Interfaces:**
- Consumes: nothing new (this is the base).
- Produces: `RoomBlock { name: Option<String>, description: Option<String>, profile: Option<String> }`; `TelegramConfig { bot_token: String, allowed_users: Vec<String>, rooms: IndexMap<String, RoomBlock> }`; `Config::telegram: Option<TelegramConfig>`; `Config::room_block_for(&str) -> Option<&RoomBlock>`; `room_profile_for` resolving via blocks; `MatrixConfig::primary_room_id()` = first `rooms` key. Task 2's `TelegramChannel` and Task 4's seed routing read these.

- [ ] **Step 1: Write the failing tests**

Add to `config.rs`'s `mod tests` (keep the existing ones that still hold; the legacy-key tests get renamed below):

```rust
#[test]
fn a_room_block_profile_wins_and_names_the_room() {
    let cfg = parse(
        "[anthropic]\napi_key = \"test\"\n\n[profiles.dev]\nprovider = \"stub\"\n\n\
         [room_profile.dev]\nprofile = \"dev\"\nmemory_namespace = \"ns\"\n\n\
         [telegram]\nbot_token = \"t\"\n\n\
         [telegram.rooms.\"-1002000333444555:42\"]\nname = \"dev\"\nprofile = \"dev\"\n",
    );
    assert_eq!(cfg.room_profile_name_for_room("-1002000333444555:42"), "dev");
    assert_eq!(cfg.namespace_for_room("-1002000333444555:42"), "ns");
    let block = cfg.room_block_for("-1002000333444555:42").unwrap();
    assert_eq!(block.name.as_deref(), Some("dev"));
    assert_eq!(block.description.as_deref(), None);
}

#[test]
fn a_block_without_profile_falls_through_exactly_as_today() {
    // A block with no `profile` key leaves the room matched by nothing:
    // [room_profile.default] catches it, then the implicit "default".
    let cfg = parse("[anthropic]\napi_key = \"test\"\n\n[telegram]\nbot_token = \"t\"\n\
             [telegram.rooms.\"-1001\"]\nname = \"daily\"\n");
    assert_eq!(cfg.room_profile_name_for_room("-1001"), "default");
}

#[test]
#[test]
fn the_first_configured_room_is_the_primary() {
    // IndexMap keeps document order, so the heartbeat default target is
    // deterministic across restarts.
    let cfg = parse(
        "[anthropic]\napi_key = \"test\"\n\n[matrix]\n\
         homeserver = \"h\"\naccess_token = \"t\"\nuser_id = \"u\"\ndevice_id = \"d\"\n\
         [matrix.rooms.\"!a:x\"]\n[matrix.rooms.\"!b:x\"]\n",
    );
    assert_eq!(cfg.matrix.as_ref().unwrap().primary_room_id(), Some("!a:x"));
}
}

#[test]
fn leftover_room_lists_are_migration_errors() {
    let cfg = parse("[anthropic]\napi_key = \"t\"\n[matrix]\nroom_ids = [\"!a:x\"]\n");
    let errors = cfg.migration_errors();
    assert!(errors.iter().any(|e| e.contains("matrix.room_ids") && e.contains("[matrix.rooms")), "{errors:?}");
    let cfg = parse("[anthropic]\napi_key = \"t\"\n[discord]\nbot_token = \"t\"\nchannel_ids = [\"7\"]\n");
    let errors = cfg.migration_errors();
    assert!(errors.iter().any(|e| e.contains("discord.channel_ids")), "{errors:?}");
    let cfg = parse("[anthropic]\napi_key = \"t\"\n[room_profile.ops]\nprofile = \"x\"\nrooms = [\"!a:x\"]\n");
    let errors = cfg.migration_errors();
    assert!(errors.iter().any(|e| e.contains("room_profile") && e.contains("rooms")), "{errors:?}");
}

#[test]
fn a_block_naming_an_unknown_room_profile_is_a_validation_error() {
    let cfg = parse("[anthropic]\napi_key = \"t\"\n[telegram]\nbot_token = \"t\"\n\
             [telegram.rooms.\"-1001\"]\nprofile = \"ghost\"\n");
    let errors = cfg.validate_profiles();
    assert!(errors.iter().any(|e| e.contains("-1001") && e.contains("ghost")), "{errors:?}");
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p sapphire-agent-server config::tests -- --nocapture` → compile errors (`TelegramConfig`, `RoomBlock`, `room_block_for` unknown) — that is the failure.

- [ ] **Step 3: Implement**

In `Cargo.toml` add: `indexmap = { version = "2", features = ["serde"] }`. In `config.rs`:

```rust
/// A per-room settings block, keyed by room id under a channel section:
/// `[telegram.rooms."<room_id>"]`. The channel's rooms map is at once the
/// listen list (empty = all the bot can see), the routing seed, the
/// display metadata surfaced via `RoomInfo`, and — via `profile` — the
/// room profile this room runs under. Rooms now name the bundle they belong
/// to, rather than the bundle listing its rooms.
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct RoomBlock {
    /// Display name for `RoomInfo`. Absent with no `description` means the
    /// raw room id is shown, as-is.
    #[serde(default)]
    pub name: Option<String>,
    /// Free-form room description — the role Matrix's room topic and
    /// Discord's channel topic play.
    #[serde(default)]
    pub description: Option<String>,
    /// The `[room_profile.<n>]` bundle this room runs under. Absent means
    /// the existing fallthrough (`[room_profile.default]`, then built-in
    /// defaults) applies, exactly as today.
    #[serde(default)]
    pub profile: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TelegramConfig {
    pub bot_token: String,
    /// Rooms the bot listens to, keyed by composite room id (`<chat_id>` or
    /// `<chat_id>:<message_thread_id>`). Empty = all chats the bot can see.
    #[serde(default)]
    pub rooms: IndexMap<String, RoomBlock>,
    /// Messages from users not in this list are silently ignored. Empty = all.
    #[serde(default)]
    pub allowed_users: Vec<String>,
}
```

`Config`: add `#[serde(default)] pub telegram: Option<TelegramConfig>` (doc: may coexist with matrix/discord). `MatrixConfig`/`DiscordConfig`: add `#[serde(default)] pub rooms: IndexMap<String, RoomBlock>`; keep the old `room_ids`/`channel_ids` fields but their doc becomes "retained only so `Config::migration_errors` can name a config that still sets it — replaced by `rooms`" (drop the legacy `alias = "room_id"` handling only if the legacy alias key should now error too — it should: drop `deserialize_room_ids` and the alias, plain `#[serde(default)]`). `RoomProfileConfig.rooms`: doc becomes migration-error-only.

New helper + resolver change:

```rust
/// The per-room block for `room_id`, wherever its channel is configured
/// (matrix > discord > telegram). A room appears in at most one channel's
/// map; this order only matters for the pathological config that breaks that.
pub fn room_block_for(&self, room_id: &str) -> Option<&RoomBlock> {
    self.matrix.as_ref().and_then(|m| m.rooms.get(room_id))
        .or_else(|| self.discord.as_ref().and_then(|d| d.rooms.get(room_id)))
        .or_else(|| self.telegram.as_ref().and_then(|t| t.rooms.get(room_id)))
}

pub fn room_profile_for(&self, room_id: &str) -> Option<(&str, &RoomProfileConfig)> {
    if let Some(profile) = self.room_block_for(room_id).and_then(|b| b.profile.as_deref())
        && let Some(rp) = self.room_profiles.get_key_value(profile)
    {
        return Some((profile, rp));
    }
    self.room_profiles.get_key_value(DEFAULT_PROFILE_NAME).map(|(k, v)| (k.as_str(), v))
}
```

`MatrixConfig::primary_room_id()` → `self.rooms.keys().next().map(String::as_str)`. `migration_errors`: one error per leftover list, worded `<channel>.<old key> is replaced by [<channel>.rooms."<room_id>"] blocks` / `room_profile.<n>.'rooms' is replaced by per-room blocks' profile keys` — follow the existing device-registry errors' exact wording style. `validate_profiles`: one error per block whose `profile` names an undefined room profile, naming the room id and the profile name. Update the `[autonomous] consult_room` doc comments and `config.example.toml`: `[matrix]`/`[discord]` rewritten to blocks (existing example room ids as keys, with `profile` set to what `[room_profile.<n>]` held before), and add a commented `[telegram]` section mirroring the `[discord]` one.

- [ ] **Step 4: Run the whole suite; fix existing tests to the new shape**

Run: `cargo test -p sapphire-agent-server -q`. Update existing config/channel tests that still assert the old shape (e.g. the matrix parse test asserting `room_ids` now asserts the migration error and a parsed block; keep one legacy-key round-trip test). The `a_room_resolves_to_the_same_profile_everywhere` style tests keep passing by name — that is the point.

- [ ] **Step 5: Commit**

```bash
git add crates/sapphire-agent-server/src/config.rs crates/sapphire-agent-server/Cargo.toml crates/sapphire-agent-server/config.example.toml
git commit -m "feat(config): per-room blocks keyed by room id; rooms now bind their room profile

Co-authored-by: Saphina <spph12@sapphire-turtle.org>"
```

---

### Task 2: telers dependency + TelegramChannel inbound

**Files:**
- Modify: `crates/sapphire-agent-server/Cargo.toml`
- Create: `crates/sapphire-agent-server/src/channel/telegram.rs`
- Modify: `crates/sapphire-agent-server/src/channel/mod.rs` (`pub mod telegram;`)
- Test: inline `mod tests` in `telegram.rs`

**Interfaces:**
- Consumes: `TelegramConfig`/`RoomBlock` (Task 1); shared attachment path (`prepare_image`, `sniff_image_mime`, `dropped_attachment_note`, `with_attachment_notes`, `MAX_DOWNLOAD_BYTES`, `SUPPORTED_IMAGE_MIME`).
- Produces: `TelegramChannel` (impls `Channel`; `name()` = `"telegram"`, `supports_replies()` = `true`); `route_update(&Message, &HashSet<String> /* allowed_users */, &IndexMap<String, RoomBlock>) -> Option<IncomingMessage>`; `room_id(chat_id: i64, thread: Option<i64>) -> String`; `split_room_id(&str) -> Result<(i64, Option<i64>)>`; backoff consts (min 1s, max 300s, stable 60s). Task 3 reads `split_room_id`'s split; Task 4's attachment task replaces `route_update`'s attachment placeholder.

- [ ] **Step 1: Verify telers handles forum topics (the gate)**

Read `telers` docs: incoming `Message` carries `message_thread_id` and `MessageOrigin::MessageThread`; `send_message` accepts `message_thread_id`; long-polled `get_updates` with `offset` ack exists. If `message_thread_id` is missing/unwired, stop and switch to `teloxide` with the identical shape (report the swap in the task report; nothing else in this plan changes).

- [ ] **Step 2: Write the failing tests**

```rust
#[tokio::test]
async fn an_inbound_topic_message_is_the_composite_room() {
    let rooms = rooms(&[("-1002000333444555:42", block(None, None, None))]);
    let msg = fixture_message(CHAT, Some(42), "1", "hello", Some("9")); // fixture: chat_id, message_thread_id, from, text, reply_to_message_id
    let incoming = route_update(&msg, &HashSet::new(), &rooms).await.unwrap();
    assert_eq!(incoming.room_id, "-1002000333444555:42");
    assert!(incoming.thread_id.is_none(), "the composite id is the room; no thread");
    assert_eq!(incoming.id, "500");
    assert_eq!(incoming.reply_to.as_deref(), Some("9"));
}
}

#[tokio::test]
async fn a_plain_chat_carries_no_colon_half() {
    let incoming = route_update(&fixture_message(CHAT, None, "1", "hi", None), &HashSet::new(), &IndexMap::new())
        .await
        .unwrap();
    assert_eq!(incoming.room_id, "-1002000333444555");
}

#[tokio::test]
async fn an_unlisted_room_or_user_is_ignored() {
    let rooms = rooms(&[("-1002000333444555:42", block(None, None, None))]);
    let missed = route_update(&fixture_message(-100999, Some(42), "1", "hi", None), &HashSet::new(), &rooms)
        .await;
    assert!(missed.is_none(), "rooms non-empty and the room is not a key");
    let empty = route_update(&fixture_message(CHAT, Some(42), "9", "hi", None), &HashSet::from(["1".to_string()]), &IndexMap::new())
        .await;
    assert!(empty.is_none(), "allowed_users non-empty miss");
}

#[tokio::test]
async fn an_empty_message_with_no_attachment_is_ignored() {
    assert!(route_update(&fixture_message(CHAT, Some(42), "1", "", None), &HashSet::new(), &IndexMap::new()).await.is_none());
#[test]
fn room_ids_split_back_into_their_halves() {
    assert_eq!(split_room_id("-1001").unwrap(), (-1001, None));
    assert_eq!(split_room_id("-1001:42").unwrap(), (-1001, Some(42)));
    assert!(split_room_id("-1001:x").is_err());
}
```

The `fixture_message`/`block`/`rooms` helpers build telers' real types from a minimal `serde_json::json!` (adapt the field names to telers' serde repr; verify with one `dbg_assert` round-trip before writing the others), mirroring discord.rs's `fn message(reference)` fixture style.

- [ ] **Step 3: Implement**

`Cargo.toml`: `telers = { version = "0", default-features = false }` (add features only if the crate demands them). `channel/telegram.rs` mirrors discord.rs's file layout (internal handler → channel struct → trait impl → helpers → tests). Struct:

```rust
pub struct TelegramChannel {
    token: String,
    rooms: IndexMap<String, RoomBlock>,
    allowed_users: HashSet<String>,
    /// Filled by `listen()` once the client exists; used by `send()`.
    client: Arc<OnceCell<Arc<telers::Client>>>,
}
impl TelegramChannel {
    pub fn new(cfg: &TelegramConfig) -> Result<Self> { /* every rooms key split_room_id-checks loudly: "Invalid Telegram room id {key}" */ }
}
```

`route_update` (async): `allowed_users` miss → `None` (debug! log wording as discord's); `rooms` non-empty and not a key → `None`; text = `with_attachment_notes(content.trim(), &[] )` for now (Task 4 replaces the placeholder with the real attachment path); empty text → `None`; then build `IncomingMessage { id, sender: from.id.to_string(), content, room_id: room_id(chat_id, message_thread_id), timestamp: date.unix_timestamp() as u64 * 1000, thread_id: None, attachments: vec![], reply_to: reply_target(&msg) }`. `reply_target` reads `reply_to_message_id`. Note in the module doc: Bot API `getUpdates` never echoes the bot's own messages, so there is no bot/self filter — unlike Discord/Matrix.

`listen()` (compiles and works from this task): clone `tx`, loop with `min/max/stable` backoff consts and the `Instant::elapsed()` reset logic copied from discord.rs; inside: `client.get_updates(&mut offset)` (long-poll, `allowed_updates = [Message]`), per update `if let Some(inc) = route_update(...).await { tx.send }`, ack `offset = msg_id + 1` unconditionally after handling (a closed agent queue warn-and-continues like discord.rs:72-73 and still advances — fixed in Task 2 fix round 1, commit c7dcd20). The `Channel::listen` body wires updates into `tx`.

- [ ] **Step 4: Run to verify they pass** — `cargo test -p sapphire-agent-server channel::telegram -q`; then `cargo test -p sapphire-agent-server -q` whole suite green.

- [ ] **Step 5: Commit** — `git commit -m "feat(channel): telegram channel — inbound topics as composite rooms ..."` (with the trailer).

---

### Task 3: Outbound send + typing

**Files:**
- Modify: `crates/sapphire-agent-server/src/channel/telegram.rs`
- Test: inline `mod tests` in `telegram.rs`

**Interfaces:**
- Consumes: `split_room_id` (Task 2).
- Produces: `split_for_telegram(&str) -> Vec<String>` (4096 **characters**, newline-preferred, char-boundary safe); `Channel::send` (first chunk carries `reply_to_message_id`; returns sent message ids); `start_typing` = `sendChatAction(typing)`, `stop_typing` no-op (expires on its own, like Discord); `room_info()` is configuration-sourced (the Bot API gives a bot no way to read a topic name, so unlike Matrix/Discord there is no live lookup): the room's `RoomBlock` renders into `RoomInfo` — `name` if set, else `description` as name with `description` shown, else the raw room id; `kind` is `"telegram"`.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn a_split_counts_characters_not_bytes() {
    // 3000 chars of 3-byte multibyte: 9000 bytes — one message for Telegram,
    // two for a bytes-counting impl.
    let one = "あ".repeat(3000);
    assert_eq!(split_for_telegram(&one).len(), 1);
    assert!(one.len() > 4096, "fixture is >4096 BYTES to catch the bytes bug");
}
#[test]
fn splits_prefer_a_newline_boundary() { /* 5000 chars, newline at 4000 → chunk 1 ends at it */ }

#[tokio::test]
async fn the_block_display_fields_render_into_room_info() {
    // name set → name wins; no name but description → description; neither → raw room id.
    let ch = channel_with(("-1001:42", block(Some("開発".into()), Some("自律セーョョン".into()), None)));
    let info = ch.room_info("-1001:42").await.unwrap();
    assert_eq!(info.name, "開発");
    assert_eq!(info.kind, "telegram");
}

#[tokio::test]
async fn a_room_seen_without_a_block_shows_its_raw_id() {
    // The map-empty-means-all-visible case: RoomInfo is built from the id alone.
    let ch = channel_with_all_visible();
    let info = ch.room_info("-1001:77").await.unwrap();
    assert_eq!(info.name, "-1001:77");
    assert_eq!(info.description, None);
}
```

- [ ] **Step 2: Run to verify they fail** (not defined).
- [ ] **Step 3: Implement** `split_for_telegram` (same shape as `split_for_discord`, but `LIMIT: usize = 4090` measured via `chars().count()`: track the byte offset by counting chars — `let end: Vec<char> = remaining.chars().take(LIMIT).collect()`-style or an explicit char→byte index walk; prefer the newline at `rfind('\n')` within range; `trim_start_matches('\n')` on the remainder), `send()` (split_room_id, `SendTextMessage`-style builder with `message_thread_id(thread)` when Some, `reply_to_message_id` on chunk 0 only, return ids), `start_typing`/`stop_typing`, and `room_info` (render the block per the display priority above; a room with no block at all renders `name = <raw room id>`).
- [ ] **Step 4: Run to verify they pass** — `cargo test -p sapphire-agent-server channel::telegram -q`.
- [ ] **Step 5: Commit** — "feat(channel): telegram outbound — 4096-character chunking, topic-aware send" + trailer.

---

### Task 4: Attachments + seed routing + wiring

**Files:**
- Modify: `crates/sapphire-agent-server/src/channel/telegram.rs`
- Modify: `crates/sapphire-agent-server/src/channel/mod.rs` (`seed_routing_from_config` + its doc comment)
- Modify: `crates/sapphire-agent-server/src/lib.rs` (channel_list + the `is_some()` gate; `consult_room_warnings`; the `verify` output lines; the heartbeat default-room wiring)
- Modify: `crates/sapphire-agent-server/src/channel/matrix.rs` + `discord.rs` (listen sets built from the rooms maps, not the legacy lists; heartbeat default = `primary_room_id()`)
- Modify: `crates/sapphire-agent-server/src/agent.rs` (session-history retention reads rooms maps)
- Modify: `crates/sapphire-agent-server/src/placement.rs` (host claim `Claim::serves` from rooms maps)
- Test: inline tests in each touched module

**Interfaces:**
- Consumes: `TelegramChannel` (Tasks 2-3); `room_block_for` / rooms maps / `primary_room_id()` (Task 1).
- Produces: `download_image_attachments(&Message) -> (Vec<Attachment>, Vec<String>)`; seed covers all three channels; every runtime reader that used to read `room_ids`/`channel_ids` now reads the per-room maps; the agent actually speaks Telegram end to end.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn a_telegram_image_over_the_download_limit_is_turned_away_before_it_is_fetched() {
    // screen: declared size > MAX_DOWNLOAD_BYTES says so without downloading;
    // 5.3 MB png downloads and is shrunk (reuse the noise_png fixture style).
}
#[tokio::test]
async fn seed_routing_covers_all_three_channels() {
    let cfg = parse("[matrix] homeserver = \"h\"\naccess_token = \"t\"\n[matrix.rooms.\"!a:x\"]\n\
             [discord]\nbot_token = \"t\"\n[discord.rooms.\"7\"]\n[telegram]\nbot_token = \"t\"\n\
             [telegram.rooms.\"-1001:42\"]\n");
    let seed = seed_routing_from_config(&cfg);
    assert_eq!(seed.get("!a:x").map(String::as_str), Some("matrix"));
    assert_eq!(seed.get("7").map(String::as_str), Some("discord"));
    assert_eq!(seed.get("-1001:42").map(String::as_str), Some("telegram"));
}
```

- [ ] **Step 2: Run to verify they fail.**
- [ ] **Step 3: Implement.** `download_image_attachments` mirroring discord's: screen by the declared mime/size (`only images (JPEG, PNG, GIF, WebP) are passed on` wording), download the photo, `spawn_blocking(prepare_image)` — the declared mime is never trusted, the bytes are sniffed; per-drop `dropped_attachment_note`. Replace Task 2's placeholder in `route_update` with the real path (attachments passed on + notes into `with_attachment_notes`; skip if text and attachments are both empty — the existing empty-text test still holds once the fixture carries none). `mod.rs`: `seed_routing_from_config` walks `config.matrix/discord/telegram` `rooms.keys()` (update the fn's doc: seeds from per-room blocks). `lib.rs`: gate becomes `matrix.is_some() || discord.is_some() || telegram.is_some()`; push `("telegram".to_string(), Arc::new(TelegramChannel::new(t).context("Failed to initialise Telegram channel")?))` after discord; `use crate::channel::telegram::TelegramChannel` next to the discord/matrix imports. **Remaining legacy readers (all switched to the per-room maps now that the old keys only exist to raise migration errors):** `matrix.rs`'s `MatrixChannel::new` listen set and `discord.rs`'s equivalent build from `rooms.keys()` (empty map = all visible); Matrix heartbeat default becomes `primary_room_id()`; `lib.rs`'s `consult_room_warnings` checks rooms-map membership across all three channels (including telegram); `lib.rs`'s heartbeat default wiring per channel uses `primary_room_id()`; `lib.rs`'s `verify` output lines print from the maps and reach telegram; `agent.rs`'s retention decision reads rooms keys; `placement.rs`'s host claim (`Claim::serves`) builds from rooms keys.
- [ ] **Step 4: Run the full suite** — `cargo test -p sapphire-agent-server -q`; also `cargo clippy -p sapphire-agent-server -D warnings` and `cargo build -p sapphire-agent-server` green (this is where the app-server startup log should show `Channels active: ..., telegram` in a manual smoke if the operator runs one — not required).
- [ ] **Step 5: Commit** — "feat(channel): telegram attachments and channel wiring; seed routing from per-room blocks" + trailer.

---

### Task 5: Example-config polish + README + closeout

**Files:**
- Modify: `crates/sapphire-agent-server/config.example.toml`
- Modify: `README.md` / `README.ja.md` (channel lists)

- [ ] **Step 1: Check every old-key mention is gone** — `grep -rn "room_ids\|channel_ids\|room_profile.*rooms" config.example.toml README*` → fix stragglers; the `[telegram]` example shows the three rooms (daily / dev / nsfw) as blocks with `name`/`profile`, and the credentials note (a `*_token` key is host-local, never in the workspace-shared config).
- [ ] **Step 2: Run docs-referenced tests** — full `cargo test -p sapphire-agent-server -q` one last time (evidence for the report: paste the tail line).
- [ ] **Step 3: Commit** — "docs: telegram and per-room blocks in the example config and READMEs" + trailer.
