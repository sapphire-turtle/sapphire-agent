# Changelog

All notable changes to `sapphire-agent-cli` will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

This crate is the continuation of the former `sapphire-call` crate
(published as `sapphire-call` on crates.io up through `0.6.1`). The
binary it ships is now `sapphire-agent-cli` as well (the binary was still named `sapphire-call` through 0.7.x). Historical entries describing the voice-satellite pipeline,
wake / VAD / TTS plumbing, and earlier feature work live in the root
`CHANGELOG.md`.

## [Unreleased]

## [0.8.1](https://github.com/sapphire-turtle/sapphire-agent/compare/sapphire-agent-cli-v0.8.0...sapphire-agent-cli-v0.8.1) - 2026-09-24

### Changed

- *(audio)* Remove redundant clone on Copy SupportedStreamConfig

### Fixed

- *(audio)* Adapt to cpal 0.18 API changes



## [0.8.0] - 2026-09-07

### Changed

- **Renamed from `sapphire-call-cli` to `sapphire-agent-cli`**, and the shipped binary from `sapphire-call` to `sapphire-agent-cli`.
- **XDG config

data dirs moved**: `~

config

sapphire-agent-cli

` and `~

local

share

sapphire-agent-cli

` (env overrides SAPPHIRE_AGENT_CLI_DEVICE_ID_PATH 

 SAPPHIRE_AGENT_CLI_CACHE_DIR); existing installs must move their config

device-id file or set the env var, otherwise the device id is re-minted on first run.

## [0.7.0] - 2026-05-23

First release under the new crate name. The version is aligned with
`sapphire-agent` 0.7.0 so the workspace versions are easy to read at a
glance; future bumps will track this crate's own change cadence.

### Changed

- **Renamed from `sapphire-call` to `sapphire-call-cli`** as part of the
  workspace split that introduced `sapphire-call-desktop`. The binary
  produced by `cargo install sapphire-call-cli` is still
  `sapphire-call`, so existing scripts and `sapphire-call voice ...`
  invocations are unaffected. Users installing from crates.io should
  switch from `cargo install sapphire-call` to
  `cargo install sapphire-call-cli`.
- **Shared config + device-id helpers extracted to `sapphire-call-core`.**
  The CLI now depends on `sapphire-call-core` for `ServerConfig` and
  per-installation `device_id` resolution, so adding new client targets
  (e.g. the desktop GUI) doesn't fork those types. The on-disk config
  format and path (`~/.config/sapphire-call/config.toml`) are
  unchanged.
