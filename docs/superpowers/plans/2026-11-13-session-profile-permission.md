# Session profiles and permission profiles Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task.
> Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the room-profile/origin×kind permission system with `[session_profile.<n>]` (a session declares its own profile) + `[permission_profile.<n>].tools = [<groups>]` (the allow table), so a turn's allowed set is "groups listed by its permission profile ∧ groups enabled on this host", decided by exactly one function on both turn loops.

**Architecture:** `config.rs` gains `ToolGroup`/`SessionProfileConfig`/`PermissionProfileConfig` and the presets, and stops resolving profiles from a room list: channel configs become `[discord.room1]`-shaped room blocks, and heartbeat/autonomous frontmatter gains `profile` (`room_id` becomes delivery-only). `policy.rs` becomes the single decision function `allow(profile, entry)` over a `ResolvedPermission` + the tool's registration `ToolEntry{group, enabled, kind}`, replacing `decide`/`host_tool_denied`/`kind_of`/`HOST_TOOLS`. `tools/mod.rs` carries `kind` and `group` on `ToolSpec` itself at registration time, registers a group only when `enabled ∧ listed`, and refuses definition-file writes. Definition files stay the `admin` group's land: the fs *write* tools refuse them, the read half keeps reading them.

**Tech Stack:** Rust (tokio task-local, serde/toml, async-trait, serde_json), the existing `Config` / `ToolSet` / `Tool` / `TurnLoop` / `TurnContext` machinery, `StubProvider`-based scripted turn tests.

**Spec:** `docs/superpowers/specs/2026-11-13-session-profile-permission-design.md` — the single authority; decisions 1-8, R1-R9, N1-N5 and the Design section are implemented exactly as written there.

## Global Constraints

- **N1**: Code, comments, commit messages and `docs/**` (specs, plans) are English.
- **N2**: One decision function. `run_llm_turn` and `Agent::handle_message` each call it with their own origin (the resolved profile name + the host keys) and nothing else.
- **N3**: The execution half of #257's policy lands as execution-user separation; this change does not move process privileges, but the gate's shape must not obstruct it.
- **N4**: Existing test style (`Config::parse_for_test`, `ServeState::for_test_scripted*` + `StubProvider`, per-instance probe tools with a handle, end-to-end through execution rather than inspecting fields).
- **N5**: `cargo fmt --all`; `cargo clippy --all-targets` (zero warnings); `cargo test -p sapphire-agent-server`. (The spec names `-p sapphire-agent-config` too; this repo has **no separate config crate** — config is the `src/config.rs` module of `sapphire-agent-server` — so every command below targets that single package.)
- Commits are conventional commits, English, each ending with the trailer line `Co-authored-by: Saphina <spph12@sapphire-turtle.org>`.
- The four group names are literally today's section names: `fs` (the file half of `HOST_TOOLS`), `shell`, `mcp` (`[tools.mcp_servers]` tools + `mcp_reconnect`), `admin` (`heartbeat_config`/`autonomous_config`/`agent_config`/`task_test`). A group's per-group `enabled` key stays **host-only** (`HOST_ONLY_PATHS`); the `tools` listings (permission profiles) are synced config.
- Registration philosophy is unchanged (decision 4): a group listed by no permission profile and every profile unlisted alike is **not registered at all** — "registered but always refused" makes the model believe a tool exists that cannot work.
- The always-allowed tools stay **outside** the table (`group: None`): `current_time`, `weather`, `web_search`, `workspace_search`, `workspace_sync`, `skill`+`skill_install`/`skill_update`/`skill_uninstall`, `subagent` (inherits the delegating turn), the memory tools, and everything else the spec does not name — the spec names exactly four groups' members, so anything outside those four lists is not the table's subject. Memory namespace comes from the session profile (`namespace_for_room_profile` → `namespace_for_session_profile`).
- Migration fails loudly (decision 8): each leftover names the key and stops startup; `room_id` in a heartbeat/autonomous frontmatter keeps parsing, now meaning delivery-only.
- Routing of the unified names to the editor's machine through ACP capabilities is unchanged (capabilities and groups are orthogonal, R9): an ACP turn with a client is exactly as decided by the editor's capabilities today; `routed_to_client` survives as "this turn's calls land elsewhere", and a turn with no client is decided by the group table alone.

## File Structure

| File | Responsibility in this plan |
|---|---|
| `server/src/config.rs` | `ToolGroup`, `SessionProfileConfig`, `PermissionProfileConfig`, presets + precedence, per-group `[tools.<group>]` enabled blocks, room blocks on `[discord]`/`[matrix]`, the four `migration_errors` cases, `validate_*`. |
| `server/src/config_layer.rs` | `HOST_ONLY_PATHS` gains the per-group `[tools.<group>]` paths. |
| `server/src/tools/policy.rs` | `Origin` (carries the resolved profile name), `Allow`, `ResolvedPermission`, `ToolEntry`, the single `allow`, `partition_without_asking`. `decide`/`host_tool_denied`/`kind_of`/`HOST_TOOLS` die. |
| `server/src/tools/mod.rs` | `ToolSpec{kind, group}`; `ToolSet::entries()`; `default_tool_set` registers per group. |
| `server/src/tools/config_tools.rs` | `current_session_profile()`/`current_permission_profile()`, the refusal naming `permission_profile`, `register_admin_tools`' condition, `definition_owner` (the write guard). |
| `server/src/tools/builtin_tools.rs` | the fs write tools call the guard; per-spec `kind`+`group` literals. |
| `server/src/heartbeat_config.rs`, `autonomous_config.rs` | frontmatter gains `profile`; carried onto `HeartbeatTask`/`AutonomousTask`. |
| `server/src/lib.rs` | group registration at startup; `validate_session_profiles`/`validate_task_profiles`; loaders carry the profile. |
| `server/src/agent.rs` | `handle_message` builds `Origin::Channel { session_profile }` and calls `allow`. |
| `server/src/serve/mod.rs` | `run_llm_turn`'s pin clone → `Origin::Acp/Unattended`; `visible_tool_predicate` and the turn gate go through `allow`. |
| `server/config.example.toml`, `server/templates/workspace/config.toml`, `README.md` | the new keys, documented in English. |

---

## Task 1: `ToolGroup`, `SessionProfileConfig`, `PermissionProfileConfig`, presets and precedence

**Files:**
- Modify: `crates/sapphire-agent-server/src/config.rs` (new types beside today's `room_profiles`; nothing is renamed yet in this task, so every existing call site and test keeps compiling)
- Test: inline `#[cfg(test)] mod tests` of `config.rs`

**Interfaces:**
- Consumes: nothing (this is the first task).
- Produces: `ToolGroup{Fs,Shell,Mcp,Admin}` (Copy+Eq), `SessionProfileConfig{profile, memory_namespace, session_policy, permission, voice_pipeline, devices}`, `PermissionProfileConfig{tools}`, `Config::permission_profiles`, `Config::session_profiles` (lands in Task 2 as the rename of `room_profiles`), `Config::permission_profile(&self, name) -> PermissionProfileConfig`, `PermissionProfileConfig::lists(&self, ToolGroup) -> bool`, `Config::group_enabled(&self, ToolGroup) -> bool`, `Config::group_registered(&self, ToolGroup) -> bool` (enabled ∧ listed by at least one effective profile — R7), `Config::session_profile_for(&self, declared: Option<&str>) -> String` (declared wins; absent means the implicit `DEFAULT_PROFILE_NAME`). Task 2-6 depend on exactly these names.

- [ ] **Step 1: Write the failing tests** — append to `config.rs`'s `mod tests`, real TOML, no stubs:

```rust
    /// R3: the three presets, and `permission` absent means the implicit `"default"`.
    #[test]
    fn permission_profile_presets_are_admin_default_and_chat() {
        let cfg = Config::for_test();
        assert_eq!(cfg.permission_profile("admin").tools,
            vec![ToolGroup::Fs, ToolGroup::Shell, ToolGroup::Mcp, ToolGroup::Admin]);
        // R3: `default` is fs + mcp — deliberately not `shell` (decision 3: a shell
        // turn could rewrite the admin-domain definitions raw) and not `admin`.
        assert_eq!(cfg.permission_profile("default").tools, vec![ToolGroup::Fs, ToolGroup::Mcp]);
        assert!(cfg.permission_profile("chat").tools.is_empty());
        assert!(!cfg.permission_profile("default").lists(ToolGroup::Shell));
        assert!(cfg.permission_profile("default").lists(ToolGroup::Fs));
        assert!(cfg.permission_profile("default").lists(ToolGroup::Mcp));
    }

    /// Decision 3: an operator-written section wins over the built-in preset of the same
    /// name, and the built-in table is the fallback — same shape as the LLM-profile
    /// resolution.
    #[test]
    fn an_operator_permission_profile_overrides_a_preset_of_the_same_name() {
        let cfg = Config::parse_for_test(
            "[anthropic]\napi_key = \"test\"\n\n[permission_profile.default]\ntools = [\"shell\"]\n",
        );
        assert_eq!(cfg.permission_profile("default").tools, vec![ToolGroup::Shell]);
    }

    /// R2/R7: the host key and the listing are two different keys.
    #[test]
    fn a_group_is_registered_only_when_enabled_here_and_listed_somewhere() {
        let cfg = Config::parse_for_test(
            "[anthropic]\napi_key = \"test\"\n\n[tools.fs]\nenabled = true\n",
        );
        // Listed by the `default` preset, and enabled on this host: registered.
        assert!(cfg.group_registered(ToolGroup::Fs));
        // Enabled nowhere and listed by nobody (the `chat` preset is empty): not registered.
        let none = Config::parse_for_test(
            "[anthropic]\napi_key = \"test\"\n\n[permission_profile.default]\ntools = []\n",
        );
        assert!(!none.group_registered(ToolGroup::Admin));
        assert!(none.group_registered(ToolGroup::Fs));
    }

    /// A session profile carries `permission`, and absent means `"default"` (decision 3).
    #[test]
    fn a_session_profile_section_parses_and_names_its_permission_profile() {
        let cfg = Config::parse_for_test(
            "[anthropic]\napi_key = \"test\"\n\n[session_profile.ops]\nprofile = \"dev\"\npermission = \"admin\"\n",
        );
        let sp = cfg.session_profile("ops").expect("the section parses");
        assert_eq!(sp.profile, "dev");
        assert_eq!(sp.permission.as_deref(), Some("admin"));
        assert_eq!(cfg.session_profile_for(Some("ops")), "ops");
        assert_eq!(cfg.session_profile_for(None), DEFAULT_PROFILE_NAME);
    }
```

- [ ] **Step 2: Run them to verify they fail** — `cargo test -p sapphire-agent-server config:: 2>&1 | tail -20`. Expected: compile error, `cannot find type \`ToolGroup\` in this scope`.

- [ ] **Step 3: Write the types** — in `config.rs`, beside `SessionPolicy` (`:186`) and the `Config` struct:

```rust
/// One of the four tool groups (decision 4). The names are literally the
/// config section names, so an operator writing `[permission_profile.<n>]`
/// is naming the same unit every `[tools.<group>]` speaks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolGroup {
    Fs,
    Shell,
    Mcp,
    Admin,
}

/// `[permission_profile.<n>]` — which groups a session may use (decision 3).
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct PermissionProfileConfig {
    /// Tool groups this permission profile allows.
    #[serde(default)]
    pub tools: Vec<ToolGroup>,
}

/// `[session_profile.<n>]` — what kind of session this is (decision 1).
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct SessionProfileConfig {
    /// Name of the LLM profile (`[profiles.<n>]`) driving this profile's turns.
    pub profile: String,
    #[serde(default)]
    pub memory_namespace: Option<String>,
    /// Override the day-boundary session policy. Falls through to `Config.session_policy`.
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
```

and the built-in permission-profile table, exactly as decision 3 writes it:

```rust
/// The built-in permission profiles: the fallback under an operator section
/// of the same name. The `default` preset is what a session profile names when
/// it writes no `permission`, and it lists `fs` and `mcp` — never `shell`,
/// never `admin` (decision 3).
const BUILT_IN_PERMISSION_PROFILES: &[(&str, &[ToolGroup])] = &[
    ("admin", &[ToolGroup::Fs, ToolGroup::Shell, ToolGroup::Mcp, ToolGroup::Admin]),
    ("default", &[ToolGroup::Fs, ToolGroup::Mcp]),
    ("chat", &[]),
];
```

and the resolution methods (they keep the LLM-profile resolution's shape — operator section wins, built-in is the fallback):

```rust
impl Config {
    /// The permission profile `name` names: the operator's
    /// `[permission_profile.<name>]` when the host wrote one, else the built-in
    /// preset. `name` is always one of `admin`/`default`/`chat` or an operator key —
    /// an unknown name is a startup error (Task 2's validation), never a fallback.
    pub fn permission_profile(&self, name: &str) -> PermissionProfileConfig {
        self.permission_profiles
            .get(name)
            .cloned()
            .unwrap_or_else(|| {
                let tools = BUILT_IN_PERMISSION_PROFILES
                    .iter()
                    .find(|(preset, _)| *preset == name)
                    .map(|(_, groups)| groups.to_vec())
                    .unwrap_or_default();
                PermissionProfileConfig { tools }
            })
    }

    /// R3: whether `name`'s effective permission profile lists `group`.
    pub fn permission_lists(&self, name: &str, group: ToolGroup) -> bool {
        self.permission_profile(name).lists(group)
    }

    /// R2: whether this host hands the agent that key (the per-group
    /// `[tools.<group>]` block, host-only — see `config_layer::HOST_ONLY_PATHS`).
    pub fn group_enabled(&self, group: ToolGroup) -> bool {
        match group {
            ToolGroup::Fs => self.tools.fs.enabled,
            ToolGroup::Shell => self.tools.shell.enabled,
            ToolGroup::Mcp => self.tools.mcp.enabled,
            ToolGroup::Admin => self.tools.admin.enabled,
        }
    }

    /// R7: registered at all ⇔ enabled here ∧ listed by at least one effective
    /// permission profile (presets and operator sections alike).
    pub fn group_registered(&self, group: ToolGroup) -> bool {
        self.group_enabled(group)
            && self.effective_permission_profiles()
                .iter()
                .any(|p| p.lists(group))
    }
}

impl PermissionProfileConfig {
    pub fn lists(&self, group: ToolGroup) -> bool {
        self.tools.contains(&group)
    }
}
```

`effective_permission_profiles()` returns every profile that decides anything here — the presets the session profiles fall through to plus every `[permission_profile.<n>]` section — and lands in Task 2 with the rename; until then it returns the presets' three entries built from `BUILT_IN_PERMISSION_PROFILES` plus the operator sections. `session_profile_for(declared)` is `declored.unwrap_or(DEFAULT_PROFILE_NAME)` (`DEFAULT_PROFILE_NAME` is `config.rs:783`), and `session_profile(name)` is `self.session_profiles.get(name)` — both are the existing `room_profile()`/`room_profile_name_for_room()` (`:1549`, `:1842`) under their new names, which Task 2 renames. The four `Config::tools` group blocks are `ToolGroupConfig { enabled: bool }` (same shape as today's `HostAccess`, `:968`) and `AdminToolsConfig { enabled, .. }`.

- [ ] **Step 4: Run to verify they pass** — `cargo test -p sapphire-agent-server config:: 2>&1 | tail -15`. Expected: all green, including the pre-existing `room_profile` tests untouched.

- [ ] **Step 5: Format and commit**

```bash
cargo fmt --all
git add crates/sapphire-agent-server/src/config.rs
git commit -m "$(cat <<'EOF'
feat(config): add ToolGroup and the session/permission profile types

Co-authored-by: Saphina <spph12@sapphire-turtle.org>
EOF
)"
```

---

## Task 2: The rename, the declaration-side binding, and the four migration cases

**Files:**
- Modify: `crates/sapphire-agent-server/src/config.rs` (`room_profiles`→`session_profiles`, `RoomProfileConfig`→`SessionProfileConfig` minus `rooms`/`api_keys`, `MatrixConfig`/`DiscordConfig` room blocks, `ToolsConfig` group blocks, `migration_errors`, `validate_profiles`)
- Modify: `crates/sapphire-agent-server/src/config_layer.rs` (`HOST_ONLY_PATHS`)
- Modify (mechanical rename only, behaviour unchanged): `tools/config_tools.rs:142,156,172` (`current_admin_room_profile`→`current_session_profile`/`current_permission_profile`, `ROOM_REFUSAL`), `serve/mod.rs:3239` + `serve/mod.rs:3301` (the pin `session_room_profiles`→`session_profiles`, `admin_room_profile`→`session_profile`), `serve/acp.rs` (the `namespace_for_room_profile` call sites), `provider/registry.rs:163`, `heartbeat.rs:95`, `agent.rs:279,338,462,776`, `lib.rs`, `config.example.toml`, `templates/workspace/config.toml`, `README.md`
- Test: `config.rs`'s `mod tests`, plus the call-site tests named below

**Interfaces:**
- Consumes: Task 1's `SessionProfileConfig`/`PermissionProfileConfig`/`Config::permission_profile`.
- Produces: `Config::session_profile(name) -> Option<&SessionProfileConfig>`; `Config::session_profile_name(&self, declared: Option<&str>, room_id: &str) -> String` (a room's own block wins, else the declared name, else the implicit `"default"`); `Config::profile_for_session(&self, name) -> Option<&str>` (the LLM profile a session profile names), `Config::namespace_for_session_profile(&self, name) -> &str`, `Config::session_policy_for_session(&self, name) -> SessionPolicy`, `Config::validate_session_profiles(&self) -> Vec<String>` and `Config::session_profile_defined(name) -> bool` (R1: an unknown name — explicit or the implicit `default` when the host defines none — fails startup).

- [ ] **Step 1: Write the failing tests**

```rust
    /// R1: a room declares its own profile. The subscription itself is untouched.
    #[test]
    fn a_room_block_is_what_names_the_session_profile() {
        let cfg = Config::parse_for_test(
            "[anthropic]\napi_key = \"test\"\n\n[discord]\nbot_token = \"t\"\n\n[discord.room1]\nroom_id = \"1474368482296807444\"\nprofile = \"ops\"\n",
        );
        let discord = cfg.discord.as_ref().expect("configured");
        assert_eq!(discord.rooms[0].room_id, "1474368482296807444");
        assert_eq!(discord.rooms[0].profile, "ops");
        assert_eq!(cfg.session_profile_name(None, "1474368482296807444"), "ops");
        // A room no channel here declares: the implicit "default" (decision 2).
        assert_eq!(cfg.session_profile_name(None, "!unlisted:srv"), DEFAULT_PROFILE_NAME);
    }

    /// R1: what ingress resolves and the turn carries is the resolved name only.
    #[test]
    fn a_declared_profile_wins_and_an_unknown_one_fails_startup() {
        let cfg = Config::parse_for_test(
            "[anthropic]\napi_key = \"test\"\n\n[session_profile.ops]\nprofile = \"dev\"\nmemory_namespace = \"user_ops\"\n",
        );
        assert_eq!(cfg.session_profile_name(Some("ops"), "!any:srv"), "ops");
        assert_eq!(cfg.namespace_for_session_profile("ops"), "user_ops");
        let errors = cfg.validate_session_profiles();
        assert!(errors.iter().any(|e| e.contains("references unknown profile 'dev'")), "{errors:?}");
    }

    /// Decision 8 cases 1, 2, 3 and 4: named, and startup stops.
    #[test]
    fn migration_leftovers_are_named_and_stop_startup() {
        // Case 1 + 2: a leftover `[room_profile.<n>]`, still carrying `rooms`.
        let cfg = Config::parse_for_test(
            "[anthropic]\napi_key = \"test\"\n\n[room_profile.ops]\nprofile = \"dev\"\nrooms = [\"!ops:x\"]\n",
        );
        let errors = cfg.migration_errors();
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].contains("[room_profile.ops]") && errors[0].contains("session_profile"));
        assert!(errors[0].contains("rooms"));

        // Case 3: a leftover `[tools.admin].room_profiles`.
        let cfg = Config::parse_for_test(
            "[anthropic]\napi_key = \"test\"\n\n[tools.admin]\nroom_profiles = [\"ops\"]\n",
        );
        assert!(cfg
            .migration_errors()
            .iter()
            .any(|e| e.contains("[tools.admin].room_profiles")
                && e.contains("permission_profile.<name>.tools")));

        // Case 4: a session resolving to "default" while the host defines none.
        let cfg = Config::parse_for_test("[anthropic]\napi_key = \"test\"\n\n[discord]\nbot_token = \"t\"\n\n[discord.room1]\nroom_id = \"!a:srv\"\n");
        assert!(cfg
            .migration_errors()
            .iter()
            .any(|e| e.contains("no [session_profile.default]")));
    }

    /// The per-group enabled keys are host-only; `tools.mcp_servers` and the
    /// listings stay synced. `starts_with` compares whole segments, so
    /// `["tools","mcp"]` does NOT cover `tools.mcp_servers`.
    #[test]
    fn per_group_enabled_stays_host_only_and_the_listings_stay_synced() {
        for group in ["fs", "shell", "mcp", "admin"] {
            assert!(!config_layer::path_allowed(&["tools", group]));
        }
        assert!(config_layer::path_allowed(&["tools", "mcp_servers"]));
        assert!(config_layer::path_allowed(&["permission_profile"]));
        assert!(config_layer::path_allowed(&["session_profile"]));
    }
```

- [ ] **Step 2: Run to verify it fails** — `cargo test -p sapphire-agent-server config:: 2>&1 | tail -20`. Expected: FAIL (`no field \`rooms\` on \`DiscordConfig\``, `no method named \`session_profile_name\``).

- [ ] **Step 3: Implement the shapes.** `Config::room_profiles` (`:85`) becomes, and the old struct loses `rooms` and `api_keys` (both are now named by `migration_errors`):

```rust
    /// Session profiles: what kind of session this is — the LLM profile, the memory
    /// namespace, the session policy, the permission profile. A session declares its
    /// own profile (decision 1); there is no profile-side room list any more.
    #[serde(default, rename = "session_profile")]
    pub session_profiles: HashMap<String, SessionProfileConfig>,
    /// Removed in this migration; retained only so a config that still sets it fails
    /// loudly rather than losing its grant silently. Never read otherwise.
    #[serde(default, rename = "room_profile")]
    pub legacy_room_profiles: HashMap<String, LegacyRoomProfile>,
    /// `[permission_profile.<n>]`: which groups a session may use (decision 3).
    #[serde(default, rename = "permission_profile")]
    pub permission_profiles: HashMap<String, PermissionProfileConfig>,
```

with the capture-only old shape (`LegacyRoomProfile { profile, rooms, api_keys }`), and the channel configs (`:1112`, `:1168`):

```rust
/// One room the bot listens to, and the session profile it runs under (decision 2).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RoomConfig {
    /// Matrix room id / Discord channel id.
    pub room_id: String,
    /// The `[session_profile.<n>]` this room's turns run under.
    pub profile: String,
    /// Voice pipeline preset for this room; absent means voice is off here.
    #[serde(default)]
    pub voice_pipeline: Option<String>,
}
```

`MatrixConfig.room_ids`/`DiscordConfig.channel_ids` are replaced by `rooms: Vec<RoomConfig>` (`MatrixConfig.primary_room_id()` becomes `self.rooms.first().map(|r| &r.room_id)`; `allowed_users` stays channel-level — room-level override is out of scope). `ToolsConfig` keeps `tavily_api_key`/`mcp_servers`/`tool_rounds`/`subagent`/`background` and replaces `host_access` with `fs: ToolGroupConfig`, `shell: ToolGroupConfig`, `mcp: ToolGroupConfig`, and `admin: AdminToolsConfig { enabled: bool, room_profiles: Vec<String> /* capture-only */ }`; `HOST_ONLY_PATHS` (`config_layer.rs:51`) gains `&["tools","fs"]`, `&["tools","shell"]`, `&["tools","mcp"]`, `&["tools","admin"]` and loses `&["tools","host_access"]`.

The resolution methods keep today's precedence, now reading the declaration side (`:1537` `room_profile_for`, `:1567` `profile_for`, `:1949` `namespace_for_room_profile`, `:1842` `room_profile_name_for_room`, `:1958` `namespace_for_room`):

```rust
    /// The session profile a turn runs under: the session's own declared name when it
    /// has one (a heartbeat/autonomous frontmatter, an ACP pin, a device binding),
    /// else the `[<channel>.room<n>]` block's, else the implicit `"default"` — the
    /// same resolution the LLM profile and the memory namespace use, so a room's grant
    /// cannot disagree with the provider it runs on.
    pub fn session_profile_name(&self, declared: Option<&str>, room_id: &str) -> String {
        declared
            .map(|n| (n, self.session_profile(n).is_some()))
            .filter(|(_, defined)| *defined)
            .map(|(n, _)| n.to_string())
            .or_else(|| {
                self.channels()
                    .iter()
                    .find_map(|(_, room)| (room.room_id == room_id).then(|| room.profile.clone()))
                    .map(|n| n)
                    .or_else(|| self.session_profiles.get(DEFAULT_PROFILE_NAME).map(|_| DEFAULT_PROFILE_NAME.to_string()))
            })
            .unwrap_or_else(|| DEFAULT_PROFILE_NAME.to_string())
    }
```

`profile_for(room_id)`/`namespace_for_room(room_id)` callers (`agent.rs:279,338,462,776`, `heartbeat.rs:95`, `serve/mod.rs:303,1148,1732`, `provider/registry.rs:163`, `serve/acp.rs:1294,1501,1572`) become `profile_for_session(name)`/`namespace_for_session_profile(name)`; `validate_profiles` (`:1589`) validates session profiles (unknown LLM profile, unknown memory namespace, unknown `voice_pipeline`, and the two `permission`-side shapes of Task 1) and `validate_subagent_profiles` (`:1740`) is untouched; `[acp].stdio_room_profile` (`:304`) becomes `stdio_session_profile` and `[cli].room_profile` (`:336`) becomes `profile`; `config_tools_enabled()`/`admin_allows_room_profile()` (`:1827`, `:1855`) die (the `admin`-group gate is `allow()`'s job, Tasks 3-4). `config.example.toml` and `templates/workspace/config.toml` are rewritten in English to `[session_profile.<n>]` + `[permission_profile.<n>]` + `[tools.<group>]`, and `README.md`'s admin-tools section names `permission_profile.<n>.tools`.

- [ ] **Step 4: Run everything** — `cargo test -p sapphire-agent-server 2>&1 | tail -15`; then `cargo clippy -p sapphire-agent-server --all-targets 2>&1 | tail -5`. Expected: green, zero warnings; the gate tests renamed in this step pin the same behaviour through the new names.

- [ ] **Step 5: Format and commit**

```bash
cargo fmt --all
git add -A
git commit -m "$(cat <<'EOF'
refactor(config)!: bind session profiles on the declaration side

Co-authored-by: Saphina <spph12@sapphire-turtle.org>
EOF
)"
```

---

## Task 3: `policy.rs` — the origins carry the resolved name, and one function decides

**Files:**
- Modify: `crates/sapphire-agent-server/src/tools/policy.rs` (whole file)
- Modify (mechanical, call sites only — end-to-end tests land in Task 6): `agent.rs:821-827`, `serve/mod.rs:2674,2692,2852-2920,2382-2410`
- Test: `policy.rs`'s `mod tests`, `agent.rs`'s gate tests (`:1028-1099`) renamed, `serve/mod.rs`'s existing turn tests

**Interfaces:**
- Consumes: Task 1's `ToolGroup`; Task 2's `Config::session_profile_name`/`permission_profile`/`group_registered`.
- Produces: `Origin::{Acp { mode: SessionMode, session_profile: String }, Channel { session_profile: String }, Unattended { session_profile: String }}`; `Allow{Allow,Ask,Deny}` (the `Decision`-equivalent the spec's Design names, replacing `Decision`); `ResolvedPermission { name, groups, mode }` with `ResolvedPermission::resolve(&Config, Origin) -> Self` and `lists(&self, ToolGroup) -> bool`; `ToolEntry { name: String, group: Option<ToolGroup>, enabled: bool, kind: ToolKind }` with `ToolEntry::for_tool(name, sp, cfg, kind) -> Self`; `pub fn allow(profile: &ResolvedPermission, entry: &ToolEntry) -> Allow`; `partition_without_asking(&ResolvedPermission, &[ToolCall], &[ToolEntry]) -> (Vec<ToolCall>, Vec<(String,String)>)`. `Origin::Trusted` is replaced by `Unattended` (R1: `/rpc`, voice and A2A sessions are governed by the profile their session belongs to, so they carry a name too).

- [ ] **Step 1: Write the failing tests**

```rust
    /// R2: the table is a group × permission-profile table, and the mode is what
    /// decides whether an offered one asks (decision 7).
    #[test]
    fn allow_is_the_group_table_and_nothing_else() {
        let entry = |group, enabled, kind| ToolEntry {
            name: "x".into(),
            group: Some(group),
            enabled,
            kind,
        };
        let default_under_fs = ResolvedPermission {
            name: "default".into(),
            groups: vec![ToolGroup::Fs],
            mode: None,
        };
        // A chat turn under `default`: the fs tools run unasked, and shell is refused
        // because no profile named it (decision 3).
        assert_eq!(allow(&default_under_fs, &entry(ToolGroup::Fs, true, ToolKind::Edit)), Allow::Allow);
        let chat = ResolvedPermission { name: "chat".into(), groups: vec![], mode: None };
        assert_eq!(allow(&chat, &entry(ToolGroup::Fs, true, ToolKind::Read)), Allow::Deny);
        // R9: the mode decides the asking, from `kind`, exactly as `decide` did.
        let acp_default = ResolvedPermission {
            name: "default".into(),
            groups: vec![ToolGroup::Fs],
            mode: Some(SessionMode::Default),
        };
        assert_eq!(allow(&acp_default, &entry(ToolGroup::Fs, true, ToolKind::Read)), Allow::Allow);
        assert_eq!(allow(&acp_default, &entry(ToolGroup::Fs, true, ToolKind::Edit)), Allow::Ask);
        assert_eq!(
            allow(
                &ResolvedPermission { name: "admin".into(), groups: vec![ToolGroup::Shell], mode: Some(SessionMode::AcceptEdits) },
                &entry(ToolGroup::Shell, true, ToolKind::Execute),
            ),
            Allow::Ask
        );
        // R7: a group this host does not hand the agent is refused whatever the profile lists.
        assert_eq!(allow(&default_under_fs, &entry(ToolGroup::Fs, false, ToolKind::Edit)), Allow::Deny);
    }

    /// R4: the always-allowed tools are outside the table's subject — they carry no group.
    #[test]
    fn always_allowed_tools_are_not_the_table_subject() {
        let chat = ResolvedPermission { name: "chat".into(), groups: vec![], mode: None };
        for kind in [ToolKind::Read, ToolKind::Fetch, ToolKind::Search] {
            let entry = ToolEntry { name: "current_time".into(), group: None, enabled: false, kind };
            assert_eq!(allow(&chat, &entry), Allow::Allow, "{kind:?}");
        }
    }

    /// A channel/unattended turn never asks: `partition_without_asking` sees only Allow/Deny.
    #[test]
    fn a_channel_turn_partitions_on_allow_alone() {
        let cfg = Config::parse_for_test(
            "[anthropic]\napi_key = \"test\"\n\n[tools.fs]\nenabled = true\n[tools.shell]\nenabled = true\n",
        );
        let profile = ResolvedPermission::resolve(&cfg, Origin::Channel { session_profile: cfg.session_profile_name(None, "!a:srv") });
        let entries = [
            ToolEntry::for_tool("file_write", "default", &cfg, ToolKind::Edit),
            ToolEntry::for_tool("shell", "default", &cfg, ToolKind::Execute),
            ToolEntry::for_tool("memory_add", "default", &cfg, ToolKind::Edit),
        ];
        let calls = vec![
            call("c0", "file_write"),
            call("c1", "shell"),
            call("c2", "memory_add"),
        ];
        let (permitted, refused) = partition_without_asking(&profile, &calls, &entries);
        let kept: Vec<&str> = permitted.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(kept, vec!["file_write", "memory_add"]);
        let name_of = |id: &str| calls.iter().find(|c| c.id == id).map(|c| c.name.clone()).unwrap();
        let blocked: Vec<String> = refused.iter().map(|(id, _)| name_of(id)).collect();
        assert_eq!(blocked, vec!["shell"]);
        for (_, reason) in &refused {
            assert!(reason.contains("permission_profile"), "{reason} names the setting to change");
        }
    }
```

- [ ] **Step 2: Run to verify it fails** — `cargo test -p sapphire-agent-server policy:: 2>&1 | tail -20`. Expected: compile error (`unknown type \`Allow\``).

- [ ] **Step 3: Write the policy.** `Origin` becomes a type that carries the resolved profile name; `Decision`→`Allow`; `decide`, `host_tool_denied`, `HOST_TOOLS`, `kind_of` and the `Trusted` variant die (the `enabled` flag inside `ToolEntry` is what absorbed `host_tool_denied` — "a fact about the deployment, not a row in the table" is kept):

```rust
/// The single decision function. Both turn loops build their `Origin`'s resolved
/// profile and pass it here with the tool's registration entry.
///
/// Allowed ⇔ `profile` lists `group` and `group` is enabled on this host. A
/// tool outside the table's subject (`group: None`) is always allowed (R4); the
/// mode decides whether an *offered* one asks, from `kind`, exactly as the old
/// `decide` row did (decision 7).
pub fn allow(profile: &ResolvedPermission, entry: &ToolEntry) -> Allow {
    let Some(group) = entry.group else {
        return Allow::Allow;
    };
    if !entry.enabled || !profile.lists(group) {
        return Allow::Deny;
    }
    // `Edit`/`Delete`/`Move`/`SwitchMode` are the middle row, `Execute`/`Other` the
    // strict one, `Read`/`Search`/`Fetch`/`Think` the safe one — grouped so a
    // `ToolKind` variant added upstream lands in the strict bucket rather than
    // silently becoming safe.
    let safe = matches!(entry.kind, ToolKind::Read | ToolKind::Search | ToolKind::Fetch | ToolKind::Think);
    match profile.mode {
        None => Allow::Allow,
        Some(SessionMode::Bypass) => Allow::Allow,
        Some(_) if safe => Allow::Allow,
        Some(SessionMode::AcceptEdits) => Allow::Ask,
        Some(SessionMode::Default) => Allow::Ask,
    }
}

impl ResolvedPermission {
    /// One call per turn, at ingress: the origin's session-profile name resolves to the
    /// permission profile that profile's `permission` names (absent means `"default"`),
    /// and the ACP asking shape travels with the mode.
    pub fn resolve(config: &Config, origin: Origin) -> Self {
        match origin {
            Origin::Acp { mode, session_profile } => Self {
                groups: config.permission_profile(&session_profile).tools,
                name: session_profile,
                mode: Some(mode),
            },
            Origin::Channel { session_profile } | Origin::Unattended { session_profile } => Self {
                groups: config.permission_profile(&session_profile).tools,
                name: session_profile,
                mode: None,
            },
        }
    }
}

impl ToolEntry {
    /// What registration knew: this tool's group and this host's flag for it.
    pub fn for_tool(name: &str, session_profile: &str, config: &Config, kind: ToolKind) -> Self {
        Self {
            group: group_of(name),
            enabled: group_of(name).is_some_and(|g| config.group_enabled(g)),
            name: name.to_string(),
            kind,
        }
    }
}
```

`group_of(name)` is the one name→group table (the four groups' members from this plan's File Structure; `None` for everything else), and `refusal_message` gains a `Refusal::Unavailable` wording that names `permission_profile.<name>.tools` / `[tools.<group>].enabled`. `Origin::Channel` never asks (a channel turn is asynchronous, so blocking it on a human reply could hang for hours); `/rpc`, voice, A2A and the unattended loops arrive as `Unattended { session_profile }` and behave as before. The two call sites become, at this stage, the mechanical shape (`agent.rs:821`: `let profile = ResolvedPermission::resolve(&self.config, Origin::Channel { session_profile: self.config.session_profile_name(None, &incoming.room_id) }); partition_without_asking(&profile, &offered, &entries)`; `serve/mod.rs:2852-2920`: the same `allow` for `Allow::Allow`/`Ask`→`progress.approve`/`Deny`), with `visible_tool_predicate`'s `host_tool_denied` clause reading `group_registered`.

- [ ] **Step 4: Run everything** — `cargo test -p sapphire-agent-server 2>&1 | tail -15` (the renamed `agent.rs` gate tests keep the old assertions' meaning: `file_write` allowed under `default`, `shell` refused, always-allowed tools unaffected). Expected: green.

- [ ] **Step 5: Format and commit**

```bash
cargo fmt --all
git add -A
git commit -m "$(cat <<'EOF'
refactor(policy)!: decide the group table, one function, on both loops

Co-authored-by: Saphina <spph12@sapphire-turtle.org>
EOF
)"
```

---

## Task 4: `ToolSpec` carries `kind` and `group`; registration is per group

**Files:**
- Modify: `crates/sapphire-agent-server/src/provider/mod.rs` (`ToolSpec` gains the two fields, `:19-23`)
- Modify: `crates/sapphire-agent-server/src/tools/mod.rs` (`Tool::kind()` stays, `ToolSet::kinds()`→`entries()`, `default_tool_set` (`:318-388`) registers by group)
- Modify: every prod `ToolSpec` literal in `tools/builtin_tools.rs`, `tools/workspace_tools.rs`, `tools/session_tools.rs`, `tools/ambient_tools.rs`, `tools/skill_tools.rs`, `tools/attachment_tools.rs`, `tools/background_tools.rs`, `tools/config_tools.rs`, `tools/client_tools.rs`, `tools/subagent.rs`, `mcp_client/mod.rs:740`
- Test: `tools/mod.rs`'s `mod tests` (the `every_tool_declares_its_kind` table, `:481-505`)

**Interfaces:**
- Consumes: `ToolGroup` (Task 1), `ToolEntry`/`ResolvedPermission` (Task 3).
- Produces: `ToolSpec { name, description, input_schema, kind: ToolKind, group: Option<ToolGroup> }`; `ToolSet::entries(&self) -> Vec<(String, ToolEntry)>`; `config_tools::definition_owner(&Path) -> Option<&'static str>` (the write guard, R6); `register_admin_tools`' condition `config.group_registered(ToolGroup::Admin)` (the `[tools.admin]` philosophy is kept — *not registered at all*).

- [ ] **Step 1: Write the failing tests**

```rust
    /// Decision 4/7: every tool is registered with the group that decides whether it
    /// is offered at all, and the kind that drives the mode's asking. A tool added
    /// without a `group` lands outside the table — so does one added without a
    /// `kind`, in `Other`, the strictest bucket.
    #[tokio::test]
    async fn every_tool_declares_its_group_and_kind() {
        let tools = default_tool_set(test_workspace(), Some("test-tavily-key".to_string()), &[], 4).await;
        let mut entries = tools.entries().await;
        entries.sort();
        let want = [
            ("file_append", ToolKind::Edit, ToolGroup::Fs),
            ("file_delete", ToolKind::Delete, ToolGroup::Fs),
            ("file_read", ToolKind::Read, ToolGroup::Fs),
            ("file_write", ToolKind::Edit, ToolGroup::Fs),
            ("dir_list", ToolKind::Search, ToolGroup::Fs),
            ("dir_walk", ToolKind::Search, ToolGroup::Fs),
            ("image_read", ToolKind::Read, ToolGroup::Fs),
            ("shell", ToolKind::Execute, ToolGroup::Shell),
            ("mcp_reconnect", ToolKind::Other, ToolGroup::Mcp),
            ("current_time", ToolKind::Read, None),
            ("memory_add", ToolKind::Edit, None),
            ("workspace_search", ToolKind::Search, None),
            ("skill", ToolKind::Read, None),
            ("subagent", ToolKind::Other, None),
        ];
        for (name, kind, group) in want {
            let (_, entry) = entries.iter().find(|(n, _)| n == name).expect(name);
            assert_eq!(entry.kind, kind, "{name}");
            assert_eq!(entry.group, group, "{name}");
        }
    }

    /// R6: the definition files are the `admin` group's land — modifiable only by the
    /// dedicated tool that owns them; the read half keeps reading them.
    #[tokio::test]
    async fn a_fs_write_tool_refuses_a_definition_file_naming_its_owner() {
        let (_, root, ws) = config_tools::test_workspace();
        let write = FileWriteTool::new(Arc::clone(&ws));
        let err = write.execute(&json!({"path": "<root>/heartbeat/morning_call.md"})).await.unwrap_err();
        assert!(err.to_string().contains("heartbeat_config"), "{err}");
        assert_eq!(config_tools::definition_owner(Path::new("heartbeat/morning_call.md")), Some("heartbeat_config"));
        assert_eq!(config_tools::definition_owner(Path::new("autonomous/journal.md")), Some("autonomous_config"));
        assert_eq!(config_tools::definition_owner(Path::new("agents/reviewer.md")), Some("agent_config"));
        assert_eq!(config_tools::definition_owner(Path::new("MEMORY.md")), None);
    }
```

- [ ] **Step 2: Run to verify it fails** — `cargo test -p sapphire-agent-server tools:: 2>&1 | tail -20`. Expected: compile error (`no field \`group\` on \`ToolSpec\``).

- [ ] **Step 3: Implement.** `provider::ToolSpec` gains `pub kind: ToolKind` and `pub group: Option<ToolGroup>` (every literal in the inventory gains both — the ones named in the table above plus `weather`/`web_search` → `(Fetch, None)`, `recall_image`/`attachment_save` → per R4 outside the table, `session_list`/`session_read`/`refresh_system_prompt`/`ask_human`/`background_task`/the 3 ambient and 3 avatar tools → `(kind, None)`, `skill_install`/`skill_update`/`skill_uninstall`/`transcript_read`/`speaker_*` → `(kind, None)`, `heartbeat_config`/`autonomous_config`/`agent_config`/`task_test` → `(Edit, Admin)`); `McpTool`'s and `McpReconnectTool`'s come from `kind_for_trust(trust)` (`mcp_client/mod.rs:778`) and `kind_for_annotations` at registration, group `Mcp` (decision 5 — `mcp_reconnect` stays `Other` and is therefore asked under an ACP mode that asks); `Tool::kind()`'s default stays `Other`, `Tool::group()` is not a method — the group is declared on the spec literal, because "the group decides whether a tool is offered at all" is registration knowledge, not behaviour. `default_tool_set` builds its `Vec<Box<dyn Tool>>` from the groups it was handed (`enabled ∧ registered-somewhere`, same shape as today's "registered when the tavily key exists") and gains `permission_profiles: &Config` so `group_registered` answers it; the guard:

```rust
/// Which dedicated tool owns `rel`'s definition file, `None` for a file that is not
/// a definition. The fs tools' *write* half refuses what this names (decision 6):
/// a heartbeat session cannot rewrite its own definitions through `fs`, and an
/// unattended task that writes no `profile` has neither `admin` nor `shell` to go
/// around the dedicated tools with. The read half keeps reading them — reading one's
/// own task is not a rewrite risk.
pub(crate) fn definition_owner(rel: &Path) -> Option<&'static str> {
    let dir = rel.iter().next()?.to_str()?.to_string();
    ConfigDir::ALL
        .iter()
        .find(|d| d.dir_name() == dir.as_str() && rel.extension().is_some_and(|e| e == "md"))
        .map(|d| d.tool_name())
}
```

called from `FileWriteTool::execute`/`FileAppendTool::execute`/`FileDeleteTool::execute` (and `ShellTool` needs nothing: under `default` it is not listed, so it is not offered).

- [ ] **Step 4: Run everything** — `cargo test -p sapphire-agent-server 2>&1 | tail -15`; `cargo clippy -p sapphire-agent-server --all-targets 2>&1 | tail -5`. Expected: green, zero warnings.

- [ ] **Step 5: Format and commit**

```bash
cargo fmt --all
git add -A
git commit -m "$(cat <<'EOF'
feat(tools)!: carry kind and group at registration, guard the definitions

Co-authored-by: Saphina <spph12@sapphire-turtle.org>
EOF
)"
```

---

## Task 5: Startup — group registration, and the unattended loaders carry `profile`

**Files:**
- Modify: `crates/sapphire-agent-server/src/heartbeat_config.rs:32-50` (`HeartbeatTaskMeta` gains `#[serde(default)] pub profile: Option<String>`; `room_id` keeps parsing, now meaning delivery-only)
- Modify: `crates/sapphire-agent-server/src/autonomous_config.rs:21-38` (`AutonomousTaskMeta` gains `profile`; `AutonomousTask` gains `pub profile: Option<String>`)
- Modify: `crates/sapphire-agent-server/src/config.rs` (`validate_task_profiles`, same shape as `validate_subagent_profiles`)
- Modify: `crates/sapphire-agent-server/src/lib.rs:1129` (the `register_admin_tools` call), `lib.rs`'s heartbeat/autonomous construction (`:1589` neighbours, `heartbeat.rs:378,425`, `autonomous.rs:155,176`)
- Test: `config.rs` + `lib.rs`'s `mod tests`

**Interfaces:**
- Consumes: `Config::group_registered`, `Config::session_profile_defined` (Tasks 1-2), `Origin::Unattended` (Task 3), `default_tool_set`'s new signature (Task 4).
- Produces: `Config::validate_task_profiles(&self, &[HeartbeatTask], &[AutonomousTask]) -> Vec<String>`; `HeartbeatTask.meta.profile` and `AutonomousTask.profile` carried onto the `Origin` when the turn is built; `register_admin_tools(&tool_set, .., config.clone(), ..)` gated on `config.group_registered(ToolGroup::Admin)`.

- [ ] **Step 1: Write the failing tests**

```rust
    /// R1: a heartbeat/autonomous task names the profile that governs its run. An
    /// unknown one is a startup error, same shape as a subagent definition's.
    #[test]
    fn a_task_frontmatter_names_its_session_profile() {
        let raw = "---\nschedule: \"0 8 * * *\"\nroom_id: \"!a:srv\"\nprofile: \"ops\"\nenabled: true\n---\n\n# Morning Call\n";
        let (_, root, ws) = config_tools::test_workspace();
        std::fs::write(&root.join("heartbeat/morning_call.md"), raw).unwrap();
        let tasks = crate::heartbeat_config::load_heartbeat_dir(&root.join("heartbeat"));
        assert_eq!(tasks[0].meta.profile.as_deref(), Some("ops"));
        assert_eq!(tasks[0].meta.room_id.as_deref(), Some("!a:srv"), "room_id keeps parsing: delivery-only");
        let cfg = Config::parse_for_test("[anthropic]\napi_key = \"test\"\n");
        let errors = cfg.validate_task_profiles(&tasks, &[]);
        assert!(errors.iter().any(|e| e.contains("'ops'") && e.contains("session_profile")), "{errors:?}");
    }
```

- [ ] **Step 2: Run to verify it fails** — `cargo test -p sapphire-agent-server heartbeat 2>&1 | tail -20`. Expected: FAIL (`no field \`profile\``).

- [ ] **Step 3: Implement.** `validate_task_profiles` reports every task profile that names no defined session profile (the two loaders' `deny_unknown_fields`/`warn!`-skip shape unchanged); `heartbeat.rs`'s `DueTask` gains `profile: Option<String>` and `fire_task` hands the task's name plus the resolved profile to the turn it builds (`Heartbeat`'s chat leg goes through `Agent::trigger(name, prompt, room_id, profile)`, and the ACP/autonomous leg through `run_llm_turn`'s pin clone); `lib.rs`'s startup keeps `validate_profiles`' renamed twin and adds `validate_session_profiles` for the frontmatter/room-block names, and every `default_tool_set`/`register_tool` call site passes the config so `group_registered` answers it — the session tools (`lib.rs:1140-1160`) and `register_admin_tools` (`:1129`) included, so an `admin` group named by nobody registers none of the four.

- [ ] **Step 4: Run everything** — `cargo test -p sapphire-agent-server 2>&1 | tail -15`. Expected: green.

- [ ] **Step 5: Format and commit**

```bash
cargo fmt --all
git add -A
git commit -m "$(cat <<'EOF'
feat: let an unattended task declare its session profile

Co-authored-by: Saphina <spph12@sapphire-turtle.org>
EOF
)"
```

---

## Task 6: Both loops through the one function, end to end

**Files:**
- Modify: `crates/sapphire-agent-server/src/agent.rs:776` (`IncomingMessage`'s room + any declared profile → `Origin::Channel { session_profile }`, `partition_without_asking(&profile, ..)` — `ns` from `namespace_for_session_profile`)
- Modify: `crates/sapphire-agent-server/src/serve/mod.rs` (`run_llm_turn`'s pin clone → `Origin::Acp { mode, session_profile }` / `Origin::Unattended { session_profile }`; `TurnContext`/`TurnLoop` field `session_profile`; `TurnLoop::run`'s gate calls `allow`; `visible_tool_predicate` (`:2382`) keeps the capability flags and `has_client`, and drops a shared name only when its group is unregistered or unroutable here)
- Modify: `tools/subagent.rs` + `tools/skill_tools.rs`' `TurnContext` literals
- Test: `agent.rs`'s gate tests, `serve/mod.rs`'s + `serve/acp.rs`'s turn tests, through execution with per-instance probe tools

**Interfaces:**
- Consumes: `allow`/`ResolvedPermission`/`ToolEntry` (Task 3), `ToolSpec{kind,group}`/`entries()` (Task 4), `validate_task_profiles` (Task 5).
- Produces: nothing (the last task). The end-to-end shape is what it pins.

- [ ] **Step 1: Write the failing end-to-end tests** — one per behaviour, each through the real loop and a per-instance probe with a handle (N4), not by reading fields:

```rust
    /// The two-loops-merged shape, through `handle_message`: the same function
    /// `run_llm_turn` goes through produces the same behaviour (Task 3 pinned the
    /// function; this pins both call sites on it).
    #[tokio::test]
    async fn a_channel_turn_under_default_runs_file_write_unasked() {
        let (calls, entries) = channel_gate_test_calls().await; // file_write / shell / memory_add / the MCP tool
        let cfg = cfg_with(&[("fs", true), ("shell", true), ("mcp", true)]);
        let profile = ResolvedPermission::resolve(&cfg, Origin::Channel { session_profile: "default".into() });
        let (permitted, refused) = partition_without_asking(&profile, &calls, &entries);
        assert_eq!(
            names(&permitted),
            vec!["file_write", "memory_add"],
            "fs is listed by `default` and enabled here; `shell` is not listed, and the mcp tool is refused by the profile that names no mcp"
        );
        let blocked: Vec<String> = refused.iter().map(|(id, _)| name_of(&calls, id)).collect();
        assert_eq!(blocked, vec!["shell"]);
    }

    /// Under `chat` even `fs` is invisible: the tool is not offered at all.
    #[tokio::test]
    async fn a_turn_under_chat_does_not_even_see_fs() {
        let state = ServeState::for_test_scripted(vec![ChatResponse::ok()]);
        let specs = state.tools.specs_filtered(turn_predicate("chat", &state.config)).await;
        assert!(!specs.iter().any(|s| s.name == "file_read"), "{:?}", names(&specs));
        assert!(specs.iter().any(|s| s.name == "memory_read"), "the always-allowed tools are not the table's subject");
    }

    /// `shell` refuses under `default` and runs under `admin`; a turn under `default`
    /// cannot see the admin tools even where the `admin` preset made them registered.
    #[tokio::test]
    async fn shell_refuses_under_default_and_runs_under_admin() {
        let (calls, entries) = shell_and_admin_calls().await;
        let under = |profile: &str| {
            let profile = ResolvedPermission::resolve(&cfg_with(&[("fs", true), ("shell", true)]), Origin::Channel { session_profile: profile.into() });
            partition_without_asking(&profile, &calls, &entries).0.into_iter().map(|c| c.name).collect::<Vec<_>>()
        };
        assert_eq!(under("default"), vec!["file_write"]);
        assert_eq!(under("admin"), vec!["file_write", "shell", "heartbeat_config"]);
    }

    /// The guard, end to end: `file_write` refuses `heartbeat/morning_call.md`
    /// naming `heartbeat_config`; `file_read` reads it; only `heartbeat_config` modifies it.
    #[tokio::test]
    async fn only_the_dedicated_tool_modifies_a_definition() {
        let (dir, root, ws) = config_tools::test_workspace();
        std::fs::create_dir_all(root.join("heartbeat")).unwrap();
        std::fs::write(&root.join("heartbeat/morning_call.md"), "---\nschedule: \"0 8 * * *\"\n---\n\nold\n").unwrap();
        let read = FileReadTool::new(Arc::clone(&ws));
        assert!(read.execute(&json!({"path": "<root>/heartbeat/morning_call.md"})).await.unwrap().contains("old"));
        let write = FileWriteTool::new(Arc::clone(&ws));
        assert!(write.execute(&json!({"path": "<root>/heartbeat/morning_call.md", "content": "new"})).await.is_err());
        let admin = ConfigTool::new(ConfigDir::Heartbeat, root.clone(), cfg_with(&[("fs", true)]), Arc::clone(&ws), None, Weak::new());
        assert!(admin.execute(&json!({"action": "read", "name": "morning_call"})).await.is_ok());
    }
```

- [ ] **Step 2: Run to verify they fail** — `cargo test -p sapphire-agent-server agent:: serve:: 2>&1 | tail -25`. Expected: FAIL (the loops still route a name through a table rather than through `allow`'s `ToolEntry`).

- [ ] **Step 3: Finish the wiring.** `agent.rs`: the room's resolved name is built once per message and travels as the `Origin`; `serve/mod.rs`: `let session_profile = if is_acp { state.session_profiles.lock().await.get(&session_id).cloned() } else { None };` → `Origin::Acp { mode: progress.mode(), session_profile }` / `Origin::Unattended { session_profile }`, and the turn's `entries` come from `tools.entries()` (so the profile that names no `admin` cannot see the four even though the `admin` preset registered them). `[acp].stdio_session_profile` and the `cli` block keep being the only sources for stdio ACP and the local clients — the principle of not adding sources of truth is unchanged (decision 2). `subagent` keeps inheriting the delegating turn's profile through `TurnContext`.

- [ ] **Step 4: Run the whole suite and the gates**

```bash
cargo fmt --all
cargo clippy -p sapphire-agent-server --all-targets
cargo test -p sapphire-agent-server
```

Expected: green, zero warnings. Then re-read the spec's Decisions 1-8 and R1-R9 against the diff before pushing.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "$(cat <<'EOF'
refactor!: let both turn loops decide through allow

Co-authored-by: Saphina <spph12@sapphire-turtle.org>
EOF
)"
```
