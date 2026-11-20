# Changelog

All notable changes to `sapphire-agent-server` will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Changes up to `0.8.0`, when this crate was a binary of its own, are recorded in
[`headless/CHANGELOG.md`](../../headless/CHANGELOG.md).

## [Unreleased]

## [0.14.0](https://github.com/sapphire-turtle/sapphire-agent/compare/sapphire-agent-server-v0.13.1...sapphire-agent-server-v0.14.0) - 2026-11-20

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
- *(config)* Add ToolGroup and the session/permission profile types
- [**breaking**] *(tools)* Register by group, guard the definitions on the opened path
- Let an unattended task declare its session profile
- *(config)* Add per-room channel blocks
- *(channel)* Telegram inbound topics as composite rooms
- *(channel)* Telegram outbound — 4096-character chunking, topic-aware send
- *(channel)* Telegram attachments and channel wiring; seed routing from per-room blocks

### Changed

- [**breaking**] *(config)* Bind session profiles on the declaration side
- [**breaking**] *(policy)* Carry group and kind on the spec, decide with allow
- [**breaking**] Let both turn loops decide through allow
- Times are jiff's, not chrono's (RUSTSEC-2026-0543)

### Documentation

- *(acp)* Document tools a client provides over MCP over ACP
- *(tools)* Document attachment retention and image reading
- Telegram and per-room blocks in the example config and READMEs

### Fixed

- *(acp)* Wait for client MCP tools only before the first turn, install them once
- *(channel)* Shrink oversized images and tell the model about dropped attachments
- *(tools)* Make the image_read tilde check cross-platform
- *(channel)* Scope the lossless-root test to the platform that can create it
- *(config)* Preserve admin profile key and clarify migration
- *(config)* Restore the duplicate room binding guard
- *(policy)* Restore the lost coverage and kill the second kind source
- Keep the room binding governing ordinary channel turns
- Complete task 6 end-to-end coverage
- The unattended turn's one resolved profile governs everything it touches
- *(config)* Preserve room order and reject cross-channel duplicates
- *(channel)* Validate Telegram tokens and advance past closed queues
- *(channel)* Route Telegram media captions with drop notes
- Carry telers dependency policy and validate Telegram user ids

### Other

- Clean the accumulated review residue

### Testing

- *(acp)* Round-trip a client-provided MCP tool
- *(acp)* Wait for the initialize reply before sending the next request
- *(acp)* Wait for the initialize reply in the load-then-prompt test too
- *(provider)* Pin image tool-result serialization



### Changed

- Times are jiff's, not chrono's, which is unmaintained (RUSTSEC-2026-0543). Session logs, transcripts and caches read and write as before (RFC 3339). chrono is left only where a dependency's API takes it: the `cron` crate's schedules (`HeartbeatTask::next_after` converts at the edge) and the A2A message timestamp.
- sapphire-framework 0.29, corundum-framework 0.13.

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
