# Session profiles and permission profiles: unifying the profile system and splitting out the tool-allow table

> **Glossary** (the word "profile" works at two levels): an **LLM profile**
> (`[profiles.<n>]`: model/provider config), a **session profile**
> (`[session_profile.<n>]`: what kind of session this is), and a **permission
> profile** (`[permission_profile.<n>]`: which tool groups it lists).

## Background (policy)

Today a Discord channel turn can run neither `shell` nor `mcp_reconnect`:

- `Origin::Channel` is always refused for `Execute`/`Other` by `decide()`'s strict table.
- While `host_access.enabled` (default false) is off, `host_tool_denied` refuses the fs/shell tools before the table is even consulted.
- `mcp_reconnect` is `Other`, so the same applies.

But this is also a problem of the structure itself: the table's rows are the two axes "origin x ToolKind", and the room profile only ever takes effect through a `rooms` list. Now that more than rooms drive sessions (heartbeat, autonomous, ACP, subagents), a design that resolves profiles through a room list is exactly why each gate grows its own duplicated resolution path per loop.

Policy:

1. **Rename `room_profile` to `session_profile`, and move the binding to the session-declaration side.** Rather than the profile being looked up from a room, each session (a channel room, a heartbeat task, an autonomous task, an ACP session) declares its own profile.
2. **Split tool allow/deny out into `permission_profile`.** The session profile only says which permission profile it uses; the allow table is a separate object in the synced config. Same precedent as memory namespaces, which are separate objects referenced by name.
3. **Reconnect tools live inside the group of the surface they reconnect.** Even a read-only MCP session can drop its connection and need reconnecting, so wherever that MCP session is allowed, its reconnect runs under the same single key.

## Decisions

### 1. `[room_profile.<n>]` is renamed to `[session_profile.<n>]`

`RoomProfileConfig` → `SessionProfileConfig`. The fields stay nearly as they are (`profile`: the LLM profile name, `memory_namespace`, `session_policy`, `voice_pipeline`, `devices`) and `rooms` is dropped. A room is no longer spoken of by a list on the profile side.

```toml
[session_profile.default]
profile = "default"          # names [profiles.<n>], the LLM profile
memory_namespace = "default"
session_policy = "reset"
devices = ["saphuna-linux"]
```

The word `profile` thus works at two levels (a session's `profile` names a session profile; a session profile's `profile` names an LLM profile). That is the consequence of declaration-side binding, and the two are told apart by simply looking at the section they sit in. State this in the docs.

### 2. A session declares its own profile

A room carries its own block, and that block names the profile:

```toml
[discord]
bot_token = "..."
enabled = true

[discord.room1]
room_id = "1474368482296807444"
profile = "default"
voice_pipeline = "default"   # a room with a voice path writes it here
```

Matrix is the same shape (`[matrix.room1] room_id = "!xxx:srv" profile = "..."`). Heartbeat and autonomous frontmatters keep `room_id` as delivery-only (where the fire is delivered) and gain `profile` (the session profile that governs the run). Absent means the implicit `"default"`.

- What ingress resolves and the turn carries is **the resolved profile name only**. `Agent::handle_message` (the Matrix/Discord loop) and `run_llm_turn` (the ACP/autonomous loop) each build an `Origin` carrying the resolved name and call the one same decision function — see Design, `policy.rs`: one function, one name, one signature.
- ACP keeps its existing session pin as the only source of truth (the principle of not adding sources of truth is unchanged). `[acp].stdio_room_profile` is renamed `stdio_session_profile`.
- voice, `/rpc` and A2A keep being governed by the profile their session belongs to (voice reverse-looked-up from the `voice/subscribe` registration; `/rpc`/A2A through the device→profile binding).

### 3. The allow table is written by `permission_profile`. Three presets; the implicit default is `default`

```toml
[permission_profile.admin]
tools = ["fs", "shell", "mcp", "admin"]   # everything

[permission_profile.default]
tools = ["fs", "mcp"]                       # when a session profile names no `permission`

[permission_profile.chat]
tools = []                                   # only the always-allowed tools
```

- `session_profile.<n>.permission: Option<String>`. Absent means the implicit `"default"`.
- Resolution precedence: **an operator-written `[permission_profile.<n>]]` section wins over the built-in preset of the same name; the built-in table is the fallback.** Same shape as the LLM-profile resolution.
- A session profile's `profile` names an entry that does not exist anywhere (built-in or operator) is a startup error — a typo must not rot into "I granted it but nothing changed".
- `shell` is **not** in `default`, deliberately: a shell turn can read and rewrite `heartbeat/*.md` and the other admin-domain definition files raw, bypassing the dedicated admin tools' domain. Only a profile that explicitly names `shell` (in practice: `admin`) gains it.
- **Always allowed (outside the table; permission is not their subject)**: `current_time`, `weather`, `workspace_search`, `skill`, `subagent` (inherits the delegating turn), and the memory tools. The memory namespace comes from the session profile's `memory_namespace` (existing `namespace_for_room_profile` → `namespace_for_session_profile`).

This **changes behaviour**. The implicit profile's `default` lists `fs`, so a Discord turn on a host where `[tools.fs].enabled = true` runs `file_write` unasked (unchanged), but where today `Execute`/`Other` is always refused, nothing under `fs` is refused by the row any more — and `shell` is refused until a profile names it. This is the explicit consequence of #257's policy ("let the agent use fs and shell freely; do the privilege separation in profiles"). A room that must stay narrow (nsfw, say) writes `profile = "chat"` or names its own permission profile.

### 4. The group is the registration unit; the host side is only enabled-per-group

```toml
# host-local config (HOST_ONLY_PATHS: the per-group `enabled` keys under [tools]
# stay host-only; the `tools` listings are synced config)
[tools.fs]
enabled = true

[tools.shell]
enabled = true
```

- **Allowed ⇔ the turn's permission profile lists the group ∧ the group is `enabled` on this host.** Two keys: "does this machine hand the agent that key" is the host's decision, and a synced file must not declare it across hosts (the current `host_access` host-only property is kept).
- The registration condition keeps the current `[tools.admin]` philosophy — *not registered at all*: a group's tools are registered only when the group is `enabled` on this host **and** listed by at least one permission profile (presets included). "Registered but always refused" makes the model believe a tool exists that cannot work. Registration is workspace-wide; visibility for one turn further requires that turn's profile to list the group.
- Four group names, and they are literally today's section names:
  - `fs` — the file half of `HOST_TOOLS` (`file_read`/`file_write`/`file_append`/`file_delete`/`dir_list`/`dir_walk`/`image_read`). Routing of the same names to the editor's machine through ACP capabilities is unchanged (capabilities and groups are orthogonal).
  - `shell` — `shell` (and the background-shell path).
  - `mcp` — the tools of `[tools.mcp_servers]` servers (`mcp__<name>__*`) and `mcp_reconnect` (decision 5).
  - `admin` — `heartbeat_config`/`autonomous_config`/`agent_config`/`task_test`.
- `McpTrust` does not disappear: per-server trust keeps deciding the kind classification *inside* the group (read→`Read` etc.). A turn allowed the group passes the read-only server's tools and its reconnect alike on the one key.

### 5. `mcp_reconnect` lives in the `mcp` group

Today `mcp_reconnect` is registered whenever at least one MCP server is, and is struck by the strict table as `Other`. Under the new design it is registered **as a tool of the `mcp` group**: a turn whose permission profile lists `mcp` — including one running a `trust = "read"` server — can reconnect the session it is already allowed to use. No new tool names for surface splitting: outbound (`[tools.mcp_servers]`) is all that exists today, and the inbound bridge surface already lives in ACP capability land, outside the group table.

### 6. Definition files are the admin group's land

`heartbeat/*.md`, `autonomous/*.md`, `agents/*.md` and the config definition files themselves are **modified** only by the admin group's dedicated tools. The guard covers the fs tools' *write* half (`file_write`, `file_append`, `file_delete`), which refuse a definition-file path with a message naming the dedicated tool that owns it; the read half (`file_read`, `dir_list`, `dir_walk`, `image_read`) keeps reading them (reading one's own task is not a rewrite risk). This pins "a heartbeat session cannot rewrite its own definitions by default" on both paths:

- an unattended task that writes no `profile` runs under the implicit `default`, which does not list `admin` → it cannot even see the admin tools; and it has no `shell` to go around them with (decision 3).
- even a turn that does hold `fs` cannot *modify* the definitions through it.

### 7. `kind` survives; only the routing tables die

`kind` stays a per-tool attribute and still drives ACP mode asking exactly as today (under a mode that asks, `Write`/`Execute`/`Other` produces `Ask`, `Read` produces `Allow`). What disappears is the name→kind routing tables: registration carries the group, and the group decides whether a tool is offered at all. `mcp_reconnect` stays `Other` and is therefore asked under an ACP mode that asks, as are the trust-classified tools of its group.

### 8. Migration fails loudly (precedent)

`Config::migration_errors` names and stops startup on:

1. a leftover `[room_profile.<n>]` (rename it to `session_profile`)
2. a section still carrying `rooms` (move it to declaration-side binding)
3. a leftover `[tools.admin].room_profiles` (move it to `permission_profile.<n>.tools`)
4. a session (room block, heartbeat/autonomous frontmatter, or pin) resolving to `"default"` while no `[session_profile.default]` is defined by the host

`room_id` inside a heartbeat/autonomous frontmatter is *not* a migration error: it keeps parsing, now meaning delivery-only.

## Requirements

### Functional

1. R1: `[session_profile.<n>]` drives a session, and the session declares the profile name — channel room blocks, heartbeat/autonomous frontmatter, the ACP pin, device bindings. No profile-side room list exists. An unknown profile name (explicit or the implicit `default` when the host defines none) fails startup.
2. R2: A turn's allowed set = "groups listed by its permission profile ∧ groups enabled on this host". One single pure function in `policy`; both turn loops call it.
3. R3: Presets `admin` (every group) / `default` (`fs`, `mcp`; not `shell`, not `admin`) / `chat` (empty). The implicit value when `permission` is absent is `default`. An operator section overrides a preset of the same name.
4. R4: Always-allowed tools (`current_time`/`weather`/search/skill/memory/delegation) are outside the table's subject. Memory uses the session profile's namespace.
5. R5: `mcp__<name>__*` and `mcp_reconnect` are the `mcp` group; trust still decides the kind classification inside the group.
6. R6: Definition files (heartbeat/autonomous/agents/config) are modifiable only by the admin group's dedicated tools; the fs write tools refuse them, the fs read tools read them.
7. R7: A group listed by no permission profile (presets and operator sections included) is not registered at all.
8. R8: Migration leftovers are named by `verify` and fail serve startup.
9. R9: ACP keeps capability routing, `session/request_permission`, and the Ask semantics of ACP modes (Default asks before edits, and so on), computed from `kind` exactly as today (decision 7). The group table decides *whether a tool is offered at all*; the mode decides *whether an offered one asks*.

### Non-functional

- N1: Code, comments, commit messages and `docs/**` (specs, plans) are English.
- N2: One decision function. `run_llm_turn` and `Agent::handle_message` each call it with their own origin (the resolved profile name + the host keys) and nothing else.
- N3: The execution half of #257's policy lands as execution-user separation; this change does not move process privileges, but the gate's shape must not obstruct it.
- N4: Existing test style (`Config::parse_for_test`, `ServeState::for_test_scripted*` + `StubProvider`, per-instance probe tools with a handle, end-to-end through execution rather than inspecting fields).
- N5: `cargo fmt --all`; `cargo clippy -p sapphire-agent-server -p sapphire-agent-config --all-targets` (zero warnings); `cargo test -p sapphire-agent-config -p sapphire-agent-server`.

## Design

### config.rs

```rust
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct SessionProfileConfig {
    /// Name of the LLM profile ([profiles.<n>]) driving this profile's turns.
    pub profile: String,
    #[serde(default)]
    pub memory_namespace: Option<String>,
    #[serde(default)]
    pub session_policy: Option<SessionPolicy>,
    /// Permission profile name. Absent means `"default"`.
    #[serde(default)]
    pub permission: Option<String>,
    #[serde(default)]
    pub voice_pipeline: Option<String>,
    #[serde(default)]
    pub devices: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct PermissionProfileConfig {
    /// Tool groups this permission profile allows.
    #[serde(default)]
    pub tools: Vec<ToolGroup>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolGroup { Fs, Shell, Mcp, Admin }
```

- `Config::session_profiles: HashMap<String, SessionProfileConfig>`, `Config::permission_profiles: HashMap<String, PermissionProfileConfig>`.
- `permission_for(name)` returns the operator section when one exists, else the built-in preset (`admin`/`default`/`chat`).
- `[channel.<x>]`: `enabled` plus room blocks (`room_id`/`profile`/`voice_pipeline`). `allowed_users` stays channel-level (room-level override is out of scope). The subscription itself stays untouched; only the profile resolution of the room a voice actually runs in moves to declaration-side.
- `HOST_ONLY_PATHS`: the per-group `enabled` keys under `[tools]` remain host-only; the `tools` listings are synced config.

### policy.rs

- `Origin` becomes **a type that carries the resolved profile name** (`Acp { mode, session_profile }` / `Channel { session_profile }` / `Unattended { session_profile }`, shaped at ingress: `handle_message`'s entry, `run_llm_turn`'s pin clone, frontmatter).
- One decision function, one name:

```rust
/// Allowed ⇔ `profile` lists `group` and `group` is enabled on this host.
/// The single decision function; both turn loops build their `Origin`'s
/// resolved profile and pass it here with the tool's registration entry.
pub fn allow(profile: &ResolvedPermission, entry: &ToolEntry) -> Allow
```

  `ToolEntry` carries what registration knew: the tool's `ToolGroup` and this host's `enabled` flag for it. `host_tool_denied` is absorbed into that flag ("a fact about the deployment, not a row in the table" is kept).
- Ask semantics: `allow` computes from `kind` + the ACP mode exactly as `decide` does today; channel and unattended turns never ask (`partition_without_asking` sees only Allow/Deny).

### tools/

- `config_tools.rs`: the `current_admin_room_profile()` task-locals become `current_session_profile()`/`current_permission_profile()`. `ROOM_REFUSAL` names `permission_profile`. `register_admin_tools`' condition becomes "`admin` is enabled on this host ∧ some permission profile lists `admin`".
- Definition-file guard: a new refusal for when a fs *write* tool receives a definition-file path, naming the dedicated tool that owns it (`heartbeat_config` and friends).

### lib.rs (startup)

- Group registration: only groups satisfying `enabled ∧ listed` are registered — same shape as today's "registered when the tavily key exists".
- Heartbeat/autonomous loaders: frontmatter `profile` is carried onto `HeartbeatTask`/`AutonomousTask` and burned into the `Origin` when the turn is built. A new `validate_*` (same shape as `validate_subagent_profiles`) reports unknown profile names in task frontmatter at startup.

### Tests

- `Config::parse_for_test` (in `sapphire-agent-config`) pins session profiles, permission profiles, and each migration case.
- The two-loops-merged end-to-end: through `handle_message`, that a room under `profile = "default"` runs `file_write` unasked on a host with fs enabled; that under `chat` even `fs` is invisible; that a turn under `default` cannot see the admin tools even where another profile (the `admin` preset) made them registered; that `shell` refuses under `default` and runs under `admin` — verifying the same function `run_llm_turn` goes through produces the same behaviour (through execution, not by reading fields).
- The guard: `file_write` refuses `heartbeat/morning_call.md` naming `heartbeat_config`; `file_read` reads it; only `heartbeat_config` modifies it.
