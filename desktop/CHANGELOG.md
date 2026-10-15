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

## [0.8.1](https://github.com/sapphire-turtle/sapphire-agent/compare/desktop-v0.8.0...desktop-v0.8.1) - 2026-10-15

### Other

- Update Cargo.lock dependencies


### Changed

- **Renamed from `sapphire-call-desktop` to `sapphire-agent-desktop`**
  (crate, binary, and GitHub release asset names). Version aligned
  with the workspace at 0.8.0 (was 0.1.0; the pre-release 0.6.x numbers
  predate any user-facing release of this binary).
