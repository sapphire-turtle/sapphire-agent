# Changelog

All notable changes to `sapphire-agent-server` will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Changes up to `0.8.0`, when this crate was a binary of its own, are recorded in
[`headless/CHANGELOG.md`](../../headless/CHANGELOG.md).

## [Unreleased]

## [0.13.2](https://github.com/sapphire-turtle/sapphire-agent/compare/sapphire-agent-server-v0.13.1...sapphire-agent-server-v0.13.2) - 2026-11-12

### Added

- *(mcp)* An rmcp transport that carries MCP over ACP mcp/message
- *(mcp)* Run McpClient over ACP and classify its tools by annotations
- *(acp)* Offer and run per-session tools in a turn
- *(acp)* Accept client MCP servers over ACP (mcp/message)
- *(acp)* Keep tool-list subscriptions alive; make client tool names collision-free
- *(channel)* Cache attachment originals for seven days
- *(tools)* Read local image files as multimodal results
- *(tools)* Save conversation-owned attachment originals
- *(channel)* Receive and cache non-image attachments
- *(agent)* Scope attachment saving to channel conversations

### Documentation

- *(acp)* Document tools a client provides over MCP over ACP
- *(tools)* Document attachment retention and image reading

### Fixed

- *(acp)* Wait for client MCP tools only before the first turn, install them once
- *(channel)* Shrink oversized images and tell the model about dropped attachments
- *(tools)* Make the image_read tilde check cross-platform
- *(channel)* Scope the lossless-root test to the platform that can create it

### Testing

- *(acp)* Round-trip a client-provided MCP tool
- *(acp)* Wait for the initialize reply before sending the next request
- *(acp)* Wait for the initialize reply in the load-then-prompt test too
- *(provider)* Pin image tool-result serialization



## [0.13.1](https://github.com/sapphire-turtle/sapphire-agent/compare/sapphire-agent-server-v0.13.0...sapphire-agent-server-v0.13.1) - 2026-11-09

### Added

- *(tools)* Background tasks and background subagents ([#380](https://github.com/sapphire-turtle/sapphire-agent/pull/380))
- *(autonomous)* A consult room where autonomous sessions ask a person (ask_human) ([#381](https://github.com/sapphire-turtle/sapphire-agent/pull/381))
- *(workspace)* Init seeds a suggested character, CHARACTER.md ([#382](https://github.com/sapphire-turtle/sapphire-agent/pull/382))
- The first conversation introduces the default character ([#383](https://github.com/sapphire-turtle/sapphire-agent/pull/383))
- *(tools)* Run shell commands in the background ([#384](https://github.com/sapphire-turtle/sapphire-agent/pull/384))
- *(api)* Reasoning_effort setting for openai_compatible providers



## [0.13.0](https://github.com/sapphire-turtle/sapphire-agent/compare/sapphire-agent-server-v0.12.1...sapphire-agent-server-v0.13.0) - 2026-11-06

### Added

- *(workspace)* Download and install the default avatar ([#340](https://github.com/sapphire-turtle/sapphire-agent/pull/340)) ([#370](https://github.com/sapphire-turtle/sapphire-agent/pull/370))
- Install the default avatar when the GUI finds none ([#373](https://github.com/sapphire-turtle/sapphire-agent/pull/373))

### Other

- [**breaking**] *(deps)* sapphire-framework 0.28, corundum-framework 0.11

## [0.12.1](https://github.com/sapphire-turtle/sapphire-agent/compare/sapphire-agent-server-v0.12.0...sapphire-agent-server-v0.12.1) - 2026-11-04

### Other

- sapphire-framework 0.27.1 and corundum 0.9.1 ([#365](https://github.com/sapphire-turtle/sapphire-agent/pull/365)); tokio 1.54.1, uuid 1.29.0, rmcp 3.8.0, bevy 0.20.1 and sherpa-onnx 2.0.1

## [0.12.0](https://github.com/sapphire-turtle/sapphire-agent/compare/v0.11.2...v0.12.0) - 2026-11-04

### Changed

- The agent's app server as a library, which both sapphire-agent editions run. Published as a binary crate up to 0.8.0, it resumes at 0.12.0 with the editions' version; see [`headless/CHANGELOG.md`](../../headless/CHANGELOG.md) for the user-visible changes (API keys under room profiles, avatar tools and files, image attachments)
- Built on sapphire-framework 0.26 (server API 6) and sapphire-agent-core 0.11
