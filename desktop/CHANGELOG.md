# Changelog

All notable changes to `sapphire-agent-desktop` will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

This crate is `publish = false`: the desktop client ships as a
pre-built binary attached to each `sapphire-agent-desktop-v*` GitHub
release rather than via `cargo install`. Versioning starts at 0.1.0
even though earlier development commits carried 0.6.x — those numbers
were inherited from the workspace at the pre-release scaffolding stage
and predate any user-facing release of this binary.

## [Unreleased]

## [0.13.0](https://github.com/sapphire-turtle/sapphire-agent/compare/v0.12.1...v0.13.0) - 2026-11-06

### Changed

- [**breaking**] Built on sapphire-framework 0.28 and corundum-framework 0.11: `update` installs only a release file signed with sapphire-turtle's key, and refuses one without a valid signature (sapphire-turtle/sapphire-framework#402)
- [**breaking**] Built on sapphire-framework 0.24 (server API 6, bridge API 4) and corundum-framework 0.7. Update `sapphire-bridge` together with this release: a 0.24 bridge no longer checks the agent's tokens, and the agent no longer asks it
- [**breaking**] Bearer tokens (`/rpc`, `/a2a`, `/acp`, ambient ingest) are the served workspace's API keys, kept in `<root>/.sapphire-agent/api_keys/` and synced with it: `sapphire-agent external-device` is now `sapphire-agent api-key` (the old name is a hidden alias for this release, without `--app`). The workgroup's external devices are imported once, keeping their tokens
- [**breaking**] An API key's profile is the room profile it runs under: `api-key add <name> --profile <room profile>` (required), or Settings › API keys. Keys imported from external devices have the profile `read-only` and are still routed by `[room_profile.<n>].devices` until re-issued with a room profile; a key whose profile is no room profile is refused with 403 (no more 503: no bridge is asked). `verify` lists every key and the room profile it runs under
- [**breaking**] The `workgroup`, `device` and `embedding` commands are gone: use `sapphire-bridge workgroup|device|embedding`

### Added

- The desktop companion: the window is now a transparent, borderless, always-on-top companion holding the character, with replies in a speech bubble that fades after a time fitting their length. Clicks outside the character and its panels go through to the desktop (X11, Windows and macOS; not on Wayland). Drag the character to move it; its place is remembered
- Gestures on the character: click for the input box, scroll for the history, right-click for the menu (settings, reconnecting, quitting), Esc to close them. Enter opens the input box (Ctrl+Enter while a panel is open), and a setting shows a button for each
- The Character settings page: the display mode (Auto, 3D or 2D), how long an expression or pose holds after a reply, the gesture buttons, and a section per renderer for the avatar's files
- The avatar's files are synced from the agent into a local cache: on connecting, after an upload from this device, and every five minutes. Until a renderer can draw it, a silhouette stands in and the bubble says why
- The 2D avatar: the character can be drawn from still images (PNG or WebP) in named poses, each with a neutral image and one per expression, crossfading between them. The Character page's 2D section adds, renames and deletes poses and uploads their images, which must all share one size

### Fixed

- No longer panics on the first frame (the chat borrowed egui's root `Ui`, which the egui pass already holds)

## [0.8.2](https://github.com/sapphire-turtle/sapphire-agent/compare/desktop-v0.8.1...desktop-v0.8.2) - 2026-10-26

### Changed

- Built on sapphire-agent-core 0.10 (sapphire-framework 0.22, server API 5): use it with sapphire-agent 0.11

### Added

- Let cargo binstall install the release binaries



## [0.8.1](https://github.com/sapphire-turtle/sapphire-agent/compare/desktop-v0.8.0...desktop-v0.8.1) - 2026-10-15

### Other

- Update Cargo.lock dependencies


### Changed

- **Renamed from `sapphire-call-desktop` to `sapphire-agent-desktop`**
  (crate, binary, and GitHub release asset names). Version aligned
  with the workspace at 0.8.0 (was 0.1.0; the pre-release 0.6.x numbers
  predate any user-facing release of this binary).
