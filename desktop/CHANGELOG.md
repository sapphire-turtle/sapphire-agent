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
