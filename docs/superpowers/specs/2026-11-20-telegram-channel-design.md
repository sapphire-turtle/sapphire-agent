# Telegram Channel Design

Date: 2026-11-20
Status: approved-in-chat (pending written-spec review)
Branch: feat/telegram-channel

## Summary

Add `telegram` as a third `Channel` implementation beside `matrix` and `discord`, and — in the same change — move per-room configuration out of `[room_profile.<n>].rooms` and into per-room blocks under each channel section (`[telegram.rooms."<room_id>"]` etc.), so that a room carries its own display name, description and room-profile binding.

## Motivation

The Telegram Bot API is the third chat surface Fumito wants the agent present on. Rooms are one dedicated group each (daily chat, development / autonomous sessions, NSFW), plus topics inside those groups. Unlike Matrix, the Bot API gives a bot no way to read a topic's name, so the per-room display metadata that Matrix and Discord get from room state must come from configuration instead.

## Decisions

### 1. Rooms are composite room ids; the channel never materialises a thread

A Telegram room id is `<chat_id>` for a plain chat (DM or general group, no topic) and `<chat_id>:<message_thread_id>` for a message sent to a forum topic. `IncomingMessage.room_id` carries that composite id verbatim and `thread_id` is always `None` — the composite is the room, exactly like a Matrix room id, and everything downstream (`(room_id, thread_id)` conversation keys, routing, room profiles) already keys on `room_id` alone.

Topics are not auto-materialised into rooms the way a Matrix DM is: a topic becomes a room because it appears in `[telegram.rooms]` (decision 2), not because a message arrived with a new `message_thread_id`. The "one autonomous session per task" blowup this would otherwise cause is the reason for this whole shape.

### 2. Per-room blocks under each channel section

Each channel config gains a `rooms` map of per-room blocks, keyed by room id (TOML quoted dotted keys):

```toml
[telegram]
bot_token = "123456:ABC..."
allowed_users = ["11111111"]

# development / autonomous-session group, dev topic
[telegram.rooms."-1002000333444555:42"]
name = "開発"
description = "開発と自律セッションの部屋"
profile = "dev"

# daily chat: a general group, messages carry no message_thread_id
[telegram.rooms."-100200099988877"]
name = "日常"
profile = "casual"
```

Block fields:

- `name` (optional) — display name for `RoomInfo`.
- `description` (optional) — free-form room description, same role as Matrix's room topic / Discord's channel topic.
- `profile` (optional) — the name of a `[room_profile.<n>]` bundle this room runs under.

`[room_profile.<n>]` keeps bundling `(profile, memory_namespace, session_policy)` and loses its `rooms` reverse-list: rooms now name the bundle they belong to, rather than the bundle listing its rooms. API sessions already bind a room-profile by name and are unaffected.

`profile` values must name a defined room profile; `validate` gains an error per block whose `profile` names none.

### 3. Migration of the old keys

`[room_profile.<n>].rooms`, `[matrix].room_ids` / legacy `room_id`, and `[discord].channel_ids` are all removed. Like `api_keys → devices` before them, each is retained as a plain field only so `Config::migration_errors` names any leftover setting loudly; no migration is performed silently. `deserialize_room_ids` goes with them.

### 4. Resolution order

`room_profile_for` becomes: an explicit `[<channel>.rooms.<id>]` block's `profile` key > `[room_profile.default]` > `None` (implicit built-in defaults, unchanged). `profile_for`, `namespace_for_room` and `session_policy_for` keep their logic and fall through unchanged for a room with no block or no `profile` key. A room appearing in no block and matched by no `[room_profile.default]` behaves exactly as today.

### 5. The rooms map doubles as the listen list

A channel listens in every room id named in its `rooms` map, plus — when the map is empty — everything the bot can see. This preserves Discord's existing "empty = all visible channels", gives Telegram the same default, and relaxes Matrix from "exactly these rooms" to the same rule (an empty `room_ids` today listens to nothing useful). Routing seeding (`seed_routing_from_config`) walks the three maps and seeds each id → channel name exactly as today. `[autonomous] consult_room`'s doc comment is updated to the new key shape; the setting itself is unchanged.

### 6. `room_info()` for Telegram is configuration-sourced

The Bot API gives a bot no way to read a forum topic's name, so Telegram has no live lookup: `TelegramChannel::room_info()` renders `RoomInfo` from the room's config block — `name` if set, else `description` if set, else the raw room id; `kind` is `"telegram"`. Matrix and Discord keep their live lookups and are untouched. Upstream (`agent.rs`, `workspace.rs::render_room_info`) consumes the same `RoomInfo` and does not change.

### 7. Dependency: `telers`

`telers = { version = "...", default-features = false }` (tokio-native, listed in the official Bot API Library Examples). Implementation-time check, in this order: (a) incoming `Message` exposes `message_thread_id` and `send_message` accepts it; (b) `getUpdates` long-poll and `offset` ack are exposed. If `telers` turns out not to handle forum topics, fall back to `teloxide` with the same shape; the channel code's surface is identical either way.

### 8. Wire details

- **Inbound**: ignore bots and service messages; filter `from.id` against `allowed_users` (empty = all); ignore a room not in the (non-empty) `rooms` map; room id = `chat_id` [+ `:` `message_thread_id`]; `reply_to` from `reply_to_message_id`; skip empty text with no usable attachment; then the shared attachment path below.
- **Attachments**: images only, same gate as Discord/Matrix — screen by declared size against `MAX_DOWNLOAD_BYTES`, download, `sniff_image_mime` the bytes (Telegram's declared mime type is not trusted either), `prepare_image` shrinks over `MAX_ATTACHMENT_BYTES`, per-drop `dropped_attachment_note` via `with_attachment_notes`.
- **Outbound**: split the composite room id on `:` into `chat_id` and `message_thread_id`; chunk at Telegram's 4096 **characters** (`chars().count()`, not bytes — `split_for_discord` counts bytes today), preferring a newline boundary; first chunk carries `reply_to_message_id` when set.
- **Transport**: long-polled `getUpdates` with `offset` ack; on exit, reconnect with the same exponential backoff as Discord (1s → 300s, reset after 60s stable).
- **Typing**: `sendChatAction(typing)`; `stop_typing` is a no-op (expires on its own), like Discord.
- `name()` is `"telegram"`; `supports_replies()` is `true`.

### 9. Tests (offline, no live Telegram)

Unit tests in the existing per-channel style:

- composite room id round-trips: inbound `chat_id`/`message_thread_id` → `room_id`, and outbound splits back correctly (plain chat has no `:` half);
- `allowed_users` and unlisted-room filtering;
- 4096-**character** splitting at a newline boundary with multibyte content (a bytes-counting implementation fails this);
- attachment screening and drop-note wording (reuse shared helpers, assert Telegram-specific wording);
- `reply_target` from `reply_to_message_id`;
- config: a block's `profile` wins over `[room_profile.default]`, an absent `profile` falls through as today; leftover `rooms` / `room_ids` / `channel_ids` keys are migration errors; a block whose `profile` names no room profile is a validation error;
- `seed_routing_from_config` seeds from the three channel maps;
- `room_info` display priority: `name` > `description` > raw room id;
- backoff constants mirror Discord.

## Out of scope

- Voice pipelines for Telegram.
- Sub-comments, message reactions, stickers.
- Creating the actual groups and topics (operator side, by hand); this spec only makes the agent speak Telegram.
- Any change to how autonomous sessions bind `room_id == task.name`.

## Files touched

- `crates/sapphire-agent-server/src/channel/telegram.rs` (new)
- `crates/sapphire-agent-server/src/channel/mod.rs` (`pub mod telegram;`, seed walks maps)
- `crates/sapphire-agent-server/src/config.rs` (`TelegramConfig`, per-channel `rooms` maps, resolver changes, migration errors)
- `crates/sapphire-agent-server/src/lib.rs` (push `("telegram", ...)` onto `channel_list`)
- `crates/sapphire-agent-server/Cargo.toml`, `config.example.toml` (new `[telegram]` section; `[matrix]` / `[discord]` examples rewritten to blocks)
