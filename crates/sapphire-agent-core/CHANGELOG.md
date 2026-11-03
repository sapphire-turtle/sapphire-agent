# Changelog

All notable changes to `sapphire-agent-core` will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Historical changes prior to `0.7.0` (when this crate shared the workspace
version with `sapphire-agent-server`) are recorded in the root `CHANGELOG.md`.

## [Unreleased]

## [0.11.0](https://github.com/sapphire-turtle/sapphire-agent/compare/sapphire-agent-core-v0.10.0...sapphire-agent-core-v0.11.0) - 2026-11-04

### Changed

- [**breaking**] Built on sapphire-framework 0.26 and sapphire-framework-server-api 6 (from 0.22 and 5): `ChatClient::from_client` takes the framework 0.26 `ipc::Client`, and the handshake names server API 6, so this client talks to sapphire-agent 0.12 and later
- [**breaking**] `ChatClient::send` takes a `Turn` (the text, its `attachments` and the client's `ClientCaps`; `Turn::text` for a plain message) and hands each `ChatEvent` (a tool call as `ChatEvent::Tool(ToolEvent)`, or an avatar cue) to its callback. `APP_API` is 3
- [**breaking**] `AvatarConfig` splits into an optional `[vrm]` section (`VrmConfig`: `model`, `idle`, `actions`) and an optional `[sprite]` section (`SpriteConfig`: `default_pose` and `poses`, each a `SpritePose` with a `base` image and an image per expression); the old top-level `model` / `idle` / `actions` form is no longer read. `AvatarConfig::paths` lists every file it references and `AvatarConfig::check` validates it

### Added

- Client capabilities and attachments per turn: `ClientCaps` with `AvatarCaps` (the `expressions`, `actions` and `poses` the client's avatar can play) and `Attachment` (`Attachment::from_bytes`), sent with `chat.send`
- Avatar cues streamed while a turn runs: `ChatEvent::Expression`, `ChatEvent::Action` and `ChatEvent::Pose` (`KIND_EXPRESSION`, `KIND_ACTION`, `KIND_POSE`), read by `parse_progress`
- The avatar's files in the workspace's `avatar/` folder, over IPC: `ChatClient::avatar_manifest`, `avatar_download`, `avatar_upload`, `avatar_set_config` and `avatar_remove` (`AVATAR_MANIFEST`, `AVATAR_READ`, `AVATAR_WRITE_*`, `AVATAR_SET_MANIFEST`, `AVATAR_REMOVE`), with `AvatarManifest`, `AvatarFile`, `AVATAR_CHUNK` and `AVATAR_MAX_FILE`
- `SPRITE_EXPRESSIONS`: the expressions a sprite pose may have an image for (happy, angry, sad, relaxed, surprised)

## [0.10.0](https://github.com/sapphire-turtle/sapphire-agent/compare/sapphire-agent-core-v0.9.0...sapphire-agent-core-v0.10.0) - 2026-10-26

### Changed

- [**breaking**] Built on sapphire-framework 0.22 and sapphire-framework-server-api 5



## [0.9.0](https://github.com/sapphire-turtle/sapphire-agent/compare/sapphire-agent-core-v0.8.0...sapphire-agent-core-v0.9.0) - 2026-10-15

### Added

- [**breaking**] *(desktop)* Talk to the local agent over IPC, like `sapphire-agent chat`

### Fixed

- Clear the RustSec advisories cargo-deny reports

### Other

- *(license)* Desktop under GPL-3.0-or-later; license files per component



## [0.8.0] - 2026-09-07

### Changed

- **Renamed from `sapphire-agent-rpc` to `sapphire-agent-core`** — the crate is the shared client

server module crate; the RPC-specific name would mislead once the agent loop is shared with the client side. crates.io history stays under `sapphire-agent-rpc` through 0.7.1.

## [0.7.0] - 2026-05-23

Aligns the crate version with `sapphire-agent-server` 0.7.0. `0.6.1` was an
unintended interim publication produced by the first release-plz run
before this crate had its own deliberate release; the source between
`0.6.1` and `0.7.0` is identical apart from the version bumps and
path-dep updates.

### Added

- **Chat audio output modality** — the `chat` JSON-RPC method now
  accepts a `modalities` parameter with `["text", "audio"]`. When
  `audio` is requested, the SSE stream emits server-side TTS frames
  (base64-encoded PCM) alongside the text response, so a single chat
  request can drive both the chat UI and the speakers. Text-only
  clients are unchanged. (#127)

### Changed

- **Crate renamed from `sapphire-agent-server-api` to `sapphire-agent-core`** to
  match the `/rpc` endpoint it talks to. Clients depending on the old
  crate name should update their `Cargo.toml`; the type surface is the
  same. The session-kind directory on the server side also moves from
  `sessions/<ns>/api/` to `sessions/<ns>/rpc/`, with a dual-read shim
  keeping legacy files visible until the bundled migration runs. The
  `api_keys` field on the server's `[room_profile.<n>]` config is
  deliberately unchanged — it gates `/rpc`, `/mcp`, and `/a2a`
  together. (#112)
