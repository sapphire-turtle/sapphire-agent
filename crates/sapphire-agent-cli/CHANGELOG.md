# Changelog

All notable changes to `sapphire-agent-cli` will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Changes up to `0.8.0`, when this crate was a binary of its own, are recorded in
[`headless/CHANGELOG.md`](../../headless/CHANGELOG.md).

## [Unreleased]

## [0.13.2](https://github.com/sapphire-turtle/sapphire-agent/compare/sapphire-agent-cli-v0.13.1...sapphire-agent-cli-v0.13.2) - 2026-11-12



## [0.13.1](https://github.com/sapphire-turtle/sapphire-agent/compare/sapphire-agent-cli-v0.13.0...sapphire-agent-cli-v0.13.1) - 2026-11-09



## [0.13.0](https://github.com/sapphire-turtle/sapphire-agent/compare/sapphire-agent-cli-v0.12.1...sapphire-agent-cli-v0.13.0) - 2026-11-06

### Other

- [**breaking**] *(deps)* sapphire-framework 0.28, corundum-framework 0.11

## [0.12.1](https://github.com/sapphire-turtle/sapphire-agent/compare/sapphire-agent-cli-v0.12.0...sapphire-agent-cli-v0.12.1) - 2026-11-04

### Other

- sapphire-framework 0.27.1 and corundum 0.9.1 ([#365](https://github.com/sapphire-turtle/sapphire-agent/pull/365)); tokio 1.54.1, uuid 1.29.0, rmcp 3.8.0, bevy 0.20.1 and sherpa-onnx 2.0.1

## [0.12.0](https://github.com/sapphire-turtle/sapphire-agent/compare/v0.11.2...v0.12.0) - 2026-11-04

### Changed

- The command line of both sapphire-agent editions as a library: `run` takes the `Edition` it runs in and dispatches its own commands (`init`, `verify`, `acp`, `chat`) and sapphire-framework's verbs (`serve`, `status`, `workspace`, `api-key`, …). Published as a binary crate up to 0.8.0, it resumes at 0.12.0 with the editions' version; see [`headless/CHANGELOG.md`](../../headless/CHANGELOG.md) for the user-visible changes
- Built on sapphire-framework 0.26 and sapphire-agent-core 0.11
