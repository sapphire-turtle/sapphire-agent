# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.8.0](https://github.com/fluo10/sapphire-agent/compare/sapphire-agent-v0.7.2...sapphire-agent-server-v0.8.0) - 2026-09-07

### Changed


- **Package + binary rename** — the server crate and binary are now `sapphire-agent-server` (was `sapphire-agent`). First release published to crates.io under the new name; history before this lives under `sapphire-agent` through 0.7.2.


### Breaking

- **`standby_mode` removed** — its only purpose was git sync on a backup
  node, and the framework no longer ships local-workspace auto-sync. An
  existing config that still sets it now **fails at startup** rather than
  silently starting a second active agent — delete the line to upgrade.
- **`[sync]` config section removed.** `sync_interval_minutes` stays and
  is now purely the workspace re-index cadence; nothing is committed or
  pushed on that tick.
- **Sessions are no longer carried between devices.** Keep the workspace
  on an external sync service if you need that.
- **`[room_profile.<n>].api_keys` removed** — plaintext bearer tokens no
  longer live in config.toml. Replaced by `[room_profile.<n>].devices`,
  a list of device ids from the workspace's `devices.toml`. An existing
  config that still sets `api_keys` fails at startup; run `sapphire-agent
  device add --name <device>` for each client and list the printed ids
  under `devices` instead.
- **`[device.<n>]` removed** — ambient ingest's hand-written key-id blocks
  are replaced by the same workspace device table used by every other
  entry point. An existing config that still sets one fails at startup;
  run `sapphire-agent device add --name <name>` to re-register the
  device (the old token cannot be carried over).
- **A usable key file is now mandatory at startup**, not just when
  ambient ingest is enabled — every authenticated endpoint (`/rpc`,
  `/a2a`, `/acp`, `/mcp`, ambient ingest) now resolves through the same
  `DeviceAuth`. A Matrix-only or fresh install that never needed a key
  file before must now run `sapphire-agent device add --name <device>`
  at least once before the agent will start.

## [0.7.2](https://github.com/fluo10/sapphire-agent/compare/sapphire-agent-v0.7.1...sapphire-agent-v0.7.2) - 2026-05-24

### Documentation

- Add CLAUDE.md with conventional-commit scope + release flow rules

### Fixed

- Bump sapphire-workspace to 0.12.1



## [0.7.1](https://github.com/fluo10/sapphire-agent/compare/sapphire-agent-v0.7.0...sapphire-agent-v0.7.1) - 2026-05-23

### Other

- *(release)* add sapphire-call-core and -cli to release-plz at 0.7.0

## [0.7.0](https://github.com/fluo10/sapphire-agent/compare/sapphire-agent-v0.6.1...sapphire-agent-v0.7.0) - 2026-05-23

### Added

- **Device-default sessions** — a new session kind keyed by
  `(device_id, room_profile)` that voice satellites and heartbeat
  pushes route into when no other session is selected. Stored under
  `sessions/<ns>/device-default/<uuid>.jsonl`; lazy-created (no file
  appears until a satellite is actually used); daily-rotated by the
  "most-recent-from-today" lookup, so a satellite that connects after
  the local-day boundary lands in a fresh session automatically. The
  previous deterministic `voice-<sha256>.jsonl` scheme is retired —
  voice traffic now flows through the same `SessionStore` surface as
  every other kind. (#122)
- **User-message metadata: input modality + user_id** — `StoredMessage`
  / `ChatMessage` gain optional `input_kind` (`Text` / `Voice`) and
  `user_id` fields. Voice transcripts get a `[voice input]` English
  prefix right before the provider call so the model knows the body
  may carry STT errors. Voice-print speaker variants
  (`KnownVoice` / `UnknownVoice`) are deferred — adding them later
  stays source-compatible with existing `{"kind":"voice"}` JSONL.
  `user_id` is a placeholder for the future per-user profile mapping
  under `<workspace>/users/<namespace>/<user_id>.md`; currently always
  `None`. Both fields are `serde(default, skip_serializing_if =
  "Option::is_none")` so legacy JSONL parses cleanly and channel
  sessions write the exact same bytes as before. (#127)
- **`recall_image` tool + on-demand historical image recall** —
  `hydrate_history` now degrades past `ImageRef` parts to a stable
  `[image: <media_type> sha256=<hex>]` text marker instead of
  re-inflating the bytes on every turn, dropping input-token cost on
  long sessions with attachments. The bytes still live in the
  workspace-external image cache, addressable by SHA-256; the new
  `recall_image(sha256, media_type)` tool (registered only when the
  cache is enabled) fetches a specific past image on demand and
  Anthropic accepts the image as a sibling content block to the
  `tool_result`. The `Tool` trait gains `execute_full` with a default
  impl that wraps the existing `execute`, so the ~20 existing tools
  need no changes. (#119)

### Changed

- **`sapphire-agent-api` crate renamed to `sapphire-agent-rpc`** to match
  the `/rpc` endpoint it talks to. The session-kind directory it persists
  to also moves: `sessions/<ns>/api/` → `sessions/<ns>/rpc/`. Until the
  bundled session migration runs (separate PR), readers transparently
  fall through to the legacy `api/` directory so existing files remain
  visible. The `api_keys` config field on `[room_profile.<n>]` is
  deliberately unchanged — it gates `/rpc`, `/mcp`, and `/a2a` together
  and the broad "api" name still fits. (#112)
- **`ServeState::rpc_session_store` → `cross_device_session_store`** —
  user-selectable, multi-device sessions resumed via `--resume
  <grain-id>` are now logically separated from device-default sessions.
  On-disk directory moves to `sessions/<ns>/cross-device/`; the bundled
  session migration relocates pre-existing files. (#122)
- **`voice-sherpa` feature promoted to default.** Local sherpa-onnx
  STT + TTS are now part of the default feature set so voice works
  out of the box. Note the longer cold-cache build (~5–10 min for
  `sherpa-onnx-sys`); opt out via `--no-default-features` plus the
  other defaults if you only run the Gradio TTS path or don't need
  voice at all. (#120)

### Migration

- **One-shot session-store migration** runs on first startup of this
  version, completing both #112 and #122:
  - `sessions/<ns>/api/<uuid>.jsonl` and `sessions/<ns>/rpc/<uuid>.jsonl`
    cross-device files move to `sessions/<ns>/cross-device/<uuid>.jsonl`.
    Stored `meta.channel = "api"` is rewritten to `"rpc"` along the way.
  - Pre-redesign voice files (`voice-<hash>.jsonl`) are quarantined to
    `sessions/<ns>/legacy-voice/`. Their `(device_id, room_profile)`
    routing key was hashed into the filename and is unrecoverable; the
    daily-log archive still indexes their content so nothing is lost
    for FTS / semantic search, only the live conversation continuation.
  - The PR-1 dual-read shim and dual-accept `"api"`/`"rpc"` fallbacks
    are removed after this migration runs.
  - Idempotent and safe to interrupt. No backup is taken — the
    workspace is expected to be under git.

## [0.6.1] - 2026-05-18

### Fixed

- **Timer fire heartbeat now reliably delivers a notification.** When a
  preset step (or single timer) fired, the timer task awaited
  `agent.trigger` inline; an AI `timer_set` call during that reply
  caused `replace_active` → `abort` on the very task driving the
  heartbeat, dropping the chat loop mid tool-round so the matching
  `tool_result` never landed in history. The next turn then hit
  Anthropic `400: tool_use ids were found without tool_result blocks
  immediately after` and the notification never reached the channel.
  `TimerManager::dispatch_fire` is now sync-spawning detached so the
  trigger runs outside the timer task. (#116)
- **Preset auto-chain prompt** — the intermediate-step heartbeat now
  states the next step is already scheduled and forbids
  `timer_set` / `timer_preset` re-invocation, to keep the AI from
  racing the state machine. (#116)
- **`timer_*` tool descriptions** — all four tools now make explicit
  that the timer state is agent-private and the AI must always notify
  the user via text on set / cancel / fire; tool-only replies are
  silent to the user. (#116)
- **`Agent::persist`** strips `ToolUse` / `ToolResult` parts in place
  instead of dropping the whole message, so the assistant text that
  accompanies a tool call is preserved in the session log. (#116)
- **Release workflow** installs `libasound2-dev` on Linux so
  `sapphire-call`'s `cpal` dependency builds cleanly during the
  workspace publish step. (#115)

## [0.6.0] - 2026-05-17

### Added

- **Voice pipeline + `sapphire-call` satellite** — local STT/TTS via
  the official `sherpa-onnx` crate (statically linked, no system
  install), Silero VAD, and a three-stage openWakeWord ONNX detector.
  `sapphire-call voice` runs as a satellite: capture mic → wake →
  VAD → ship PCM to the agent's `voice/pipeline_run` JSON-RPC method →
  receive synthesized speech back via SSE. Always-on listening with
  silence-cancel timeout (`listen_timeout_seconds`), distinct
  wake/capture/timeout beeps, supervisor threads that survive ALSA
  POLLERR, `--list-devices` discovery, persistent TOML config, and
  heartbeat→satellite push so the agent can speak unprompted. Wake
  word is served from the agent so all satellites for a given
  `room_profile` share the same phrase, and voice routes by
  `(device_id, room_profile)` rather than session pinning. Multiple
  TTS providers are supported (Style-Bert-VITS2 / Gradio / OpenAI
  audio/speech). Optional `voice-sherpa` feature for the local
  inference path. (Issues #82, #83, #87, #88, #94, #95, #103, #105,
  #106 and related.)
- **OpenAI-compatible chat provider + per-room routing** — new
  `[providers]` / `[profiles]` / `[room_profile]` schema lets you mix
  Anthropic and OpenAI-compatible (local LLM, OpenRouter, …) backends
  and bind each Matrix/Discord room to a profile. Background and
  refusal-fallback chains layer on top so heartbeat / catch-up tasks
  can use a different model than the user-facing room. Per-session
  profile selection is exposed on the JSON-RPC `initialize` call.
  (#68)
- **Memory namespaces** — workspaces can now host multiple memory
  namespaces, each with its own digest cadence and optional
  `background_profile`. Writes, reads, and catch-up are threaded
  through the namespace; room → profile → namespace pairing is
  declared in `[room_profile.<n>]`. (#70)
- **A2A (Agent2Agent Protocol) server** — minimal v1 implementation
  exposing `POST /a2a` (JSON-RPC 2.0, `SendMessage`) and
  `GET /.well-known/agent-card.json`. Off by default; enable via
  `[a2a].enabled = true`. Auth is per-room profile:
  `[room_profile.<n>].api_keys` declares bearer tokens, and an
  incoming `Authorization: Bearer <token>` reverse-looks-up the owning
  profile so clients (e.g. sapphire-world) never name `room_profile`
  themselves. `FilePart` is routed to the multimodal provider for
  image / vision input, with an external image cache so satellites can
  reference content by hash instead of re-uploading bytes.
  Streaming, `tasks/*`, and push notifications are deferred. (#78, #93,
  #108)
- **MCP server for external AI clients** — `POST /mcp` now publishes
  the `recall_memory` and `write_report` tools so Claude Code (and
  other MCP clients) can share project context with the agent. See
  [docs/mcp-integration.md](docs/mcp-integration.md). (#79)
- **Concurrent Matrix + Discord channels** — both channels now run in
  the same process and can be enabled simultaneously. (#76)
- **Timer + Pomodoro tools** — single-slot in-memory timer with
  `timer_set` / `timer_preset` / `timer_cancel` / `timer_status`,
  including Pomodoro presets. The expiry fires a notification back into
  the active conversation. (#110)
- **Intra-day cross-session memory digest** — sessions opened later
  the same day inherit a compressed digest of prior same-day sessions
  for the same room, along with explicit room metadata in the system
  prompt. (#85)
- **Periodic log catch-up + draft merge** — heartbeat now back-fills
  missing weekly / monthly / yearly logs after downtime and merges
  pre-written drafts instead of overwriting them. (#67)
- **`ANTHROPIC_API_KEY` env fallback** — `[anthropic].api_key` falls
  back to the standard environment variable when omitted from
  `config.toml`.

### Changed

- **`sapphire-workspace` 0.10.1 → 0.11.0** — picks up upstream
  improvements; `cpal` 0.16 → 0.17, `bzip2` 0.5 → 0.6, `sha2` 0.10 →
  0.11, plus various Dependabot patch bumps (sherpa-onnx, tokio,
  grain-id, matrix-sdk, openssl, …).

### Breaking

- **`sapphire-agent` subcommands collapsed** — the `call` and `serve`
  subcommands are gone. `sapphire-agent` with no arguments now starts
  the channel listeners + JSON-RPC HTTP control API directly. `verify`
  and `summarize` remain. Anyone scripting `sapphire-agent serve` must
  drop the subcommand. (#113)
- **Control API endpoint renamed `/mcp` → `/rpc`** — the existing
  JSON-RPC control surface (`initialize`, `chat`, `get_session`,
  `list_sessions`, `voice/*`) is sapphire-agent's own control API, not
  an MCP server, so it moves to a neutral path. The session header is
  likewise renamed `Mcp-Session-Id` → `Session-Id`. `sapphire-call` is
  updated in lockstep. `/mcp` is now bound to the real MCP server
  described in "Added" above. (#79, #80, #84)
- **`sapphire-call` config: `room_profile` knob replaced by bearer
  token** — the satellite no longer names a `room_profile` itself.
  Instead, the bearer token in `[server].token` must appear in some
  `[room_profile.<n>].api_keys` on the agent, and the matching profile
  is selected server-side (same mechanism as MCP and A2A). (#111)
- **Per-room config moved into `[room_profile]`** — room → profile →
  namespace pairing is now declared in `[room_profile.<n>]` instead of
  being scattered across `[matrix.rooms]` / `[discord.channels]`. Old
  layouts must be migrated. (#70)

### Fixed

- **ALSA POLLERR survival** — input and output `cpal::Stream`s now run
  under supervisor threads that restart on ALSA error, so the satellite
  survives transient USB / device hiccups instead of falling silent.
  (#104)
- **Follow-up listen window after TTS reply** — the mic re-opens
  reliably after the AI speaks, with a separate wake-then-command
  timeout and a distinct double-beep on cancellation so the user can
  tell success from silence. (#106)
- **Gradio TTS reliability** — corrected `audio_field` SSE shape,
  surfaced upstream errors instead of swallowing them, and included
  `enable_watermark` in the Irodori-TTS payload.

## [0.5.0] - 2026-04-22

### Changed

- **`sapphire-workspace` 0.9 → 0.10.1** — the workspace crate now requires
  each host app to supply cache/data directories and host-detected device
  facts before any workspace is opened. `main` wires this up via a new
  `init_app_ctx()` call at startup (using `directories` for paths and a new
  `hostname` direct dependency), so the git sync backend can record device
  info without panicking in `APP_CTX.device()`.

### Fixed

- **`file_write` / `memory_add` on non-existent files** — picked up via the
  workspace 0.10.1 bump, which fixes `canonicalize_or_parent` returning a
  path with a trailing separator. Previously every new daily log and every
  new `memory_add` call failed with EISDIR until the target file already
  existed on disk. See fluo10/sapphire-workspace#48.

### Breaking

- **`sync_interval_minutes` moved out of `[sync]`** — the cadence drives
  both `sapphire-sync` and `sapphire-retrieve`, so nesting it under `[sync]`
  was misleading. The field is now read from the agent config root. Users
  whose `config.toml` had `sync_interval_minutes` inside `[sync]` must move
  it to the top level.

## [0.4.1] - 2026-04-20

### Added

- **Unsupported image format notification** — when a Matrix message contains an
  image whose MIME type is not on Anthropic's allowlist, the agent now sends an
  explicit reply informing the user that the format is not supported, instead of
  silently dropping the attachment.

### Fixed

- **Image MIME type validation** — image attachments in Matrix messages are now
  validated against Anthropic's supported media-type allowlist
  (`image/jpeg`, `image/png`, `image/gif`, `image/webp`) before being forwarded
  to the API, preventing request errors caused by unsupported formats.

## [0.4.0] - 2026-04-19

### Added

- **`weather` tool** — fetches a short-term weather forecast via the Open-Meteo
  API (no API key required). Accepts either a `location` name (geocoded
  internally) or explicit `latitude` / `longitude`, plus an optional `days`
  (1–7, default 3). Returns current conditions and a daily min/max /
  precipitation / weather-code summary.
- **`file_append`, `dir_list`, `dir_walk` tools** — round out the filesystem
  toolset: append text to a file (creating it if missing), list direct children
  of a directory, and recursively walk a tree bounded by `max_depth` /
  `max_entries`. All three share the same path semantics as `file_read` /
  `file_write` (absolute, `~/...`, or workspace-relative) and update the
  retrieve index + git sync for internal paths.
- **Weekly / monthly / yearly log auto-generation** — heartbeat now writes
  summarised logs under `memory/{weekly,monthly,yearly}/` at the appropriate
  day boundaries (Monday / 1st / Jan 1). Each log carries a YAML frontmatter
  `digest:` array of importance-ordered bullets produced by the same LLM call
  that writes the body, so the agent no longer has to call the `memory` tool
  to recall long-horizon context.
- **Periodic digest injection** — the system prompt gains four new blocks
  after "Yesterday's Log": "This Week's Digests", "This Month's Digests",
  "This Year's Digests", and "Past Years' Digests". Each block pulls the
  top-N items of the relevant logs' digests (N per kind is configurable via
  the new `[digest]` config section: `daily_items`, `weekly_items`,
  `monthly_items`, `yearly_items`; defaults 3/3/5/5).
- **Daily digest back-fill** — pre-existing daily logs that lack a `digest:`
  frontmatter are upgraded in-place at startup and in the hourly catchup
  loop. Unrelated frontmatter keys the memory tool writes (`last_read_at`,
  `read_count`) are preserved.

### Changed

- **`terminal` → `shell`** — the shell-execution tool is renamed for clarity
  and now honours `$SHELL` by default (falling back to `/bin/sh`) instead of
  hard-coding `sh`. An optional `shell` parameter lets the agent pick a
  specific interpreter (e.g. `bash`, `zsh`, `fish`, or an absolute path) per
  call. Callers that hard-coded the old `terminal` name must update.
- **Tool naming — `<namespace>_<operation>`** — the three file tools are
  renamed from `read_file` / `write_file` / `delete_file` to `file_read` /
  `file_write` / `file_delete`, matching the convention used by every other
  tool in the agent (`memory_*`, `workspace_*`, `web_search`, `mcp_reconnect`).
  Callers that hard-coded the old names must update.
- **Unified file-operation tools** — `file_read`, `file_write`, and
  `file_delete` now route all paths through `WorkspaceState`, so a single tool
  handles both workspace-internal and external paths. Paths may be absolute,
  `~/...`, or relative to the workspace root; internal paths still update the
  search index and git sync automatically, external paths fall through to
  plain `std::fs`. The redundant `workspace_read` and `workspace_write` tools
  are removed — the generic `file_read` / `file_write` replace them. Enabled
  by `AppContext::allow_external_paths()` (new in sapphire-workspace 0.9).
- **sapphire-workspace 0.9** — upgraded the workspace dependency from 0.8.1
  to 0.9.0. The retrieve store's search API now takes typed `FtsQuery` /
  `VectorQuery` builders and `search_similar` embeds the query internally
  (no pre-computed vector needed), so the `workspace_search` tool no longer
  needs to manually call the embedder or dedup chunk results. Semantic search
  output drops the per-result title (the new `FileSearchResult` is already
  file-level with `id`, `path`, `score`, `chunks`); results now render as
  `- {path} [score]`.
- **Daily log format** — newly generated dailies now carry YAML frontmatter
  with a `digest:` array. The existing body format (`# Daily Log: YYYY-MM-DD`
  + summary) is unchanged. Files can be hand-edited freely.

## [0.3.3] - 2026-04-16

### Added

- **Daily log catchup loop** — heartbeat now runs an hourly catchup task that
  regenerates any missing daily log without waiting for the next process
  restart, so a transient failure at the midnight boundary self-heals within
  the hour.

### Fixed

- **Shutdown summary cancellation** — `main` now awaits the agent task after
  `serve::run` returns, so the shutdown summary's LLM call completes instead
  of being cancelled along with the runtime (previously surfaced as a
  misleading `dns error: task NN was cancelled`).
- **Stale system prompt after late daily log** — freshly written daily logs
  now invalidate the per-conversation system-prompt cache, so the model sees
  the new log immediately instead of staying on the cached snapshot until the
  next day boundary.

## [0.3.2] - 2026-04-15

### Fixed

- **Daily log sync** — daily log writes now go through `WorkspaceState` so
  generated logs are picked up by the workspace git sync instead of being
  written directly to disk and skipped.
- **Discord reconnect** — the Discord gateway now reconnects with exponential
  backoff on disconnect instead of silently stopping, matching the Matrix
  sync reconnect behavior.

## [0.3.1] - 2026-04-15

### Fixed

- **Windows release build** — bundle SQLite into the binary via matrix-sdk's
  `bundled-sqlite` feature so the Windows release no longer fails on the
  missing system SQLite library.

### Changed

- **Release workflow** — macOS and Windows matrix entries are now
  `continue-on-error`, so a failure on those best-effort platforms does not
  block the Linux x86_64/aarch64 release.

## [0.3.0] - 2026-04-15

### Added

- **Daily log injection** — previous day's daily log is now injected into the
  system prompt at session start, giving the agent continuity from yesterday's
  activity without loading raw history.
- **MCP manual reconnect** — new `mcp_reconnect` built-in tool lets the agent
  manually reconnect to an MCP server that has dropped without restarting the
  whole process.
- **Configurable day-boundary policy** — `session.day_boundary` option controls
  whether and how a new session is opened at midnight; defaults to the previous
  rolling behavior.
- **Matrix multi-room** — multiple Matrix rooms can now be configured with
  independent session state.
- **Install scripts** — `install.sh` (Unix) and `install.ps1` (Windows) added
  for installing pre-built binaries without Cargo.

### Changed

- **Session context on restart** — sessions now carry a generated summary into
  the next session rather than replaying raw message history, keeping the
  context window usage predictable after long-running sessions.
- **Workspace config consolidation** — workspace-level settings are now merged
  from the agent `config.toml` so a separate workspace config file is no longer
  required.
- **Release workflow** — GitHub Actions release workflow improved for more
  reliable artifact publishing.

### Fixed

- **Matrix sync reconnect** — the Matrix sync loop now auto-reconnects on
  network disconnect instead of silently stopping.
- **Bootstrap room filter** — fallback summarization is skipped for rooms that
  have no agent config, preventing spurious errors on startup.

## [0.2.1] - 2026-04-13

### Fixed

- **`sapphire-workspace` 0.8.0 → 0.8.1** — fixes git sync push not working
  due to a bug in `sapphire-sync` where the push step was silently skipped.

## [0.2.0] - 2026-04-12

### Added

- **MCP client** — built-in MCP client for integrating external tool servers,
  with support for `notifications/tools/list_changed` to dynamically refresh
  the tool list at runtime.
- **Context compression** — automatically compresses the conversation when the
  session approaches the context window limit, keeping sessions alive longer.
- **Heartbeat config** — `heartbeat_enabled` and `standby_mode` options in the
  agent config to pause or suspend background heartbeat tasks without
  restarting.
- **Crate split** — `sapphire-agent-api` (shared types + SSE client) and
  `sapphire-call` (standalone CLI client) extracted into their own workspace
  crates so they can be published and used independently.
- **Dependabot** — automated dependency update PRs with grouped Cargo patch
  updates on Fridays and auto-merge for patch-level bumps.

### Changed

- **`sapphire-workspace` 0.5.0 → 0.8.0** — picks up upstream improvements to
  file indexing, vector search, and git sync.
- **`reedline` upgraded to 0.47** — handles the new non-exhaustive `Signal`
  enum without warnings.
- **Memory tools** — `MemoryTool` redesigned into per-file entry tools with
  frontmatter tracking for finer-grained read/write control.

### Fixed

- `[sync]` config is now read from the agent config file rather than only from
  the workspace config, so sync settings specified in `config.toml` are
  actually honoured.

## [0.1.0] - 2026-04-09

Initial release of `sapphire-agent` — a personal AI assistant built around the
Anthropic API, with Matrix and Discord chat frontends, an interactive REPL, and
an HTTP/MCP server mode.

### Added

- **Core agent** — Anthropic API client with tool-use support and multi-round
  tool execution that preserves assistant text across rounds.
- **Matrix frontend** — `matrix-sdk` based client with end-to-end encryption
  and Markdown rendering.
- **Discord frontend** — `serenity` based bot with channel support and
  per-channel light/heavy model switching.
- **`call` command** — interactive REPL powered by `reedline` with CJK-safe
  input, history, cursor movement, history dump on resume, and date in prompt.
- **`serve` command** — HTTP server exposing an MCP Streamable HTTP API,
  with graceful shutdown on Ctrl-C.
- **Built-in tools** — `read_file`, `write_file`, `delete_file`, `web_search`,
  `terminal` (defaulting to the workspace root), and `workspace_search`.
- **Workspace integration** — `sapphire-workspace` 0.5 with file indexing,
  full-text search, vector search via LanceDB (default feature
  `lancedb-store`), and git sync.
- **Sessions** — UUIDv7-backed session files, human-readable `grain-id`
  aliases for API sessions, auto-generated session titles, and deferral of
  empty sessions so they are not persisted.
- **Memory system** — long-term memory with periodic compaction.
- **Heartbeat** — cron-scheduled background tasks defined via YAML
  frontmatter, daily logs, async tool execution, and periodic workspace sync.
- **Configuration** — TOML config with XDG directories, `~/` path expansion,
  and workspace-aware writes.
- **Logging** — `tracing` with env-filter and ANSI output.

[0.6.0]: https://github.com/fluo10/sapphire-agent/releases/tag/v0.6.0
[0.5.0]: https://github.com/fluo10/sapphire-agent/releases/tag/v0.5.0
[0.4.1]: https://github.com/fluo10/sapphire-agent/releases/tag/v0.4.1
[0.4.0]: https://github.com/fluo10/sapphire-agent/releases/tag/v0.4.0
[0.3.3]: https://github.com/fluo10/sapphire-agent/releases/tag/v0.3.3
[0.3.2]: https://github.com/fluo10/sapphire-agent/releases/tag/v0.3.2
[0.3.1]: https://github.com/fluo10/sapphire-agent/releases/tag/v0.3.1
[0.3.0]: https://github.com/fluo10/sapphire-agent/releases/tag/v0.3.0
[0.2.1]: https://github.com/fluo10/sapphire-agent/releases/tag/v0.2.1
[0.2.0]: https://github.com/fluo10/sapphire-agent/releases/tag/v0.2.0
[0.1.0]: https://github.com/fluo10/sapphire-agent/releases/tag/v0.1.0
