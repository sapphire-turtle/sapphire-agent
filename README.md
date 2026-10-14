# sapphire-agent

> Language: **English** | [日本語](README.ja.md)

A personal AI assistant agent that works on file-based, local-first [`sapphire-framework`](https://github.com/sapphire-turtle/sapphire-framework) workspaces together with you — talking through Matrix and Discord.

> **Status: personal project.** This is something I built for my own use. It only has to work in my environment, and that is the only environment I test it in. You are welcome to use it, fork it, or send pull requests, but I am not going to maintain providers, channels, or features I do not personally use. If your use case overlaps with mine, great; if not, fork freely.
>
> The reason this exists at all is that other agents I tried (openclaw, zeroclaw, …) either did not support what I needed, were not actually tested for the parts I cared about, or did not accept fixes. So I wrote my own. Please calibrate expectations accordingly.

## What it does

- **Channels**: Matrix (E2EE via `matrix-sdk`) and Discord (`serenity`), running concurrently.
- **Providers**: Anthropic Messages API with SSE streaming and a multi-round tool-use loop, plus OpenAI-compatible backends (local LLMs, OpenRouter, …) selectable per room/session via the `[providers]` / `[profiles]` / `[room_profile]` schema.
- **Workspace**: backed by [`sapphire-framework`](https://github.com/sapphire-turtle/sapphire-framework) — file index, full-text + vector search (redb + tantivy, plus LanceDB vectors by default).
- **Built-in tools**: `file_read`, `file_write`, `file_append`, `file_delete`, `dir_list`, `dir_walk`, `shell`, `timer_set` / `timer_preset` / `timer_cancel` / `timer_status` (incl. Pomodoro presets), plus workspace memory / search / sync tools. The seven that touch a filesystem or a shell (`file_read`, `file_write`, `file_append`, `file_delete`, `dir_list`, `dir_walk`, `shell`) plus `shell_start` / `shell_output` / `shell_kill` for long-running commands are **session-routed: which machine they reach is decided by the session, not by the name**. Inside an ACP session they act on the machine the connected editor is running on, over `/acp`'s `fs/*` and `terminal/*` requests, and only for the capabilities that editor declared at `initialize` — with no fallback to the agent's own disk when one is missing. Everywhere else (Matrix, Discord, `/rpc`, voice, `/a2a`, heartbeat, autonomous, subagents) they act on this agent's own machine, and are opt-in: `[tools.host_access] enabled = false` by default, for every origin. Turning that on is a deliberate act; running the agent in a container is the recommended way to do it once it is. See [Shell and file tools: whose machine](#shell-and-file-tools-whose-machine) below.
- **Sessions**: human-readable [`grain-id`](https://crates.io/crates/grain-id) aliases, auto-generated titles, history dump on resume.
- **Background**: heartbeat cron tasks, periodic memory compaction, periodic workspace re-index, daily / weekly / monthly / yearly logs with catch-up.
- **Voice**: a server-side STT → turn → TTS pipeline (`voice/*` on `/rpc`) with local STT/TTS via `sherpa-onnx`, for networked voice clients. The `sapphire-agent-cli voice` satellite and the desktop's voice input / spoken replies were dropped when the local clients moved to IPC ([#278](https://github.com/sapphire-turtle/sapphire-agent/issues/278)); the satellite role moves to wearable device firmware.
- **Ambient audio ingest**: optional always-on capture from a wearable/pendant device — `POST /audio/ingest` takes raw audio (metadata in query params, no JSON/base64 framing) from a bearer-authenticated device, re-gates it, transcribes it, attributes it to a speaker against reference audio curated in the workspace, and stores the transcript outside the workspace. Records without answering: nothing in this path starts an LLM turn. `transcript_read`, `speaker_candidates` and `speaker_promote` expose the result as agent tools. Disabled by default — enable via `[ambient].enabled = true`; see `config.example.toml`.
- **Agent-to-agent**: `/a2a` endpoint speaks the v1 A2A protocol (JSON-RPC `SendMessage`, AgentCard) with per-device bearer-token auth — enable via `[a2a].enabled = true`.
- **Subagents**: `<workspace>/agents/<name>.md` definitions the main agent can delegate a task to via the `subagent` tool — its own system prompt, its own tool loop, only the final answer comes back, it can be resumed by handle for a later round, and a delegated agent can itself delegate up to `[tools.subagent] max_depth` deep. See [Subagents](#subagents) below.
- **Skills**: written procedures (planning, TDD, debugging, code review, …) loaded on request from a checkout on the *editor's* machine, over ACP. Requires an ACP client that declared terminal support — off entirely for Matrix, Discord and voice. See [Skills](#skills) below.
- **Editor integration**: Zed (or any ACP client) drives the running agent over the Agent Client Protocol — on the agent's own machine by starting `sapphire-agent acp` (stdio, relayed to the running agent over its local IPC endpoint), from elsewhere over the `/acp` WebSocket endpoint (`[acp].enabled = true`); see [Zed / ACP](#zed--acp) below.
- **Commands**:
  - `sapphire-agent` (or `sapphire-agent serve`) — start the channel listeners + JSON-RPC HTTP control API (`/rpc`, `/a2a`, `/acp`)
  - `sapphire-agent chat` — interactive REPL with the agent running on this host, over its local IPC endpoint (`-m TEXT` for one message, `--list`, `--history`, `--session ID`, `--json`); runs under `[cli].room_profile`
  - `sapphire-agent acp` — ACP on stdio for an editor, relayed to the running agent over IPC
  - `sapphire-agent init [PATH]` — seed a workspace with the files the agent reads (`AGENTS.md`, `SOUL.md`, …), then print the host-local config to paste; never overwrites an existing file
  - `sapphire-agent verify` — validate config and report loaded workspace files, including the device -> room_profile bindings
  - The sapphire-framework verbs, which talk to the running server or the bridge: `status`; `service install|uninstall|status`; `workspace init|select|show|…` (the agent serves the selected workspace — `workspace_dir` in `config.toml` is only adopted on the first start, and switching restarts the agent around the new one); `workgroup`; `device`; `embedding`; `external-device add|list|rotate|retire|restore|apps` (clients that reach the agent with a bearer token — `add` and `rotate` print the token once)

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/sapphire-turtle/sapphire-framework/main/install/install.sh | sh -s -- agent
```

This installs `sapphire-agent` into `~/.local/bin` together with `sapphire-bridge`, which it syncs through. Add `agent-desktop` for the desktop client. See the [install script's README](https://github.com/sapphire-turtle/sapphire-framework/tree/main/install) for the options.

Or with cargo:

```sh
cargo install sapphire-agent
```

Or from source:

```sh
git clone https://github.com/sapphire-turtle/sapphire-agent
cd sapphire-agent
cargo build --release
```

Release binaries are published for **Linux** (x86_64, aarch64) and **macOS** (aarch64). Each is a single self-contained file — `sherpa-onnx` and its ONNX Runtime are linked statically, so there is nothing to install alongside it.

The agent is a headless server application and is not built for Windows ([#182](https://github.com/sapphire-turtle/sapphire-agent/issues/182)), and neither is `sapphire-agent-desktop`, which talks to the agent on its own host.

The server and its CLI are one binary ([#278](https://github.com/sapphire-turtle/sapphire-agent/issues/278)), the way `sapphire-ledger` and the other sapphire-framework apps are: the client commands (`chat`, `acp`, `status`, …) talk to the server already running on the same host over its local IPC endpoint. So does `sapphire-agent-desktop`, the GUI chat client; like `chat` it runs under `[cli].room_profile`.

## Configure

Start with an empty workspace:

```sh
sapphire-agent init ~/sapphire-workspace
```

This seeds the files the agent reads — `AGENTS.md`, `SOUL.md`, `IDENTITY.md`,
`USER.md`, `TOOLS.md`, `BOOTSTRAP.md`, the `memory/default/` tree, an example
heartbeat task, and a `.sapphire-agent/config.toml` listing the settings the
workspace layer is allowed to set. Most are deliberately blank: `BOOTSTRAP.md`
is a first-run ritual that has the agent ask what to call it, write its answers
into `IDENTITY.md` and `SOUL.md`, and then delete itself.

`init` never overwrites, so it is safe to re-run — that is also how a workspace
made by an older build picks up a file added since.

It writes nothing outside the workspace. Credentials and machine paths are
host-local, so rather than writing that file `init` prints it for you to place
at `~/.config/sapphire-agent/config.toml` (on Linux). See `config.example.toml`
for everything else it can hold — channels, bind addresses, MCP servers,
STT/TTS model paths.

Then:

```sh
sapphire-agent verify   # sanity-check config and workspace
sapphire-agent          # start the channel listeners + HTTP control API
sapphire-agent chat     # interactive session with it (needs [cli].room_profile)
```

### Keys nothing reads

A key no field consumes is a warning, never an error. A typo (`skils = true`)
and a setting written under the wrong table (`skills` belongs to
`[memory_namespace.<name>]`; under `[room_profile.<name>]` it parses fine and
does nothing) are otherwise silent — serde drops the key, so the file reads as
configured to whoever wrote it and as unset to the agent. Startup logs one
warning per file, naming the file through the same provenance `verify` uses:

```
WARN Ignoring 1 unrecognised key(s) in /home/you/.config/sapphire-agent/config.toml:
     room_profile.default.skills. Nothing reads these, so the agent is running as if
     they were absent — check for a typo, or for a setting written under the wrong table.
```

`verify` prints the same list under `Unknown keys`. This is deliberately not
`#[serde(deny_unknown_fields)]`: a hard error would make a config written for a
newer build unloadable by an older one, which matters most for the workspace
layer, since it travels between hosts.

## Several hosts (sync)

The agent is a sapphire-framework app: with the host's `sapphire-bridge`
running, `sapphire-agent workspace sync enable` syncs its workspace —
sessions, memory, logs, definitions — with the other devices of the
workgroup, and every device can run its own agent around its copy. Without
a bridge, or with sync off, the agent runs alone exactly as before.

Work that must happen once is placed on one host:

- **Chat channels** run where their credentials are. `[matrix]` and
  `[discord]` are host-local settings and never sync; configure each bot on
  exactly one host. Each host writes the channels it serves — accounts and
  rooms, no credentials — to `.sapphire-agent/hosts/<device-id>.toml` in the
  workspace, and warns at startup when another host claims the same account
  (both would answer every message).
- **Heartbeats** run on the one host that can deliver them: the device named
  by `device:` in the task's front matter (name or id), else the host serving
  its `room_id`, else the host its `voice:` satellite is subscribed to. A task
  with no target runs on the primary device, delivered to that host's
  default room. With both `room_id` and `voice`, the room's host runs it and
  falls back from voice to the room within itself.
- **The daily, weekly and monthly logs, memory compaction and autonomous
  sessions** run on the workspace's primary device, which the framework
  elects (set a device's priority to influence it).

Each host still needs its own host-local `config.toml` — LLM keys are not
synced. Tool results and images are cached outside the workspace and do not
sync: a session continued on another host sees placeholders in their place.

## Subagents

The main agent can delegate a task to a specialised subagent via the
`subagent` tool: a nested conversation with its own system prompt and its
own tool-calling loop, which hands back only its final answer. Use it to
keep a large investigation (read 30 files, conclude in 3 lines) out of the
conversation that would otherwise carry all 30 files forward on every later
turn.

**Defining one.** Drop a Markdown file under `<workspace>/agents/<name>.md`
— the same shape as `<workspace>/heartbeat/*.md`, reusing that convention
rather than inventing a second one. YAML frontmatter for the metadata, the
body for the prompt:

```markdown
---
description: Reviews a diff. Reads and reports; does not edit.
tools: [file_read, workspace_search, memory_read]
profile: dev
---
You are a reviewer. Read the diff, report problems, and stop there.
```

- **`description`** is the *only* thing the parent model sees before
  deciding whether to delegate — a weak one means the agent never gets
  called.
- **`tools`** is optional. Omit it and the subagent inherits the parent's
  whole visible tool set (see "What a subagent does not inherit" below for
  the one exception); give it a list — including `[]` — and the subagent is
  restricted to exactly that. Both are legitimate: an empty list is a valid
  definition, for an agent that only needs the prompt to summarise or judge.
- **`profile`** is optional and names a `[profiles.<name>]` entry — the
  subagent then runs on *that* provider (with its `fallback_provider`
  included) instead of the parent's. Omit it and the subagent runs on
  whatever model the delegating turn runs on: both are legitimate, and
  "no profile" is the pre-existing behaviour unchanged. A name that
  config's `[profiles]` does not define fails startup outright — a
  typo'd profile name should not silently run the agent on a different
  model.
- **`subagents`** is optional, and mirrors `tools` one level up: it
  restricts *which agents this one may delegate to*. Omit it and the
  agent inherits whichever agents the delegating turn could already
  delegate to; give it `[]` and this agent never delegates further,
  whatever depth cap is configured; give it a list and only those agent
  names are offered. Same shape, same semantics, different namespace —
  agent names here, tool names in `tools:`.
- **The body** is the subagent's entire system prompt. That is genuinely
  all of it — see below.

Definitions are read once at process startup, the same as heartbeat tasks;
adding or editing one needs a restart. A file with no or invalid
frontmatter, or no `description`, is skipped with a warning at load time —
one broken definition does not take the others down.

**`tools:` is enforced, not a hint.** `TurnLoop::run` refuses, before any
other check, any tool call whose name was not in the round's own advertised
list — this is what makes a reviewer definition that says "reviews, does
not edit" actually unable to edit, rather than merely undocumented for
editing. The same check is what caps delegation depth (see below) and
applies identically on the parent's own turn, closing a hallucinated- or
out-of-list tool name there too.

**What a subagent does not inherit.** Its system prompt is the
definition's body plus the current date and time — and nothing else. In
particular, none of the following reach it: `SOUL.md`, `IDENTITY.md`,
`USER.md`, `AGENTS.md`, `TOOLS.md` or any other workspace file the main
agent's prompt is built from, the memory digest, today's cross-session
digest, room metadata, or the configured `anthropic.system_prompt`. **This
is the point of the feature, not an oversight.** The main agent carries all
of that deliberately — it is meant to be someone to work *with* — but a
code review does not need yesterday's conversation, and dropping the parts
a task doesn't need is what a subagent is *for*. The date is the one
exception: an agent that doesn't know today's date can't correctly use a
tool that writes one, and that's a fact rather than a personality trait.

That statement is about the *prompt text*, though, not about *tool reach*:
a definition with no `tools:` key still inherits whichever `memory_*` tools
the parent can see, and a subagent's memory calls land in the same
namespace the delegating conversation is already in. So "a subagent has no
memory" is true of what it's told up front, not of what it's able to go
look up — that distinction is entirely in the definition's own `tools:`
list.

The provider is the one element a subagent does not always inherit: a
definition that names a `profile:` runs on that profile's provider instead
of the parent's. Everything else here is entirely unchanged by whether a
profile is set — the same permission gate, the same enforced tool list, the
same isolation. A profile changes only *which model* runs the subagent,
never what it may do or see.

**Isolation is about history and the store, not about visibility.** A
subagent's tool calls still fire the same `tool_start`/`tool_end`
notifications on the parent's host that the parent's own calls do, so they
show up live in an ACP session's stream — that's necessary for a
permission prompt triggered by a subagent's call to make sense as coming
from *this* conversation. What doesn't happen is persistence: nothing from
a subagent's internal turns reaches the parent's stored history or the ACP
session store. Only the returned final answer does, as the `subagent`
tool's own result — reload the session later and you see "delegated to
reviewer, got back Y", not the 30 files it read to get there.

That "only the returned final answer" is text-only, too: `SubagentTool::execute`
returns a plain `String`, so a nested tool's image output has nowhere to
go. If a subagent's own tool list includes `recall_image`, calling it
produces nothing the parent (or the model) can see — the image bytes are
simply dropped rather than attached to anything.

**Delegation nests, up to `[tools.subagent] max_depth`** — 2 by default.
The main agent runs at depth 0, a subagent it dispatches at depth 1, and a
subagent that one dispatches at depth 2; a planner can therefore hand a
focused search to an explorer, but nothing runs three levels down. Set
`max_depth = 1` for the old "delegation is always exactly one level"
behaviour, or `0` to forbid delegation outright — the `subagent` tool is
then not offered to any turn, the main agent's included. Enforcement is
exactly the tool-list check described above: once a turn sits at the cap,
its offered list simply omits `subagent`, and a call naming it anyway is
refused the same way any other out-of-list name is, so a subagent cannot
recurse by guessing — nor can a definition widen its own reach by naming
`subagent` in `tools:`.

**The allowlist narrows at every level.** A definition's `subagents:` is
the same shape as `tools:`, applied to agent names instead of tool names:
omit it and the child can delegate to whatever the delegating turn could,
write `[]` and it never delegates further regardless of the depth cap, and
write a list and only those names are offered. A nested subagent is thus
governed by both the depth cap and every allowlist on the chain above it —
each level decides locally, rather than the whole chain being computed up
front.

**`[tools.subagent] turn_timeout_secs` applies per level, not per chain.**
Each nested subagent turn gets its own deadline, so a child that hangs is
stopped and reported to its own parent as a tool error, and that parent
carries on.

**Permission requests work normally.** A subagent's tool calls are judged
by the parent's own `Origin`, through the parent's own `TurnHost` — the
same gate, the same person asked, the same standing allow/deny answers.
Delegating is not a way to get done by proxy what the model would be
refused directly.

**`subagent` runs unconditionally on every trusted transport, and is
excluded only on Matrix and Discord.** Its `ToolKind` is `Other` — what a
subagent actually does depends entirely on which agent it delegates to, so
it can't honestly be classified as `Read`, `Edit`, `Execute`, etc., and
`Other` is the policy table's most conservative bucket. `Origin::Trusted`
(`/rpc`, `/a2a`, voice) allows every kind unconditionally, so those three
run `subagent` the same as any other tool; on ACP it is asked for in
`default` mode and allowed once edits are accepted or bypassed, same as
any other `Other` call. `Origin::Channel` is the one origin that denies
every `Other` call unconditionally, so Matrix and Discord — and only
Matrix and Discord — cannot call `subagent` at all. See the
[permission table](#permission-and-modes) below.

**Project conventions are not shared yet.** A client-side project's
`CLAUDE.md` isn't threaded into a subagent's prompt — but the main agent
doesn't read it either, currently. Both are tracked together in
[#199](https://github.com/sapphire-turtle/sapphire-agent/issues/199); when that
lands, subagents get it too.

### Resuming a subagent

A dispatch's answer is prefixed with a handle:

```
[subagent reviewer · handle 0198f...]
<the child's answer>
```

Call `subagent` again with `resume` (the handle) and a new `prompt` — instead
of `agent` — to continue that exact child conversation: its own prior
history, its own system prompt, picking up where it left off. `agent` and
`resume` are mutually exclusive; giving both, or neither, is a recoverable
error.

**Resume is best-effort.** A handle can stop resolving — pruned by age, the
process restarted with no cache directory resolvable, or a history that grew
past the cache's byte cap and was never written. Any of those is a
recoverable error telling the model to dispatch a fresh child instead; there
is no other recovery path, and none is needed — a fresh dispatch is the
answer SDD's own fix loop already gives for rounds 4-5.

**The child's history lives outside the workspace**, at
`~/.cache/sapphire-agent/subagents/<handle>.json` (`dirs::cache_dir()`,
beside the digest and tool-result caches) — never under
`<workspace>/sessions`. That is deliberate: `sessions` is in the retrieve
search index, and a subagent's full internal transcript — tool calls,
intermediate reasoning, false starts — is the single most effective way to
skew a search over it. Losing the cache directory costs only the ability to
resume in-flight children, nothing else.

Two config knobs, under `[subagent_cache]`, both optional:

```toml
[subagent_cache]
max_history_bytes = 8388608   # 8 MiB, the default
retain_days = 7                # the default
```

- **`max_history_bytes`** caps one child's *serialized* history. Over the
  cap, the history is not written at all — never truncated, because dropping
  the oldest messages can leave a `tool_use` with no matching `tool_result`,
  which the provider API rejects outright and makes the whole history
  unloadable. The child's answer still comes back normally; only the handle
  line says it is not resumable (and, for a resume that itself went over the
  cap, that the *previously* stored copy — ending before this exchange — is
  what a later resume would still see).
- **`retain_days`** is how long a child may sit untouched before the daily
  heartbeat sweep prunes it.

**A resume against a handle already running is refused**, so two turns in
this process can never interleave writes into one stored history —
`busy_handles` is an in-process `HashSet`, not a lock shared across
processes or machines.

**The offered tool list is recomputed on every resume, not restored.** Only
the agent's *name* is stored; its definition is reloaded from
`<workspace>/agents/` each time, so an edit to the `.md` is picked up and —
more importantly — a resumed child cannot carry forward a wider tool list
than its current definition (or the current parent turn) allows. If the
definition no longer resolves — renamed, deleted, or simply a `.md` that
failed to parse on this particular load — resume fails with a recoverable
error rather than silently restoring a stale tool set.

**A missing definition does not delete the stored child.** `load_agents_dir`
skips a single unparseable `.md` with only a warning rather than failing the
whole load, so "the definition doesn't currently resolve" can be transient —
a mid-save YAML typo, fixed a moment later. Resume bails and leaves the
handle for `retain_days` to retire on its own schedule, rather than
destroying a conversation over what might be a one-off glitch.

## Zed / ACP

`sapphire-agent` can be driven directly from [Zed](https://zed.dev) as an ACP
(Agent Client Protocol) agent, running against the same production workspace
and memory as every other transport — a Zed conversation is not a separate
agent. Its sessions do get their own store, though: a separate directory
tree and line format from `/rpc`, Matrix and Discord, not a `cwd`-tagged row
folded into one of theirs. See
[Loading past sessions](#loading-past-sessions) below for the layout.

There are two ways in. On a machine where the agent runs — every synced host
runs one — the editor starts `sapphire-agent acp` itself. From a machine that
does not, it connects to `/acp` over the network.

### On the agent's machine: `sapphire-agent acp`

`sapphire-agent acp` speaks ACP on stdio, which is the transport editors
expect, and relays every line to the agent already running on this host over
its local IPC endpoint. The conversation runs in that agent, on this host; the
relay starts nothing, and exits with an error when no agent is running. No
token is involved — the endpoint admits only the OS user the agent runs as —
so the room profile a token would pick is named in the host config instead:

```toml
[acp]
stdio_room_profile = "zed"   # any [room_profile.<name>]; required for `acp`

[room_profile.zed]
profile = "default"
rooms   = []
```

Then, in Zed's `settings.json`:

```jsonc
"agent_servers": {
  "sapphire": { "type": "custom", "command": "sapphire-agent", "args": ["acp"] }
}
```

`[acp].enabled` is not needed for this; it gates the WebSocket endpoint only.

### From another machine: `/acp`

**1. Enable the endpoint** in your config (host-local; the workspace config
layer cannot turn this on):

```toml
[acp]
enabled = true
```

**2. Mint a token.** Authentication works as for `/rpc` and `/a2a`: add an
external device (the sapphire bridge must be running), bind its name under a
`[room_profile.<name>]`'s `devices`, and that device's token both authenticates the connection and
selects the room profile (and therefore the provider and memory namespace)
the ACP session runs under. A dedicated profile keeps the editor on its own
model or namespace:

```sh
sapphire-agent external-device add zed-editor
# prints the device's id and its token, once
```

```toml
[room_profile.zed]
profile = "default"
rooms   = []              # ACP-only; no chat rooms map here
devices = ["zed-editor"]  # its name, or the id `external-device add` printed
```

Note that `[acp].enabled = true` opens `/acp` to **every** device bound to
**any** room profile, not only to one minted for the editor — each simply
connects under its own profile. No privilege is gained by doing so (`/a2a`
already runs the full tool set through the same executor for the same
devices), but if you want the editor confined to its own model, namespace or
audit trail, that is what the dedicated profile above is for; giving it a
device does not take `/acp` away from the others.

**3. Install [`websocat`](https://github.com/vi/websocat).** Zed's
`agent_servers` setting only takes a `command` to spawn, not a URL — and ACP
itself still specifies only stdio transport, so *some* local process has to
bridge stdio to the network. `websocat` is that bridge, and no bridge code
was written for it: its `--text` stdio mode maps one line of stdin to one
WebSocket text frame and one received text frame to one line of stdout,
which is exactly the one-JSON-RPC-message-per-frame framing ACP's WebSocket
transport uses.

**4. Point Zed at it**, in Zed's `settings.json`:

```jsonc
"agent_servers": {
  "sapphire": {
    "type": "custom",
    "command": "websocat",
    "args": ["--text", "-H", "Authorization: Bearer <token>", "wss://your-host/acp"]
  }
}
```

Use `wss://`, not `ws://`, for anything beyond a loopback connection — the
bearer token travels in a plain header, so TLS is expected to already be
terminating in front of the endpoint (a reverse proxy, typically) rather than
being provided by the agent itself.

### Permission and modes

Tool calls from the editor are gated. Each tool declares what it does (read,
search, edit, delete, execute), and what happens next depends on the session's
mode:

| Mode | Reads, searches, fetches | Edits and deletes | Commands, unclassified tools |
|---|---|---|---|
| `default` | run | **ask** | **ask** |
| `accept_edits` | run | run | **ask** |
| `bypass` | run | run | run |

Zed's mode picker switches between them; the session starts in `default`.

When the agent asks, "Always allow this tool" and "Never allow this tool" are
recorded in `~/.config/sapphire-agent/acp-permissions.json`, keyed by room
profile and tool name. Delete the file to be asked everything again. It sits
beside the host-local config rather than in the workspace on purpose: trusting
a particular editor is a statement about *this machine*, not something to sync
to other hosts. The agent's own tools cannot write to that directory — see the
refusal in `file_write` — because a tool that could edit the permission record
could grant itself anything.

Both standing answers only apply where the agent would otherwise ask. "Never
allow this tool" is not a kill switch: `bypass` runs everything regardless, and
`accept_edits` runs edits regardless.

Declining a tool does not end the turn. The model is told the call was
refused and can try another route.

**Chat channels are not asked — they are restricted.** Matrix and Discord
cannot call `shell` at all, and reach an outbound MCP server's tools only if
the operator declared that server trusted (see "Trusting an outbound MCP
server" below). A chat turn is asynchronous, so holding one open waiting for
a human could hang it for hours; and routing the question through the model
would let it broker its own permission request.
`/rpc`, the voice pipeline and `/a2a` are unchanged in how *this* gate treats
them — they are still never asked. But none of the three can reach the seven
host-machine tools either, once `[tools.host_access] enabled = false` (the
default this crate ships): that gate applies to every origin, not just chat.
See "Reaching the agent's own machine is opt-in" below.

**The heartbeat's chat leg counts as a channel.** A scheduled task under
`<workspace>/heartbeat/` runs through the same path as a chat message when it
replies to a room, so it cannot call `shell`, and it reaches an MCP server's
tools on the same terms a chat message does. This is deliberate rather than an
oversight: heartbeat task bodies are workspace files, and `file_write` is an
edit, which a channel may perform without being asked *when host access is
on* — so trusting that path would let a chat message write itself a task that
runs a command on the next tick. With host access off (the default),
`file_write` is one of the seven host-machine tools and is refused for every
origin before this reasoning is even reached — but it still governs the moment
host access is turned on.

#### Trusting an outbound MCP server

Tools from `[[tools.mcp_servers]]` carry no classification of their own, so by
default they are unclassified — the strictest bucket — and a channel refuses
every one of them. That is the fail-safe working as designed, not a judgement
that MCP is dangerous, but it means a heartbeat task and a chat message alike
cannot call any MCP tool.

`trust` on the server entry is how the operator supplies the missing
classification:

| `trust` | Tools are classified | Chat channels and the heartbeat | `/acp` in `default` |
|---|---|---|---|
| `"none"` (default) | unclassified | **refused** | **ask** |
| `"read"` | reads | run | run |
| `"edit"` | edits | run | **ask** |

```toml
[[tools.mcp_servers]]
name  = "ledger"
type  = "http"
url   = "http://127.0.0.1:3173/mcp"
trust = "edit"
```

It is declared by the operator rather than read from the server for the reason
the channel restriction exists at all. MCP servers can annotate their own tools
(`readOnlyHint`, `destructiveHint`) more finely than one value per server —
but those annotations are *self-reported*, and a channel turn carries untrusted
input that nobody can be asked about. Letting the far side declare its own
tools safe would invert the thing being defended. Whoever wrote the config has
already decided to connect to that server; this is where they say how much that
decision was worth.

It is per server rather than per tool for a smaller reason: a list of tool
names would go stale the moment the server added a tool, and it would go stale
*silently* — the new tool falls back to `none`, is refused, and looks like the
server being broken. Coarser, but it cannot rot.

Nothing about `decide` changes. A `read` server's tools take the same path
`workspace_search` already takes, and an `edit` server's take `file_write`'s.

### Shell and file tools: whose machine

Ten tools — `file_read`, `file_write`, `file_append`, `file_delete`, `dir_list`,
`dir_walk`, `shell`, and the long-running trio `shell_start` / `shell_output` /
`shell_kill` — read or write a filesystem or run a shell. **Which machine they
reach is decided by the session, not by the name.** Inside an ACP session they
act on the machine the connected editor is running on, over `/acp`'s `fs/*` and
`terminal/*` requests; everywhere else (Matrix, Discord, `/rpc`, voice, `/a2a`,
heartbeat, autonomous, subagents) they act on this agent's own machine. Inside
an ACP session each tool maps to a request on the editor's machine:

| Tool | ACP call(s) | `ToolKind` |
|---|---|---|
| `file_read` | `fs/read_text_file` | `Read` |
| `file_write` | `fs/write_text_file` | `Edit` |
| `file_append` | `fs/read_text_file` then `fs/write_text_file` | `Edit` |
| `file_delete` | `terminal/*` — runs `rm` | `Delete` |
| `dir_list` | `terminal/*` — runs `find` | `Search` |
| `dir_walk` | `terminal/*` — runs `find` | `Search` |
| `shell` | `terminal/create` → `wait_for_exit` → `output` → `release` | `Execute` |
| `shell_start` | `terminal/create` | `Execute` |
| `shell_output` | `terminal/output` | `Read` |
| `shell_kill` | `terminal/kill` + `terminal/release` | `Execute` |

Inside an ACP session a tool is offered only for the capability the connected
editor actually declared in `initialize`, and there is no fallback: a missing
capability simply hides the tool rather than routing it to the agent's own disk.
`fs.read_text_file` and `fs.write_text_file` are read **independently** — an
editor that can read but not write files gets `file_read` (and the read half of
`file_append`) and nothing that writes. `terminal` gates the delete, directory,
and shell tools together. There is no round trip spent finding this out: an
unsupported tool is simply absent from the list the model sees.

`file_read` passes `line`/`limit` straight through to `fs/read_text_file`; that
pair exists in ACP for exactly this reason, so a large file can be read in
pieces instead of shipped over the wire whole.

**`shell`'s timeout does not kill the command.** It waits up to `timeout_secs`
(default 120, overridable per call, capped at 600) and, if the command is still
running when that runs out, hands back the terminal handle instead of an error —
the process is left running. ACP defines `terminal/release` as also killing the
command, so releasing on timeout would throw away however long a build had
already run, and for a non-idempotent command (`git push`, a migration, a script
that writes files) a subsequent retry would run it a second time. The result
text tells the model explicitly not to re-run, and points it at `shell_output` /
`shell_kill` instead.

**Terminals are tracked per session, not per connection, and a dropped
connection releases nothing.** `terminal/release` kills the command, so a network
blip must not trigger one — tracking by session id (rather than by connection)
is what lets a client that reconnects and reloads the session reach the same
handles with `shell_output` / `shell_kill`. A handle is released in exactly two
cases — the one-shot `shell` finishing inside its timeout, or the model calling
`shell_kill` — plus one case that is an *untrack* rather than a release: the
client reporting the handle unknown, where there is nothing left to free. An
output check that fails does **not** untrack the handle: the failure could be
transient, and dropping tracking of a terminal that might still be running would
be unrecoverable. `shell_kill`, by contrast, untracks unconditionally even when
the underlying kill or release call itself errors — over-counting is the
recoverable direction (the model can check or kill again); a permanently stuck
cap slot is not.

Each session may hold at most 8 terminals at once, counting both
`shell_start`'s handles and any `shell` call that timed out without finishing. A
ninth is refused, and the refusal names every handle currently held so the model
knows what to `shell_kill` first.

**Directory listing and delete go over the terminal, not a dedicated request.**
ACP's `agent → client` surface is exactly `session/request_permission`, `fs/*`
and `terminal/*` — there is no request for listing or deleting. `file_delete`,
`dir_list` and `dir_walk` run a `rm` / `find` through `terminal/*` inside an ACP
session rather than inventing a protocol-unbacked convention, and produce the
same output shape as their agent-side siblings (DFS order, `~` expanded), so the
model sees one tool either way.

**Reaching the agent's own machine is opt-in, and that closed a real hole.**
Outside an ACP session these tools reach the agent's own machine, and there they
are opt-in: the seven file and shell tools (`file_read`, `file_write`,
`file_append`, `file_delete`, `dir_list`, `dir_walk`, `shell`) require
`[tools.host_access] enabled = true`; the default is `false`, for every origin.
Before this switch existed, `Origin::Channel` (Matrix/Discord) refused only
`Execute` and `Other` calls, so `file_write` (`Edit`) and `file_delete`
(`Delete`) went through unconditionally — a Discord message asking to delete a
file reached the agent's own filesystem. Enabling host access is now a
deliberate, config-level act; running the agent in a container is the
recommended way to do it once it is on.

### Loading past sessions

An ACP session lives at `<sessions_dir>/<namespace>/acp/<id>/`, its own
tree with its own event kinds — separate from `/rpc`'s. It is routed by the
`namespace` recorded in the session's own header, not by a room id (an ACP
session has none), and it is recorded with `channel: "acp"`. Like every
session it is a directory: `session.md` holds the header (`session_id`,
`namespace`, `cwd`, `created_at`) as front matter, and each event is its own
`<uuidv7>.md` — front matter for its `kind`, the `parent` it was appended
after and its time, the message text as the Markdown body. One file per
event is what lets two hosts append to a synced session without writing the
same file. Sessions from before this layout are single `.jsonl` files; they
are read as they are and converted the first time something is written to
them ([#284](https://github.com/sapphire-turtle/sapphire-agent/issues/284)
tracks removing that). **The `parent` chain is the authority on
order, not the `id`'s embedded clock** — two writers with skewed clocks
would otherwise sort against each other wrongly.

Zed's session picker lists the connection's own conversations: `session/list`
returns every open session under the memory namespace of the connection's
room profile — the one its token picked on `/acp`, or
`[acp].stdio_room_profile` for `sapphire-agent acp` — that is the isolation boundary, not the room profile
itself, so two profiles that leave `memory_namespace` unset both land on
`default` and see each other's sessions. Zed can further narrow the list to
the project directory it has open (`cwd`), which every ACP session records
(the header field is required, not optional, since `session/new` always
reports one).
Picking one hands it to `session/load`, which replays the stored
conversation into the editor before answering the request, or to
`session/resume`, which does the same without the replay. Either way the
session is a real one afterwards — prompting it continues the existing
history rather than starting a fresh conversation.

**A per-session digest is cached outside the workspace, addressed by
session id.** `<cache_dir>/sapphire-agent/digests/<session_id>.json`
holds one intra-day digest per session — "what this session has covered
today" — overwritten in place rather than appended to. Losing it costs
only today's cross-session block — the block other rooms' system prompts
use to see what this session has been doing today. It is not the durable
record: the daily log is, built from the session's own events rather than
from the digest.

A second, similarly workspace-external location,
`<cache_dir>/sapphire-agent/tool-results/<sha256>`, is a content-addressed
cache holding a tool call's result by hash — the session's event file stores
only the tool name, the result's hash, not the result body, and the input as
given. The input has nowhere to be hashed out to the way a result does, so
it goes into the event file directly; one that serialises past the same
50,000 byte cap is elided to a small marker object on the way to disk rather
than stored, so a single oversized call cannot put its raw content into the
(indexed) workspace. An ACP session's `tool_use` and `tool_result` messages
are persisted (see "Tool calls are persisted for ACP sessions" below);
every other transport still is not.

**Both caches belong to the host, and are not synced.** The session files
are in the workspace and travel to every synced host; the caches stay on the
host that wrote them. A session continued on another host therefore sees the
results of tool calls made elsewhere as placeholders (see the next
paragraph). Whether to sync them is an open question in the
[design note](docs/superpowers/specs/2026-10-13-app-server-sync-acp-stdio-design.md).

**Losing the cache degrades a session rather than breaking it.** Delete
the directory, or lose an entry to a write failure, and the session still
loads: the missing result is rendered as a placeholder, but the `tool_use`
/ `tool_result` pair itself is kept intact. That pairing is not optional —
the Anthropic API rejects a history where a `tool_use` has no matching
`tool_result` in the very next message — so `history()` also repairs an
orphan it finds on read: a `tool_use` whose `tool_result` append never
made it to disk (a cache write that failed, or the process dying between
the two appends) gets a synthesised placeholder result spliced in
immediately after the message that carries it. This repair is read-side
only — it is never written back to the store, so a later read repeats it
if the underlying orphan is still there.

Both caches sit outside the workspace for the same reason: `<workspace>/sessions`
is in the retrieve search index, and a digest that is rewritten as the
conversation grows would otherwise pile up near-duplicate summaries inside
an indexed file and skew search — sync cost is secondary. A digest also
cannot be pruned out of the session log itself: events are chained by
`parent`, and removing one orphans its children.

**ACP sessions appear in both the cross-session "today" digest and the
daily log.** The digest is kept current by a sweep on a rolling 30-minute
cadence from process start, which regenerates only the sessions whose
newest message postdates their cached digest. `/rpc` and device-default
sessions still do not reach the daily log —
[#189](https://github.com/sapphire-turtle/sapphire-agent/issues/189).

**Tool calls are persisted for ACP sessions.** `run_llm_turn` writes
`tool_use` and `tool_result` events to the ACP session's directory (see
"A second, similarly workspace-external location" above for what an
event file stores versus the result cache). `/rpc` and device-default
sessions do not — their event format has no reference form for a result,
so writing one raw would put the content in the workspace and the
retrieve index, which is exactly what the ACP store's external cache
exists to avoid for results ([#194](https://github.com/sapphire-turtle/sapphire-agent/issues/194)).
That protection is only for results, though — a tool *input* has no
hash-and-cache indirection, so it lands in the event file directly, bounded
rather than eliminated: capped at the same 50,000-byte limit, and
elided to a marker object above it. Reloading an ACP session therefore
restores what the agent did as well as what was said — but the editor
does not currently render it: see "A loaded session's tool calls are
not shown in the editor" below.

**Opening the same session on two connections at once is not safe to
prompt from both.** Nothing stops a second Zed window — or any other ACP
client — from loading a session that is already open elsewhere, but
`run_llm_turn` clones a session's history at the start of a turn and
writes the whole vector back at the end. Two turns racing on one session
are last-writer-wins in memory, while both have already appended their own
messages to the on-disk transcript, so the two views diverge. Loading does
not create this race — `/rpc` and the voice heartbeat could already reach
it with two prompts on one connection — it makes it reachable *across*
connections too. It is not fixed here: a `session/load` or
`session/resume` that lands on an already-open session logs a `session
{id} is now open on N connections` warning, so a divergence at least
leaves a trace, but the fix itself needs a per-session lock held across a
whole turn.

### Known limitations

- **Replies are not streamed token by token.** `Provider::chat` returns a
  complete response, not a stream, so the whole reply arrives as a single
  chunk at the end of the turn. Tool calls *do* still appear progressively as
  they run.
- **No automatic reconnection.** A dropped connection does not resume on
  its own — ACP v1 has no mechanism for the editor to reconnect
  transparently. The conversation is not lost, though: reopen it through
  `session/load` or `session/resume` from Zed's session picker, see
  "Loading past sessions" above.
- **Tool calls reach the editor as a bare name.** No arguments and no results
  are sent — the shared turn executor reports only a tool's id and name — so
  Zed shows "shell" rather than the command it ran or what came back. The one
  exception is a permission request, which does carry the tool's kind and its
  raw input so you can see what you are approving.
- **A failed or refused tool is reported as completed.** The executor's
  `ToolOutput` carries no success bit, so every tool that finishes is sent to
  the editor as `completed`, whether it succeeded, errored, or was declined.
  The model still sees the reason and reacts to it; only the editor's status
  display is wrong.
- **Partly exercised against a real Zed** (2026-08-31). Confirmed working
  from an actual Zed install: the connection, `initialize`, `session/new`
  with its mode list, and the permission prompt for an `Execute` tool —
  both allowing and declining. Still unconfirmed against a real client:
  the mode picker actually switching modes, "always allow" surviving a
  reconnect, multi-turn conversations, and cancellation.
- **No token usage is reported.** The provider layer discards the `usage`
  the API returns, so neither `session/update: usage_update` nor
  `PromptResponse.usage` is sent, and the editor cannot show what a turn
  cost or how full the context is.
- **`/rpc` and device-default sessions still do not persist tool
  calls.** Only ACP sessions write `tool_use` / `tool_result` events;
  the other transports' event format has no reference form for a
  result, so a reload of one of those restores what was said but not what
  was done. Tracked as [#194](https://github.com/sapphire-turtle/sapphire-agent/issues/194).
- **A loaded session's tool calls are not shown in the editor.** Even for
  an ACP session, whose `tool_use` / `tool_result` pairs are on disk and
  do reach the model on the next turn, `session/load`'s replay only
  projects text parts to `session/update` notifications — tool calls in
  the stored history are silently skipped rather than rendered. The
  model sees them; the editor's thread view does not. Tracked as
  [#192](https://github.com/sapphire-turtle/sapphire-agent/issues/192).
- **Tool output is capped at 50,000 bytes** (roughly 20,000 from the
  head, 30,000 from the tail, with a marker spliced in between) — this
  applies to every tool, on every transport, not only the shell tool over
  ACP; `ToolSet::execute` is the one path both `/acp` and the
  Matrix/Discord turn executor call through. The head, tail and marker
  budgets are sized to sum to the cap exactly, so applying the truncation
  to an already-truncated result is a no-op rather than cutting again and
  nesting markers.

### Configuring `websocat` in Zed

One trap, since Zed spawns the command **without a shell**: do not quote
the header argument. In a terminal you would write
`-H 'Authorization: Bearer …'` and the shell would strip the quotes; in
`settings.json` the quotes become part of the header value and the server
answers `401` before the WebSocket upgrade. Write it bare:

```jsonc
"args": [
  "--text",
  "wss://your-host/acp",
  "-H",
  "Authorization: Bearer <token>"   // no surrounding quotes
]
```

`-H` takes multiple values, so it has to come *after* the URL or it
swallows it.

### Idle connections and the proxy in front

A reverse proxy terminating TLS for `/acp` will close a WebSocket that
carries no traffic, and ACP gives it none to see: an editor with its panel
open but nothing being asked is silent for as long as the person is away.
Measured against one HAProxy, the tunnel is cut at **1800.005s** of
silence.

What that looks like is worse than a disconnect. The agent logs

```
ACP: connection for room profile '…' ended with an error: Internal error: {
  "data": "IO error: Connection reset by peer (os error 104)"
}
```

while on the editor's machine `websocat` **does not exit** — its socket
sits in `CLOSE-WAIT` and the process stays up. Zed sees a live subprocess
with a writable stdin, so it keeps sending into a dead socket and never
reconnects. The symptom is an agent that has stopped answering, not an
error.

The agent prevents this by pinging on its own, every
`[acp] keepalive_secs` seconds (30 by default), so **nothing needs
configuring on the editor's side** — no `--ping-interval`, no change to
`agent_servers`. Set `keepalive_secs` lower if something in front of the
endpoint is stricter than nginx's 60s `proxy_read_timeout`, or `0` to send
no pings at all.

If you are reading this because connections still drop, the interval is
the first thing to check against the proxy's own timeout — `timeout
tunnel` in HAProxy, `proxy_read_timeout` in nginx.

## Autonomous sessions

When nothing else has touched the agent for `idle_minutes`, it picks the
highest-priority due task out of `<workspace>/autonomous/*.md` and works on it
as an ordinary session, for up to that task's `max_turns`.

Three things separate this from a heartbeat task:

- **It starts on idleness, not on a clock.** A heartbeat fires at a time; this
  waits until no other session has moved.
- **It keeps going until the task is done.** A heartbeat is one prompt; this is
  a session, run to the end of one cycle and closed when the model answers
  `DONE` or `max_turns` runs out (mid-run pausing is out of scope).
- **One task is one session.** `session_list` shows them as `server/<task>` and
  `session_read` shows the transcript, so last night’s work is something you
  read rather than something that was posted at you.

Off by default:

```toml
[autonomous]
enabled = true
idle_minutes = 30    # how quiet everything else has to be
poll_seconds = 60    # how often the loop looks for work
origin = "channel"   # "channel" (default) | "trusted"
```

`origin = "channel"` runs tasks with the same permissions as a chat message:
reads and unapproved edits, no `shell`. Tasks that need `shell` (filing an
issue, committing) need `origin = "trusted"` **and**
`[tools] host_access.enabled = true` — without the latter, the host tools are
refused before permissions are consulted at all.

While a task runs, `<workspace>/state/autonomous.json` carries a one-line
reason (`task: journal, turn 2/3`) so you can tell what the agent is doing
without reading the session. Task definitions are re-read every cycle, so
editing one takes effect without a restart.

## Editing the agent's own definitions

The files an operator hand-edits to describe the work the agent does on its
own — `<workspace>/heartbeat/*.md`, `<workspace>/autonomous/*.md` and
`<workspace>/agents/*.md` — are readable and writable from a chat room too,
through four tools:

| Tool | Actions | Definitions |
|---|---|---|
| `heartbeat_config` | `list` · `read` · `write` · `set_enabled` | `<workspace>/heartbeat/*.md` |
| `autonomous_config` | `list` · `read` · `write` · `set_enabled` | `<workspace>/autonomous/*.md` |
| `agent_config` | `list` · `read` · `write` | `<workspace>/agents/*.md` |
| `task_test` | `kind` = `heartbeat` or `autonomous`, plus `name` | runs one definition once |

- `list` reports each definition and whether it is effectively enabled;
  `read` returns one exactly as it is on disk; `write` creates or replaces
  one, refusing content the agent's own loader would drop — a definition that
  would never fire is not something you should be able to write and then
  puzzle over.
- `set_enabled` changes **only the `enabled:` line**. Comments, other keys and
  the body are left byte-for-byte as they were. That is the whole point: these
  are files people hand-edit, and the `schedule:` line usually has a comment
  above it saying why — a `write` to flip one switch would take it with it.
  A definition with no `enabled:` line at all is enabled by default. Both
  task loaders re-read their directory every cycle, so the flag takes effect
  without a restart.
- `agent_config` has no `set_enabled`, because a subagent definition has no
  `enabled:` flag to flip: it runs when it is *called*. Whether to delegate at
  all is a decision the parent model makes per call, not a switch on the file.
  A definition written with `agent_config` is offered for delegation
  immediately, without restarting the agent; one you hand-edit under
  `agents/` still needs a restart, as [Subagents](#subagents) describes.

**A room profile has to be named, in the host config.** The tools are registered
only when `[tools.admin].room_profiles` lists at least one room profile, and every
action — including the read-only `list` — is refused in a session whose profile is
not on it:

```toml
# host-local config, never the workspace's
[tools.admin]
room_profiles = ["ops", "developer"]   # [room_profile.<name>] keys, not room ids
```

With `room_profiles` absent or empty the four tools are **not registered at all** —
the model is never told they exist. A tool that is offered and then always refuses
is the worse answer: it invites a retry loop against a wall.

A **channel** turn is judged by the profile its room resolves to — the same
resolution that picks its provider and memory namespace, so an explicit listing in a
`[room_profile.<name>].rooms` array wins and `[room_profile.default]` catches every
room no profile claims. An **ACP** session (Zed and other editors) is judged by the
profile its bearer token pinned at `session/new`, which is what makes these tools
usable from an editor at all: ACP has no room id. Note what `room_profiles =
["default"]` therefore means — every room no other profile claims — and list the
specific profiles you mean unless that is what you want.

Voice, `/rpc`, A2A and the unattended loops (heartbeat, autonomous) name no profile
an operator declared, so the tools are refused there even when the list is not empty.
A heartbeat or autonomous task that fires *into* a room of a listed profile is the
exception, and the reason a listed profile's rooms should not also be a task's
delivery target: the grant follows the room, not the caller, so such a turn can
author unattended work with nobody watching.

`[tools]` is deliberately outside the workspace layer's allowlist, so a synced
workspace `config.toml` cannot grant itself this — a `[tools.admin]` written there is
dropped and named in a startup warning, the same as any other key that layer may not
set. Who may run these tools is the same kind of host-local decision as the API key or
the bind address, and it has to be: a listed profile can rewrite, through the agent,
what runs unattended. **Pair it with that profile's own `[room_profile.<name>].devices`
or the room's `allowed_users`**, or everyone who can reach a session of that profile
can author code-adjacent work that runs with nobody watching. `[tools.admin].rooms`
from an earlier build is rejected at startup and names `room_profiles` in the error.

**Trying a task before enabling it.** `task_test` runs one heartbeat or
autonomous definition once, in a throwaway session, so a task can be
exercised while it is still `enabled: false` — the case it exists for. The
run is production's in every respect that matters: the same model, the same
prompt assembly, the same permission row (`[autonomous] origin`). Three
things differ:

- It stops after **3 turns** at most. A heartbeat task is one prompt and is
  always exactly one turn; a definition's own `max_turns` can only lower the
  autonomous cap, never raise it. A tool for checking that a task does not eat
  tokens must not eat tokens itself.
- Its session is named `test:<kind>:<name>`, **not** after the task, so the
  task's cooldown is untouched. Testing a task does not postpone it.
- **Delivery is not verified.** A heartbeat task's `room_id:` / `voice:`
  targets are ignored: what is checked is the prompt and how the run ends, not
  where a result would go.

The report names the session it created, so the transcript can be read
afterwards with `session_read`.

## Skills

A skill is a written procedure for a kind of work — planning, TDD,
debugging, code review, finishing a branch — that the model can pull up and
follow instead of improvising. Skills come from a checkout that lives on the
**editor's** machine, not this agent's: this crate has no lib and no
built-in skill content of its own, and the checkout is intended to be
[obra/superpowers](https://github.com/obra/superpowers) or a compatible
directory, though nothing here names that project in code.

**Skills require an ACP client whose editor declared terminal support.**
There is no ACP call to list a directory — no list, glob or stat exists in
the agent→client surface — so resolving and indexing the skills directory
always runs a shell script on the client over its terminal capability. That
makes every skill tool, including the read-only `skill`, depend on it. In
practice this means Matrix, Discord and voice never see any skill tool at
all: `Agent::handle_message` (`src/agent.rs`) passes every client
capability flag as `false`, and `src/agent.rs` is not touched by this
feature. Enforcement is in two places, deliberately:

- an arm in `visible_tool_predicate` (`src/serve/mod.rs`) gating
  `skill`/`skill_install`/`skill_update`/`skill_uninstall` on
  `has_client && client_terminal` — the same function `src/agent.rs` calls,
  which is what closes off the channel transports without editing that
  file;
- a per-namespace switch composed at the `run_llm_turn` call site
  (`src/serve/mod.rs`), layered on top, so skills can be off for a
  namespace even on a fully client-capable ACP connection.

### Where the directory lives

The server stores no path — it serves editors on different machines and
operating systems, so any path in its own config would be right for at most
one of them. Instead, a fixed shell script runs on the client and resolves
one of these, first hit wins:

| Order | Candidate |
|---|---|
| 1 | `$SAPPHIRE_AGENT_SKILLS_DIR`, if set |
| 2 | `$APPDATA/sapphire-agent/skills` (Windows) |
| 3 | `$HOME/Library/Application Support/sapphire-agent/skills` (macOS) |
| 4 | `${XDG_DATA_HOME:-$HOME/.local/share}/sapphire-agent/skills` (Linux / BSD) |

`SAPPHIRE_AGENT_SKILLS_DIR` is set **on the client**, by whoever's machine it
is — it is the escape hatch for a checkout that already lives somewhere
else, or for a client environment (a stripped-down service account, an
unusual `XDG_DATA_HOME`) the convention above gets wrong. The resolved
directory and index are cached per ACP session (keyed by
`TurnContext::session_id`, capped at 128 sessions) and reused for later
`skill()` calls in the same session; a call with no session to key on (a
subagent's own nested tool call) always re-resolves rather than risking a
shared cache entry.

`skill_install`/`skill_update`/`skill_uninstall` resolve the directory
through a second script rather than this read-only one, because they are
also the ones allowed to create it — but that script walks the same four
candidates in the same order, and picks the same **first-existing-wins**
one this table describes. It only falls back to creating the first
*eligible* candidate (base variable set) when none of the four already
exists. The two scripts used to disagree — the write side picked on
eligibility alone, with no existence check — which could make a mutating
call `mkdir -p` a second, empty directory beside a real, already-populated
one instead of finding it; they are kept in sync now, on purpose.

### Enabling skills

Off by default, per memory namespace — the same discipline `using-superpowers`
imposes (check for a relevant skill before answering *at all*) is right for
development work and wrong for an everyday conversation:

```toml
[memory_namespace.dev]
skills = true   # default false
```

Two gates, both required: this flag, and an ACP client that declared terminal
support. When the tools are missing, the connection log says which gate is shut
— every `initialize` records what the client can do:

```
INFO ACP: connection accepted for room profile 'dev'
INFO ACP: Zed 0.209.3 initialized: fs.read=true fs.write=true terminal=true
```

A client with `terminal=false` will never be offered them however the namespace
is configured; with `terminal=true` and no tools in sight, the namespace flag is
the one to check — and misspelling it or putting it under `[room_profile.<name>]`
now warns at startup (see [Keys nothing reads](#keys-nothing-reads)).

### The four tools

| Tool | `ToolKind` | Does |
|---|---|---|
| `skill()` / `skill(name)` | `Read` | No argument: lists every skill's name and description. With a name: returns that skill's `SKILL.md` body, prefixed with the skill's absolute directory on the editor's machine (skills reference sibling files by relative path — `./implementer-prompt.md`, `references/…` — so the model needs to be told where it is to resolve those). |
| `skill_install(url)` | `Execute` | `git clone`s an `https://` URL into the skills directory. Refuses if that source is already installed. |
| `skill_update(name?)` | `Execute` | `git pull --ff-only` on one installed source, or on every installed source when `name` is omitted; one entry failing does not stop the rest. |
| `skill_uninstall(name)` | `Delete` | Removes one installed source. **No `force` parameter exists.** A checkout with uncommitted local changes is always refused — full stop, no override — and the person resolves it themselves (`git stash`, a commit, or their own `rm -rf`) on their own machine. This is deliberate: `ToolKind::Delete` is *allowed* rather than asked about under `Origin::Acp(AcceptEdits)` (unlike `Execute`, which is asked), so a model-settable `force` would let the model discard someone's uncommitted edits with nobody asked. |

All three — not just `skill_install` — resolve the skills directory through
the create-or-resolve script above, so `skill_update(name)` and
`skill_uninstall(name)` will also `mkdir -p` it if (and only if) none of the
four candidates exists yet, before going on to report that `name` isn't
installed there.

Install/update/uninstall all run through the same permission path as any
other `Execute`/`Delete` call, per the [permission table](#permission-and-modes)
above: `default` asks about all three; `accept_edits` still asks for
`skill_install`/`skill_update` (`Execute`) but runs `skill_uninstall`
(`Delete`) unasked, the same as any other edit or delete once edits are
accepted; `bypass` runs all three unasked.

**Only `https://` source URLs are accepted** for `skill_install`, and the
same check re-validates the stored remote before `skill_update` pulls.
Rejected: `ext::…` (git treats it as a command to run — remote code
execution by design), `file://`, `git://`, `ssh://`, scp-like
`user@host:path`, and anything starting with `-` (which could be
reparsed as a flag). The URL is passed after a `--` separator on the `git`
command line regardless, so a check that was somehow missed still can't turn
it into an option.

The destination directory name (`skill_install`) and the entry name
(`skill_uninstall`) are never taken from the model as a path — they are
derived (from the URL's final path segment) or supplied as a bare name, then
constrained to `[A-Za-z0-9._-]`, rejected if empty, `.`, `..`, leading `-`,
trailing `.`/space, or a Windows reserved device name (`CON`, `PRN`, `AUX`,
`NUL`, `COM1`–`COM9`, `LPT1`–`LPT9`, matched case-insensitively and against
the segment before the first `.` too, so `CON.txt` is caught as well as
`CON`).

**Installing needs `git` and a shell on the editor's machine.** Every `git`
invocation runs with `GIT_TERMINAL_PROMPT=0` (so a private repository
refuses cleanly instead of hanging a terminal the model can't answer) and
`GIT_ALLOW_PROTOCOL=https` — the setting that actually closes every
config-based route to a non-`https` transport (`branch.<current>.remote`,
`url.<base>.insteadOf`), enforced by git itself at the point it opens a
transport regardless of which config field carried the bad URL. The
pre-pull remote-URL re-check (above) is a cheap, specific sanity check on
top of that, not what makes a doctored `.git/config` safe on its own — see
`docs/superpowers/specs/2026-09-02-skills-and-subagent-resume-design.md`'s
`## 実装時の訂正` section for the detail. `git pull` also runs the
checkout's own `.git/hooks/post-merge` if the pull changed anything; no URL
or protocol guard prevents that, and none is meant to — it's the person's
own machine and their own checkout.

### The two accepted layouts

A checkout is not itself a skill — the index walks two shapes, so a
hand-written local skill can sit beside an installed bundle:

- `<dir>/<name>/SKILL.md` — a skill placed directly under the skills
  directory.
- `<dir>/<repo>/skills/<name>/SKILL.md` — a cloned bundle that declares its
  own `skills/` subdirectory (the shape superpowers itself uses).

### How a skill's body is read

`fs/read_text_file` first, falling back to `cat` over the client terminal on
any failure. This is the expected path, not a fallback for a rare case: an
editor may scope `fs/read_text_file` to the open project, and the skills
checkout is deliberately outside it.

## License

Licensed under either of:

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT License ([LICENSE-MIT](LICENSE-MIT))

at your option.
