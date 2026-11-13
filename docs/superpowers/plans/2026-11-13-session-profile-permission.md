# Session profiles and permission profiles Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task.
> Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the room-profile/origin×kind permission system with `[session_profile.<n>]` (a session declares its own profile) + `[permission_profile.<n>].tools = [<groups>]` (the allow table), so a turn's allowed set is "groups listed by its permission profile ∧ groups enabled on this host", decided by exactly one function on both turn loops.

**Architecture:** `config.rs` gains `ToolGroup`/`SessionProfileConfig`/`PermissionProfileConfig` and the presets, and is renamed at the same time (`room_profiles` → `session_profiles`), so the rename is what makes Task 1 compile and pass on its own; Task 2 then moves the binding to the declaration side (`[discord.room1]`-shaped room blocks, the four `migration_errors` cases, the per-group `[tools.<group>]` blocks). `policy.rs` becomes the single decision function `allow(profile, entry)` over a `ResolvedPermission` + the tool's registration entry `ToolEntry{group, enabled, kind}`, replacing `decide`/`host_tool_denied`/`kind_of`/`HOST_TOOLS`; the entry is built from `ToolSpec`, which by then carries `kind` and `group` themselves — one source, no name→group table. The definition files stay the `admin` group's land: the fs *write* tools refuse them (on the path the writer will actually open), the read half keeps reading them.

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
- The always-allowed tools stay **outside** the table (`group: None`): `current_time`, `weather`, `web_search`, `workspace_search`, `workspace_sync`, `skill`+`skill_install`/`skill_update`/`skill_uninstall`, `subagent` (inherits the delegating turn), the memory tools, and everything else the spec does not name — the spec names exactly four groups' members, so anything outside those four lists is not the table's subject. Being outside the table is about the *group* only: `kind` still meets the mode exactly as a grouped tool's does (Task 3's `allow` has no early return). Memory namespace comes from the session profile (`namespace_for_room_profile` → `namespace_for_session_profile`).
- Migration fails loudly (decision 8): each leftover names the key and stops startup; `room_id` in a heartbeat/autonomous frontmatter keeps parsing, now meaning delivery-only.
- Routing of the unified names to the editor's machine through ACP capabilities is unchanged (capabilities and groups are orthogonal, R9): an ACP turn with a client is exactly as decided by the editor's capabilities today; `routed_to_client` survives as "this turn's calls land elsewhere", and a turn with no client is decided by the group table alone.
- The guard reads the path the writer is about to open — `expand_path`'s output (`~/...` already expanded, a relative path already resolved against the root) then `lexically_normalize`'s (a `..` detour collapsed) — the same raw-and-collapsed double comparison `refuse_if_sensitive_in` uses, and it covers every definition file: the `ConfigDir` directories' Markdown *and* the config definition files `PROTECTED_CONFIG_FILES` names.

## File Structure

| File | Responsibility in this plan |
|---|---|
| `server/src/config.rs` | `ToolGroup`, `SessionProfileConfig` (the rename of `RoomProfileConfig`), `PermissionProfileConfig`, presets + precedence, the per-group `[tools.<group>]` enabled blocks, room blocks on `[discord]`/`[matrix]`, the four `migration_errors` cases, `validate_*`. |
| `server/src/config_layer.rs` | `HOST_ONLY_PATHS` gains the per-group `[tools.<group>]` paths. |
| `server/src/provider/mod.rs` | `ToolSpec { name, description, input_schema, kind: ToolKind, group: Option<ToolGroup> }` (`:19-23`). |
| `server/src/tools/policy.rs` | `Origin` (carries the resolved profile name), `Allow`, `ResolvedPermission`, `ToolEntry`, the single `allow`, `partition_without_asking`. `decide`/`host_tool_denied`/`kind_of`/`HOST_TOOLS` die. |
| `server/src/tools/mod.rs` | `ToolSet::kinds()` → `entries()`; `default_tool_set` registers per group. |
| `server/src/tools/config_tools.rs` | `current_session_profile()`/`current_permission_profile()`, the refusal naming `permission_profile`, `register_admin_tools`' condition, `definition_owner` (the write guard). |
| `server/src/tools/builtin_tools.rs` | the fs write tools call the guard; per-spec `kind`+`group` literals. |
| `server/src/heartbeat_config.rs`, `autonomous_config.rs` | frontmatter gains `profile`; carried onto `HeartbeatTask`/`AutonomousTask`. |
| `server/src/lib.rs` | group registration at startup; `validate_session_profiles`/`validate_task_profiles`; loaders carry the profile. |
| `server/src/agent.rs` | `handle_message` builds `Origin::Channel { session_profile }` and calls `allow`. |
| `server/src/serve/mod.rs` | `run_llm_turn`'s pin clone → `Origin::Acp/Unattended`; `visible_tool_predicate` and the turn gate go through `allow`. |
| `server/config.example.toml`, `server/templates/workspace/config.toml`, `README.md` | the new keys, documented in English. |

---

## Task 1: The types, and the rename that makes them the same field

**Files:**
- Modify: `crates/sapphire-agent-server/src/config.rs` (`room_profiles` → `session_profiles` (`:85`), `RoomProfileConfig` → `SessionProfileConfig` — `rooms`/`api_keys` keep existing for now, so every existing call site and test keeps compiling and passing, and only gain `permission`)
- Modify (mechanical rename, behaviour unchanged): `tools/config_tools.rs:142,156,172` (`current_admin_room_profile` → `current_session_profile`, plus a `current_permission_profile()` reading it), `serve/mod.rs:3239` + `serve/mod.rs:3301` (the pin `session_room_profiles` → `session_profiles`, `admin_room_profile` → `session_profile`), `serve/acp.rs` (the `namespace_for_room_profile` call sites), `provider/registry.rs:163`, `heartbeat.rs:95`, `agent.rs:279,338,462,776`, `config.example.toml`'s and `templates/workspace/config.toml`'s section names, `README.md`
- Test: inline `#[cfg(test)] mod tests` of `config.rs`

**Interfaces:**
- Consumes: nothing (this is the first task).
- Produces: `ToolGroup{Fs,Shell,Mcp,Admin}` (Copy+Eq), `SessionProfileConfig{profile, memory_namespace, session_policy, permission, voice_pipeline, devices}` (as `RoomProfileConfig` was, plus `permission`; `rooms`/`api_keys` are still read by the existing `room_profile_for` until Task 2 retires them), `PermissionProfileConfig{tools}`, `Config::session_profiles`, `Config::permission_profiles`, `Config::session_profile(&self, name) -> Option<&SessionProfileConfig>`, `Config::permission_profile(&self, name) -> PermissionProfileConfig`, `Config::permission_profile_name_for_session(&self, session_profile: &str) -> String` (that profile's `permission`, absent meaning `DEFAULT_PROFILE_NAME`), `PermissionProfileConfig::lists(&self, ToolGroup) -> bool`, `Config::group_registered(&self, ToolGroup) -> bool` (enabled ∧ listed by at least one effective profile — R7), `Config::session_profile_for(&self, declared: Option<&str>) -> String` (declared wins verbatim; absent means the implicit `DEFAULT_PROFILE_NAME`). Tasks 2-6 depend on exactly these names.

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
        assert!(cfg.permission_profile("default").lists(ToolGroup::Mcp));
    }

    /// Decision 3: an operator-written section wins over the built-in preset of the same
    /// name, and the built-in table is the fallback — same shape as the LLM-profile
    /// resolution. And what a session profile's `permission` names is what governs:
    /// absent means `"default"`, never the session profile's own name guessed at.
    #[tokio::test]
    async fn an_operator_permission_profile_overrides_a_preset_and_a_session_names_one() {
        let cfg = Config::parse_for_test(
            "[anthropic]\napi_key = \"test\"\n\n[permission_profile.default]\ntools = [\"shell\"]\n",
        );
        assert_eq!(cfg.permission_profile("default").tools, vec![ToolGroup::Shell]);

        let named = Config::parse_for_test(
            "[anthropic]\napi_key = \"test\"\n\n[session_profile.ops]\nprofile = \"dev\"\npermission = \"admin\"\n",
        );
        assert_eq!(named.session_profile("ops").expect("the section parses").permission.as_deref(), Some("admin"));
        // The chain: session profile -> the permission profile its `permission` names.
        assert_eq!(named.permission_profile_name_for_session("ops"), "admin");
        let unnamed = Config::parse_for_test(
            "[anthropic]\napi_key = \"test\"\n\n[session_profile.ops]\nprofile = \"dev\"\n",
        );
        assert_eq!(unnamed.permission_profile_name_for_session("ops"), DEFAULT_PROFILE_NAME);
    }

    /// R2/R7: two different keys. `fs` enabled here and listed by the profile the only
    /// session profile names: registered. `admin` enabled here and listed by nobody: not
    /// registered at all (decision 4). And the converse half: listed, off on this host.
    #[test]
    fn a_group_is_registered_only_when_enabled_here_and_listed_by_a_profile() {
        let both = Config::parse_for_test(
            "[anthropic]\napi_key = \"test\"\n\n\
             [session_profile.ops]\nprofile = \"dev\"\npermission = \"ops\"\n\
             [permission_profile.ops]\ntools = [\"fs\"]\n\n[tools.fs]\nenabled = true\n",
        );
        assert!(both.group_enabled(ToolGroup::Fs) && both.group_registered(ToolGroup::Fs));
        assert!(both.permission_lists("ops", ToolGroup::Fs));

        let off = Config::parse_for_test("[anthropic]\napi_key = \"test\"\n");
        assert!(!off.group_enabled(ToolGroup::Fs));
        assert!(!off.group_registered(ToolGroup::Fs), "listed by the `default` preset, switched off on this host");

        let unlisted = Config::parse_for_test(
            "[anthropic]\napi_key = \"test\"\n\n\
             [session_profile.ops]\nprofile = \"dev\"\npermission = \"chat\"\n\n[tools.admin]\nenabled = true\n",
        );
        assert!(unlisted.group_enabled(ToolGroup::Admin));
        assert!(!unlisted.group_registered(ToolGroup::Admin), "enabled, and the `admin` preset lists it, but no effective profile here does");
    }
```

- [ ] **Step 2: Run them to verify they fail** — `cargo test -p sapphire-agent-server config:: 2>&1 | tail -20`. Expected: compile error, `cannot find type \`ToolGroup\` in this scope`.

- [ ] **Step 3: Write the types and do the rename** — in `config.rs`, beside `SessionPolicy` (`:186`) and the `Config` struct:

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

/// `[session_profile.<n>]` — what kind of session this is (decision 1). Renamed
/// from `RoomProfileConfig` in this task; `rooms` and `api_keys` are still read
/// by `room_profile_for` until Task 2 retires them into the legacy capture.
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
    /// Still read by `room_profile_for` until Task 2; not what names a profile.
    #[serde(default)]
    pub rooms: Vec<String>,
    #[serde(default)]
    pub api_keys: Vec<String>,
}

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
    /// preset. An unknown name is a startup error (Task 2's validation), never a
    /// silently-chosen preset.
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

    /// The permission profile that governs a turn under session profile `session_profile`:
    /// that profile's `permission`, absent meaning `"default"`. This is the second half
    /// of the resolution — the session profile names a permission profile, and the
    /// permission profile is what decides (decision 3).
    pub fn permission_profile_name_for_session(&self, session_profile: &str) -> String {
        self.session_profile(session_profile)
            .and_then(|sp| sp.permission.clone())
            .unwrap_or_else(|| DEFAULT_PROFILE_NAME.to_string())
    }

    /// R3: whether the profile governing a turn under `session_profile` lists `group`.
    pub fn permission_lists(&self, session_profile: &str, group: ToolGroup) -> bool {
        let name = self.permission_profile_name_for_session(session_profile);
        self.permission_profile(&name).lists(group)
    }

    /// R2: whether this host hands the agent that key. Today every group answers with
    /// `[tools.host_access].enabled`, the one switch that exists until Task 2 splits it
    /// into `[tools.<group>]` — a fact about the deployment, not a row in the table.
    pub fn group_enabled(&self, group: ToolGroup) -> bool {
        self.tools.host_access.enabled
    }

    /// R7: registered at all ⇔ enabled here ∧ listed by at least one effective
    /// permission profile (presets and operator sections alike).
    pub fn group_registered(&self, group: ToolGroup) -> bool {
        self.group_enabled(group)
            && self.effective_permission_profiles()
                .iter()
                .any(|p| p.lists(group))
    }

    /// Every profile that decides anything here: every `[permission_profile.<n>]` section,
    /// plus every built-in preset at least one session profile falls through to.
    pub(crate) fn effective_permission_profiles(&self) -> Vec<PermissionProfileConfig> {
        let mut out = self.permission_profiles.values().cloned().collect::<Vec<_>>();
        for (preset, _) in BUILT_IN_PERMISSION_PROFILES {
            if self.session_profile(preset).is_none() {
                out.push(self.permission_profile(preset));
            }
        }
        out
    }

    /// The session profile `name` names, by name — `room_profile()` (`:1549`) under its
    /// new name, same body.
    pub fn session_profile(&self, name: &str) -> Option<&SessionProfileConfig> {
        self.session_profiles.get(name)
    }
}

impl PermissionProfileConfig {
    pub fn lists(&self, group: ToolGroup) -> bool {
        self.tools.contains(&group)
    }
}
```

The field rename is `#[serde(default, rename = "session_profile")] pub session_profiles: HashMap<String, SessionProfileConfig>` (`:85`) with every internal reader renamed too (`room_profile_for`→`session_profile_for_room` (`:1537`), `profile_for`→`profile_for_session` (`:1567`), `namespace_for_room_profile`→`namespace_for_session_profile` (`:1949`), `room_profile_name_for_room`→`session_profile_name_for_room` (`:1842`), `namespace_for_room` (`:1958`)); `validate_profiles` (`:1589`) now reports an unknown `permission` target alongside the LLM profile/namespace/voice checks, and `validate_subagent_profiles` (`:1740`) is untouched; `[acp].stdio_room_profile` (`:304`) becomes `stdio_session_profile` and `[cli].room_profile` (`:336`) becomes `profile`; `config_tools_enabled()`/`admin_allows_room_profile()` (`:1827`, `:1855`) keep answering today's `register_admin_tools` until Task 4 replaces their condition with `group_registered(Admin)`. `config.example.toml` and `templates/workspace/config.toml` get their `[room_profile.<n>]` sections renamed to `[session_profile.<n>]` in English (the `rooms` keys stay until Task 2 moves them), and `README.md` names the sections it already names under their new names.

- [ ] **Step 4: Run to verify they pass** — `cargo test -p sapphire-agent-server config:: 2>&1 | tail -15`; then `cargo clippy -p sapphire-agent-server --all-targets 2>&1 | tail -5`. Expected: all green including the pre-existing profile tests (renamed, assertions unchanged), zero warnings.

- [ ] **Step 5: Format and commit**

```bash
cargo fmt --all
git add -A
git commit -m "$(cat <<'EOF'
feat(config): add ToolGroup and the session/permission profile types

Co-authored-by: Saphina <spph12@sapphire-turtle.org>
EOF
)"
```

---

## Task 2: The declaration-side binding, the per-group host keys, and the four migration cases

**Files:**
- Modify: `crates/sapphire-agent-server/src/config.rs` (`MatrixConfig`/`DiscordConfig` room blocks, `SessionProfileConfig` loses `rooms`/`api_keys` into a capture-only `LegacyRoomProfile`, `ToolsConfig`'s per-group blocks replacing `host_access`, `migration_errors`)
- Modify: `crates/sapphire-agent-server/src/config_layer.rs` (`HOST_ONLY_PATHS`)
- Modify (mechanical, behaviour unchanged): the `room_ids`/`channel_ids` readers (`MatrixConfig.primary_room_id()`, `serve/acp.rs`), `tools/config_tools.rs`' `ROOM_REFUSAL` wording
- Test: `config.rs`'s `mod tests`, plus the renamed channel tests

**Interfaces:**
- Consumes: Task 1's `SessionProfileConfig`/`PermissionProfileConfig`/`Config::permission_profile`/`group_enabled`.
- Produces: `RoomConfig { room_id: String, profile: String, voice_pipeline: Option<String> }`; `MatrixConfig.rooms`/`DiscordConfig.rooms: Vec<RoomConfig>`; `Config::session_profile_name(&self, declared: Option<&str>, room_id: &str) -> String` — an explicit declaration wins **verbatim** (never silently dropped: an unknown name is what `validate_session_profiles` stops startup for, and `Config::session_profile_defined(name) -> bool` is what it reads), else the room block's, else `DEFAULT_PROFILE_NAME`; `Config::group_enabled` now reads the four per-group blocks.

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

    /// R1: an explicit declaration is carried verbatim — including one this host has no
    /// section for, which is what `validate_session_profiles` stops startup for. Silently
    /// falling through to a room's or the implicit profile is what a typo rotting into
    /// "I granted it but nothing changed" would mean.
    #[test]
    fn an_explicit_declaration_wins_verbatim_and_an_unknown_one_fails_startup() {
        let cfg = Config::parse_for_test(
            "[anthropic]\napi_key = \"test\"\n\n[session_profile.ops]\nprofile = \"dev\"\npermission = \"admin\"\nmemory_namespace = \"user_ops\"\n",
        );
        assert_eq!(cfg.session_profile_name(Some("ops"), "!any:srv"), "ops");
        assert_eq!(cfg.permission_profile_name_for_session("ops"), "admin");
        assert_eq!(cfg.namespace_for_session_profile("ops"), "user_ops");
        assert!(cfg.session_profile_defined("ops"));

        let unknown = Config::parse_for_test("[anthropic]\napi_key = \"test\"\n");
        // No `[session_profile.default]` on this host, and the name the frontmatter gave is
        // unknown: carried, and named. The room block does not override an explicit one.
        assert_eq!(cfg.session_profile_name(Some("opss"), "!any:srv"), "opss");
        assert!(!unknown.session_profile_defined("opss"));
        let errors = unknown.validate_session_profiles();
        assert!(errors.iter().any(|e| e.contains("'opss'") && e.contains("session_profile")), "{errors:?}");
        // Case 4: resolving to "default" while the host defines none is itself an error.
        assert!(errors.iter().any(|e| e.contains("no [session_profile.default]")), "{errors:?}");
    }

    /// Decision 8 cases 1, 2, 3: named, and startup stops.
    #[test]
    fn migration_leftovers_are_named_and_stop_startup() {
        // Cases 1 and 2 together: a leftover `[room_profile.<n>]`, still carrying `rooms`.
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
    }

    /// The per-group enabled keys are host-only; `tools.mcp_servers` and the
    /// listings stay synced. `starts_with` compares whole segments, so
    /// `["tools","mcp"]` does NOT cover `tools.mcp_servers`.
    #[test]
    fn per_group_enabled_stays_host_only_and_the_listings_stay_synced() {
        for group in ["fs", "shell", "mcp", "admin"] {
            assert!(!config_layer::path_allowed(&["tools", group]), "{group} is a fact about this deployment");
        }
        assert!(config_layer::path_allowed(&["tools", "mcp_servers"]));
        assert!(config_layer::path_allowed(&["permission_profile"]));
        assert!(config_layer::path_allowed(&["session_profile"]));
    }
```

- [ ] **Step 2: Run to verify it fails** — `cargo test -p sapphire-agent-server config:: 2>&1 | tail -20`. Expected: FAIL (`no field \`rooms\` on \`DiscordConfig\``, `no method named \`session_profile_name\``).

- [ ] **Step 3: Implement the shapes.** The channel configs (`:1112`, `:1168`):

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

`MatrixConfig.room_ids`/`DiscordConfig.channel_ids` are replaced by `rooms: Vec<RoomConfig>` (`primary_room_id()` becomes `self.rooms.first().map(|r| r.room_id.as_str())`; `allowed_users` stays channel-level — room-level override is out of scope). `SessionProfileConfig` loses `rooms`/`api_keys`, which move to the capture-only `LegacyRoomProfile { profile, rooms, api_keys }` on `Config::legacy_room_profiles` (`#[serde(default, rename = "room_profile")]`) — read by nothing but `migration_errors`, so a config still naming the old section cannot lose its grant silently. `ToolsConfig` keeps `tavily_api_key`/`mcp_servers`/`tool_rounds`/`subagent`/`background` and replaces `host_access` with `fs: ToolGroupConfig`/`shell: ToolGroupConfig`/`mcp: ToolGroupConfig` (`ToolGroupConfig { enabled: bool }`, same shape as today's `HostAccess`, `:968`) and `admin: AdminToolsConfig { enabled: bool, room_profiles: Vec<String> /* capture-only */ }`; `group_enabled` becomes

```rust
    /// R2: whether this host hands the agent that key (the per-group `[tools.<group>]`
    /// block, host-only — see `config_layer::HOST_ONLY_PATHS`).
    pub fn group_enabled(&self, group: ToolGroup) -> bool {
        match group {
            ToolGroup::Fs => self.tools.fs.enabled,
            ToolGroup::Shell => self.tools.shell.enabled,
            ToolGroup::Mcp => self.tools.mcp.enabled,
            ToolGroup::Admin => self.tools.admin.enabled,
        }
    }
```

and the resolution, which carries an explicit declaration rather than discarding one:

```rust
    /// The session profile a turn runs under: the session's own declared name when it has
    /// one — a heartbeat/autonomous frontmatter, an ACP pin, a device binding, a
    /// subagent delegation — else the `[<channel>.room<n>]` block's, else the implicit
    /// `"default"`. A declared name is carried verbatim: an unknown one is a startup error
    /// (`validate_session_profiles`), never a silent fall-through to a room's or the implicit
    /// profile, because a typo would rot into "I granted it but nothing changed".
    pub fn session_profile_name(&self, declared: Option<&str>, room_id: &str) -> String {
        if let Some(name) = declared {
            return name.to_string();
        }
        self.channels()
            .iter()
            .find_map(|(_, room)| (room.room_id == room_id).then(|| room.profile.clone()))
            .unwrap_or_else(|| DEFAULT_PROFILE_NAME.to_string())
    }

    /// R1: whether `name` is defined — the implicit `"default"` when the host defined a
    /// `[session_profile.default]`, or any section the host wrote.
    pub fn session_profile_defined(&self, name: &str) -> bool {
        name != DEFAULT_PROFILE_NAME || self.session_profiles.contains_key(name)
    }
```

`session_profile_defined`'s body is `self.session_profiles.contains_key(name) || name == DEFAULT_PROFILE_NAME && self.session_profiles.contains_key(DEFAULT_PROFILE_NAME)` as written in the plan's prose above: an unknown name is `false`, `"default"` is `true` only where the host wrote it. Callers `profile_for(room_id)`/`namespace_for_room(room_id)` (`agent.rs:279,338,462,776`, `heartbeat.rs:95`, `serve/mod.rs:303,1148,1732`, `provider/registry.rs:163`, `serve/acp.rs:1294,1501,1572`) become `profile_for_session(name)`/`namespace_for_session_profile(name)`; `config.example.toml` gains `[permission_profile.<n>]` + `[tools.<group>]` and its channel sections gain room blocks.

- [ ] **Step 4: Run everything** — `cargo test -p sapphire-agent-server 2>&1 | tail -15`; then `cargo clippy -p sapphire-agent-server --all-targets 2>&1 | tail -5`. Expected: green, zero warnings.

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

## Task 3: `ToolSpec` carries `kind` and `group`; `policy.rs` decides with one function

**Files:**
- Modify: `crates/sapphire-agent-server/src/provider/mod.rs` (`ToolSpec` gains the two fields, `:19-23`)
- Modify: every prod `ToolSpec` literal: `tools/builtin_tools.rs`, `tools/workspace_tools.rs`, `tools/session_tools.rs`, `tools/ambient_tools.rs`, `tools/skill_tools.rs`, `tools/attachment_tools.rs`, `tools/background_tools.rs`, `tools/config_tools.rs`, `tools/client_tools.rs`, `tools/subagent.rs`, `mcp_client/mod.rs:740`
- Modify: `crates/sapphire-agent-server/src/tools/policy.rs` (whole file)
- Modify (mechanical, call sites only — the end-to-end tests land in Task 6): `agent.rs:821-827`, `serve/mod.rs:2674,2692,2852-2920,2382-2410`
- Test: `policy.rs`'s `mod tests`, `agent.rs`'s gate tests (`:995-1099`) renamed

**Interfaces:**
- Consumes: Task 1's `ToolGroup`, Task 2's `session_profile_name`/`permission_profile_name_for_session`/`group_registered`.
- Produces: `ToolSpec { name, description, input_schema, kind: ToolKind, group: Option<ToolGroup> }`; `Origin::{Acp { mode: SessionMode, session_profile: String }, Channel { session_profile: String }, Unattended { session_profile: String }}`; `Allow{Allow,Ask,Deny}` (replacing `Decision`); `ResolvedPermission { name: String, permission: String, groups: Vec<ToolGroup>, mode: Option<SessionMode> }` with `ResolvedPermission::resolve(&Config, Origin) -> Self` and `lists(&self, ToolGroup) -> bool`; `ToolEntry { name: String, group: Option<ToolGroup>, enabled: bool, kind: ToolKind }` with `ToolEntry::from_spec(&ToolSpec, &Config) -> Self`; `pub fn allow(profile: &ResolvedPermission, entry: &ToolEntry) -> Allow`; `partition_without_asking(&ResolvedPermission, &[ToolCall], &[ToolEntry]) -> (Vec<ToolCall>, Vec<(String,String)>)`. `Origin::Trusted` is replaced by `Unattended` (R1: `/rpc`, voice and A2A sessions are governed by the profile their session belongs to, so they carry a name too).

- [ ] **Step 1: Write the failing tests**

```rust
    /// R2/R9: the table is group × permission profile, and the mode is what decides
    /// whether an *offered* one asks, from `kind` — today's `decide` rows, unchanged,
    /// including for a tool that carries no group.
    #[tokio::test]
    async fn allow_is_the_group_table_and_the_mode_row() {
        let entry = |name: &str, group: Option<ToolGroup>, enabled: bool, kind| ToolEntry {
            name: name.into(), group, enabled, kind,
        };
        let default_fs = ResolvedPermission {
            name: "default".into(),
            permission: "default".into(),
            groups: vec![ToolGroup::Fs],
            mode: None,
        };
        // A channel turn under `default`: `fs` is listed and enabled here, so it runs unasked.
        assert_eq!(allow(&default_fs, &entry("file_write", Some(ToolGroup::Fs), true, ToolKind::Edit)), Allow::Allow);
        // `shell` is refused because no profile named it (decision 3), whatever its kind.
        assert_eq!(allow(&default_fs, &entry("shell", Some(ToolGroup::Shell), true, ToolKind::Execute)), Allow::Deny);
        // R2's ∧ half: a group this host does not hand the agent is refused whatever lists it.
        assert_eq!(allow(&default_fs, &entry("file_write", Some(ToolGroup::Fs), false, ToolKind::Edit)), Allow::Deny);

        // R9: under `default` an offered edit asks; under `accept_edits` it does not, and
        // only `Execute`/`Other` still do; `bypass` asks nothing.
        let edits = ResolvedPermission { name: "default".into(), permission: "default".into(), groups: vec![ToolGroup::Fs], mode: Some(SessionMode::Default) };
        assert_eq!(allow(&edits, &entry("file_write", Some(ToolGroup::Fs), true, ToolKind::Edit)), Allow::Ask);
        let accepted = ResolvedPermission { mode: Some(SessionMode::AcceptEdits), ..edits.clone() };
        assert_eq!(allow(&accepted, &entry("file_write", Some(ToolGroup::Fs), true, ToolKind::Edit)), Allow::Allow);
        assert_eq!(allow(&accepted, &entry("shell", Some(ToolGroup::Shell), true, ToolKind::Execute)), Allow::Ask);
        let bypass = ResolvedPermission { mode: Some(SessionMode::Bypass), ..edits.clone() };
        assert_eq!(allow(&bypass, &entry("shell", Some(ToolGroup::Shell), true, ToolKind::Execute)), Allow::Allow);
    }

    /// R4: the always-allowed tools are outside the table's subject — `enabled` and the
    /// profile's listing are both beside the point. Their `kind` still meets the mode
    /// exactly as a grouped tool's does: the old early-return that would have skipped it dies.
    #[tokio::test]
    async fn always_allowed_tools_are_outside_the_table() {
        let chat = ResolvedPermission { name: "chat".into(), permission: "chat".into(), groups: vec![], mode: None };
        for kind in [ToolKind::Read, ToolKind::Fetch, ToolKind::Search, ToolKind::Think] {
            let entry = ToolEntry { name: "current_time".into(), group: None, enabled: false, kind };
            assert_eq!(allow(&chat, &entry), Allow::Allow, "{kind:?}");
        }
        // A delegation tool with no group is still asked under a mode that asks.
        let acp_default = ResolvedPermission { name: "chat".into(), permission: "chat".into(), groups: vec![], mode: Some(SessionMode::Default) };
        assert_eq!(allow(&acp_default, &ToolEntry { name: "subagent".into(), group: None, enabled: false, kind: ToolKind::Other }), Allow::Ask);
        assert_eq!(allow(&acp_default, &ToolEntry { name: "memory_read".into(), group: None, enabled: false, kind: ToolKind::Read }), Allow::Allow);
    }

    /// A channel turn never asks: the partition sees Allow and Deny only, and every refusal
    /// names the setting that would change it.
    #[tokio::test]
    async fn a_channel_turn_partitions_on_allow_alone() {
        let cfg = Config::parse_for_test(
            "[anthropic]\napi_key = \"test\"\n\n\
             [session_profile.ops]\nprofile = \"dev\"\npermission = \"default\"\n\n\
             [tools.fs]\nenabled = true\n[tools.shell]\nenabled = true\n",
        );
        // Declared wins verbatim; its `permission` names `default`, and `default` lists fs
        // and mcp — never shell.
        let profile = ResolvedPermission::resolve(&cfg, Origin::Channel { session_profile: cfg.session_profile_name(Some("ops"), "!a:srv") });
        assert_eq!(profile.name, "ops", "the turn carries the session-profile name it came with");
        assert_eq!(profile.permission, "default", "and the permission profile is what the chain resolves to");

        let entries = [
            ToolEntry { name: "file_write".into(), group: Some(ToolGroup::Fs), enabled: cfg.group_enabled(ToolGroup::Fs), kind: ToolKind::Edit },
            ToolEntry { name: "shell".into(), group: Some(ToolGroup::Shell), enabled: cfg.group_enabled(ToolGroup::Shell), kind: ToolKind::Execute },
            ToolEntry { name: "memory_add".into(), group: None, enabled: false, kind: ToolKind::Edit },
        ];
        let calls: Vec<crate::provider::ToolCall> = [("file_write", "c0"), ("shell", "c1"), ("memory_add", "c2")]
            .into_iter()
            .map(|(name, id)| crate::provider::ToolCall { id: id.into(), name: name.into(), input: serde_json::json!({}) })
            .collect();

        let (permitted, refused) = partition_without_asking(&profile, &calls, &entries);
        let kept: Vec<&str> = permitted.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(kept, vec!["file_write", "memory_add"]);
        let name_of = |id: &str| calls.iter().find(|c| c.id == id).map(|c| c.name.clone()).unwrap();
        let blocked: Vec<String> = refused.iter().map(|(id, _)| name_of(id)).collect();
        assert_eq!(blocked, vec!["shell"]);
        for (id, reason) in &refused {
            assert!(reason.contains(&name_of(id)), "{reason} names its tool");
            assert!(reason.contains("permission_profile"), "{reason} names the setting to change");
        }
        assert_eq!(permitted.len() + refused.len(), calls.len());
    }
```

- [ ] **Step 2: Run to verify it fails** — `cargo test -p sapphire-agent-server policy:: 2>&1 | tail -20`. Expected: compile error (`no field \`group\` on \`ToolSpec\``, `unknown type \`Allow\``).

- [ ] **Step 3: Write the fields and the policy.** `provider::ToolSpec` gains `pub kind: ToolKind` and `pub group: Option<ToolGroup>`; every literal gains both — `file_read`/`dir_list`/`dir_walk`/`image_read` `(their kind, Some(Fs))`, `file_write`/`file_append` `(Edit, Some(Fs))`, `file_delete` `(Delete, Some(Fs))`, `shell` `(Execute, Some(Shell))`, `heartbeat_config`/`autonomous_config`/`agent_config` `(Edit, Some(Admin))`, `task_test` `(Edit, Some(Admin))`, `weather`/`web_search` `(Fetch, None)`, `mcp_reconnect` and `McpTool` `(kind_for_trust(trust)`/`kind_for_annotations`'s kind (`Other` for reconnect, decision 5), Some(Mcp))`, and everything else the four groups do not name `(its kind, None)` — the always-allowed set, `session_list`/`session_read`/`refresh_system_prompt`/`ask_human`/`background_task`/the ambient and avatar tools/the delegation tools included. `Tool::kind()`'s default stays `Other` and **there is no `Tool::group()`**: the group is declared on the spec literal and nowhere else, because "the group decides whether a tool is offered at all" is registration knowledge, not behaviour — a second `group_of(name)` table beside these literals would be a second source of truth. `policy.rs` becomes:

```rust
/// The single decision function. Both turn loops build their `Origin`'s resolved
/// profile and pass it here with the tool's registration entry.
///
/// Allowed ⇔ `profile` lists `group` and `group` is enabled on this host. A tool
/// outside the table's subject (`group: None`) is allowed by the table either way (R4) —
/// what still applies to it is the mode, from `kind`, exactly as today.
pub fn allow(profile: &ResolvedPermission, entry: &ToolEntry) -> Allow {
    // A grouped tool is refused when either half of R2's ∧ is missing. An ungrouped
    // one is the table's subject for neither: `enabled` is a fact about a group, and
    // there is no group to list it.
    if entry.group.is_some_and(|g| !entry.enabled || !profile.lists(g)) {
        return Allow::Deny;
    }
    // `Edit`/`Delete`/`Move`/`SwitchMode` are the middle row, `Execute`/`Other` the
    // strict one, `Read`/`Search`/`Fetch`/`Think` the safe one — written so a
    // `ToolKind` variant added upstream lands in the strict bucket rather than silently
    // becoming safe.
    let risky = matches!(entry.kind, ToolKind::Execute | ToolKind::Other);
    let safe = matches!(entry.kind, ToolKind::Read | ToolKind::Search | ToolKind::Fetch | ToolKind::Think);
    match profile.mode {
        None => Allow::Allow,
        Some(SessionMode::Bypass) => Allow::Allow,
        Some(_) if safe => Allow::Allow,
        // Today's rows: `accept_edits` asks for the strict bucket only; `default` asks
        // for everything not safe.
        Some(SessionMode::AcceptEdits) if risky => Allow::Ask,
        Some(SessionMode::AcceptEdits) => Allow::Allow,
        Some(SessionMode::Default) => Allow::Ask,
    }
}

impl ResolvedPermission {
    /// One call per turn, at ingress. Two hops, both reading a config the operator wrote:
    /// the origin's session-profile name resolves to the permission profile that
    /// profile's `permission` names (absent means `"default"`), and that permission
    /// profile is what lists the groups. The ACP asking shape travels with the mode.
    pub fn resolve(config: &Config, origin: Origin) -> Self {
        let (name, mode) = match origin {
            Origin::Acp { mode, session_profile } => (session_profile, Some(mode)),
            Origin::Channel { session_profile } => (session_profile, None),
            Origin::Unattended { session_profile } => (session_profile, None),
        };
        let permission = config.permission_profile_name_for_session(&name);
        Self { groups: config.permission_profile(&permission).tools, name, permission, mode }
    }

    pub fn lists(&self, group: ToolGroup) -> bool {
        self.groups.contains(&group)
    }
}

impl ToolEntry {
    /// What registration knew, read off the spec the tool was registered with: its group,
    /// its kind, and this host's flag for that group. There is no name→group table.
    pub(crate) fn from_spec(spec: &crate::provider::ToolSpec, config: &Config) -> Self {
        Self {
            name: spec.name.to_string(),
            group: spec.group,
            kind: spec.kind,
            enabled: spec.group.is_some_and(|g| config.group_enabled(g)),
        }
    }
}
```

`decide`, `host_tool_denied`, `HOST_TOOLS`, `kind_of` and the `Trusted` variant die (the `enabled` flag inside `ToolEntry` is what absorbed `host_tool_denied`); `refusal_message`'s `Refusal::Unavailable` wording gains the names it must name — `permission_profile.<profile>.tools` and `[tools.<group>].enabled` — and `Refusal::NotOffered`/`NeedsApproval`/`UserDeclined` keep their wording. The two call sites become, at this stage, the mechanical shape (`agent.rs:821`: `let profile = ResolvedPermission::resolve(&self.config, Origin::Channel { session_profile: self.config.session_profile_name(None, &incoming.room_id) }); let entries = specs.iter().map(|s| ToolEntry::from_spec(s, &self.config)).collect::<Vec<_>>(); partition_without_asking(&profile, &offered, &entries)` — reading today's `specs_filtered`'s `Vec<ToolSpec>` until Task 4's `entries()` replaces `kinds()`; `serve/mod.rs:2852-2920`: the same `allow` for `Allow::Allow`/`Ask`→`progress.approve`/`Deny`), with `visible_tool_predicate`'s `host_tool_denied` clause reading `group_registered` and `has_client`/the capability flags unchanged.

- [ ] **Step 4: Run everything** — `cargo test -p sapphire-agent-server 2>&1 | tail -15` (the renamed `agent.rs` gate tests keep the old assertions' meaning: `file_write` allowed under `default`, `shell` refused, always-allowed tools unaffected). Expected: green.

- [ ] **Step 5: Format and commit**

```bash
cargo fmt --all
git add -A
git commit -m "$(cat <<'EOF'
refactor(policy)!: carry group and kind on the spec, decide with allow

Co-authored-by: Saphina <spph12@sapphire-turtle.org>
EOF
)"
```

---

## Task 4: Registration is per group, and the definitions stop being writable through `fs`

**Files:**
- Modify: `crates/sapphire-agent-server/src/tools/mod.rs` (`ToolSet::kinds()`→`entries()`, `default_tool_set` (`:318-388`) registers by group and gains `config: &Config`)
- Modify: `crates/sapphire-agent-server/src/tools/builtin_tools.rs` (the three write tools call the guard)
- Modify: `crates/sapphire-agent-server/src/tools/config_tools.rs` (`definition_owner`, `register_admin_tools`' condition, `test_workspace`)
- Test: `tools/mod.rs`'s `mod tests`, `config_tools.rs`'s `mod tests`

**Interfaces:**
- Consumes: `ToolSpec{kind, group}`/`ToolEntry::from_spec` (Task 3), `Config::group_registered` (Tasks 1-2).
- Produces: `ToolSet::entries(&self) -> Vec<(String, ToolEntry)>`; `default_tool_set(state, tavily_api_key, mcp_servers, day_boundary_hour, config: &Config)`; `config_tools::definition_owner(abs: &Path, workspace_root: &Path) -> Option<&'static str>` (the write guard, R6); `register_admin_tools`' condition `config.group_registered(ToolGroup::Admin)` (the `[tools.admin]` philosophy is kept — *not registered at all*).

- [ ] **Step 1: Write the failing tests**

```rust
    /// Decision 4/7: every tool is registered with the group that decides whether it is
    /// offered at all, and the kind that drives the mode's asking. A tool added without a
    /// `group` lands outside the table — so does one added without a `kind`, in `Other`,
    /// the strictest bucket.
    #[tokio::test]
    async fn every_tool_declares_its_group_and_kind() {
        let cfg = Config::parse_for_test("[anthropic]\napi_key = \"test\"\n");
        let tools = default_tool_set(test_workspace(), Some("test-tavily-key".to_string()), &[], 4, &cfg).await;
        let entries = tools.entries().await;
        let want: Vec<(&str, ToolKind, Option<ToolGroup>)> = vec![
            ("file_append", ToolKind::Edit, Some(ToolGroup::Fs)),
            ("file_delete", ToolKind::Delete, Some(ToolGroup::Fs)),
            ("file_read", ToolKind::Read, Some(ToolGroup::Fs)),
            ("file_write", ToolKind::Edit, Some(ToolGroup::Fs)),
            ("dir_list", ToolKind::Search, Some(ToolGroup::Fs)),
            ("dir_walk", ToolKind::Search, Some(ToolGroup::Fs)),
            ("image_read", ToolKind::Read, Some(ToolGroup::Fs)),
            ("shell", ToolKind::Execute, Some(ToolGroup::Shell)),
            ("mcp_reconnect", ToolKind::Other, Some(ToolGroup::Mcp)),
            ("task_test", ToolKind::Edit, Some(ToolGroup::Admin)),
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

    /// R6: every definition file is the `admin` group's land — modifiable only by the
    /// dedicated tool that owns it, on every spelling the writer will open it with.
    #[tokio::test]
    async fn a_fs_write_tool_refuses_a_definition_file_naming_its_owner() {
        let (_, root, ws) = config_tools::test_workspace();
        std::fs::create_dir_all(root.join("heartbeat")).unwrap();
        std::fs::write(&root.join("heartbeat/morning_call.md"), "---\n---\n\nold\n").unwrap();
        let write = crate::tools::builtin_tools::FileWriteTool::new(Arc::clone(&ws));
        // The path the writer opens is what is compared: `expand_path`'s output (so a
        // `~/...` and a workspace-relative one are the same file to it) and
        // `lexically_normalize`'s (so `nope/../` and `./` and a `..` detour are).
        for spelling in [
            format!("heartbeat/morning_call.md"),
            format!("./heartbeat/./morning_call.md"),
            format!("nope/../heartbeat/../heartbeat/morning_call.md"),
            root.join("heartbeat/morning_call.md").to_string_lossy().to_string(),
        ] {
            let err = write.execute(&json!({"path": spelling, "content": "new"})).await.unwrap_err();
            assert!(err.to_string().contains("heartbeat_config"), "{spelling} -> {err}");
        }
        // The config definition files are covered too, by the same comparison.
        assert!(write.execute(&json!({"path": ".sapphire-agent/config.toml", "content": ""})).await.is_err());
        // And the read half keeps reading them — reading one's own task is not a rewrite risk.
        let read = crate::tools::builtin_tools::FileReadTool::new(Arc::clone(&ws));
        assert!(read.execute(&json!({"path": "heartbeat/morning_call.md"})).await.unwrap().contains("old"));
    }
```

- [ ] **Step 2: Run to verify it fails** — `cargo test -p sapphire-agent-server tools:: 2>&1 | tail -20`. Expected: FAIL (`no method named \`entries\``, `no field \`config\` on default_tool_set`).

- [ ] **Step 3: Implement.** `ToolSet::entries()` is today's `kinds()` (`:139`) with the group read off the same spec — `self.specs.iter().map(|s| (s.name.to_string(), ToolEntry::from_spec(s, &self.config))).collect()` — and `default_tool_set` builds its `Vec<Box<dyn Tool>>` from the groups it was handed (`group_registered`, same shape as today's "registered when the tavily key exists"; `register_admin_tools` (`config_tools.rs:947`) keeps its own loop over `ConfigDir::ALL` and its own early return on `config.group_registered(ToolGroup::Admin)`). The guard:

```rust
/// Which dedicated tool owns the definition file `abs` is, `None` for a file that is not
/// a definition. `abs` is the path the writer is about to open — `expand_path` already
/// applied, `lexically_normalize` already collapsed — and the comparison is the same
/// raw-and-collapsed pair `refuse_if_sensitive_in` uses, so no spelling of a definition
/// file slips through: a `~/...`, an absolute path, a `./`, and a `nope/../` detour
/// all name the same file. Covers the `ConfigDir` directories' Markdown *and* the config
/// definition files (`PROTECTED_CONFIG_FILES`) — R6 names both.
pub(crate) fn definition_owner(abs: &Path, workspace_root: &Path) -> Option<&'static str> {
    let raw = abs.to_string_lossy().to_string();
    let collapsed = lexically_normalize(abs).to_string_lossy().to_string().replace('\\', "/");
    for candidate in [&raw, &collapsed] {
        let rel = Path::new(candidate.as_str()).strip_prefix(workspace_root).ok()?;
        if rel.extension().is_some_and(|e| e.eq_ignore_ascii_case("md"))
            && let Some(dir) = ConfigDir::ALL.iter().find(|d| d.dir_name() == rel.iter().next()?.to_str()?)
        {
            return Some(dir.tool_name());
        }
        if PROTECTED_CONFIG_FILES.iter().any(|f| rel.file_name().is_some_and(|n| n == **f)) {
            return Some("the config definition file");
        }
    }
    None
}
```

called right after each write tool's existing `let path = expand_path(path_str);` and `refuse_if_sensitive(..)?`, from `FileWriteTool::execute`/`FileAppendTool::execute`/`FileDeleteTool::execute`, with `bail!("{verb} '{path}' is a definition file, owned by {owner}: use it.")`; `ShellTool` needs nothing (under `default` it is not listed, so it is not offered).

- [ ] **Step 4: Run everything** — `cargo test -p sapphire-agent-server 2>&1 | tail -15`; `cargo clippy -p sapphire-agent-server --all-targets 2>&1 | tail -5`. Expected: green, zero warnings.

- [ ] **Step 5: Format and commit**

```bash
cargo fmt --all
git add -A
git commit -m "$(cat <<'EOF'
feat(tools)!: register by group, guard the definitions on the opened path

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

- [ ] **Step 3: Implement.** `validate_task_profiles` reports every task profile that names no defined session profile (`!cfg.session_profile_defined(name)`, the two loaders' `deny_unknown_fields`/`warn!`-skip shape unchanged); `heartbeat.rs`'s `DueTask` gains `profile: Option<String>` and `fire_task` hands the task's name plus the resolved profile to the turn it builds (`Heartbeat`'s chat leg goes through `Agent::trigger(name, prompt, room_id, profile)`, and the ACP/autonomous leg through `run_llm_turn`'s pin clone); `lib.rs`'s startup keeps `validate_profiles`' renamed twin and adds `validate_session_profiles` for the frontmatter/room-block names, and every `default_tool_set`/`register_tool` call site passes the config so `group_registered` answers it — the session tools (`lib.rs:1140-1160`) and `register_admin_tools` (`:1129`) included, so an `admin` group named by nobody registers none of the four.

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
- Modify: `crates/sapphire-agent-server/src/agent.rs:776,821` (`IncomingMessage`'s room → `Origin::Channel { session_profile }`, `partition_without_asking(&profile, ..)` — `ns` from `namespace_for_session_profile`)
- Modify: `crates/sapphire-agent-server/src/serve/mod.rs` (the pin clone → `Origin::Acp { mode, session_profile }` / `Origin::Unattended { session_profile }`; `TurnContext`/`TurnLoop` field `session_profile`; `TurnLoop::run`'s gate calls `allow`; `visible_tool_predicate` (`:2382`) keeps the capability flags and `has_client`, and drops a shared name only when its group is unregistered or unroutable here)
- Modify: `tools/subagent.rs` + `tools/skill_tools.rs`' `TurnContext` literals
- Test: `agent.rs`'s and `serve/mod.rs`'s turn tests below — each runs a real turn through `handle_message`/`run_llm_turn` with a per-instance probe with a handle (N4), and reads what the turn actually did to the filesystem and to the stored history

**Interfaces:**
- Consumes: `allow`/`ResolvedPermission`/`ToolEntry` (Task 3), `ToolSpec{kind,group}`/`ToolSet::entries()` (Task 4), the loaders carrying `profile` (Task 5).
- Produces: nothing (the last task). The end-to-end shape is what it pins.

- [ ] **Step 1: Write the failing end-to-end tests** — the fixture (in `agent.rs`'s `mod tests`; the ACP test below sits in `serve/mod.rs`'s own `mod tests` and reuses its own `tool_round`/`final_round`/`persisted`/`texts_of` helpers there):

```rust
    /// The session profile the tests pin, the permission profile it names, and the groups that
    /// profile lists — with **every** group enabled on this host, so what a turn may use is
    /// decided only by the listing, never by a host key a test forgot to set. The channel
    /// name is `incoming()`'s own `"test-room"`, so what the channel loop resolves is the
    /// room block's name (decision 2), and each test names the whole chain itself.
    fn gate_config(session_profile: &str, permission: &str, groups: &[&str]) -> Config {
        let enabled = ["fs", "shell", "mcp", "admin"]
            .iter()
            .map(|g| format!("\n[tools.{g}]\nenabled = true\n"))
            .collect::<String>();
        Config::parse_for_test(&format!(
            "[anthropic]\napi_key = \"test\"\n\n[profiles.default]\nprovider = \"stub\"\n\n\
             [session_profile.{session_profile}]\nprofile = \"default\"\npermission = \"{permission}\"\n\n\
             [permission_profile.{permission}]\ntools = [{groups}]\n{enabled}"
        ))
    }

    /// One channel turn through `handle_message`: `name`'s call, then a final round with no
    /// tool_use. Returns the handles the assertions read — what the channel was told, the
    /// workspace root the tools write into, and the probe.
    fn gate_channel_turn(name: &str, input: Value, cfg: Config) -> (Vec<OutgoingMessage>, PathBuf, Arc<GateProbe>) {
        let state = test_workspace();   // agent.rs's existing helper, Arc<WorkspaceState>, tempdir leaked on purpose
        let root = state.workspace.root.clone();
        let tools = crate::tools::default_tool_set(Arc::clone(&state), None, &[], 4, &cfg).await;
        let probe = GateProbe::new();
        tools.register_tool(Box::new(Arc::clone(&probe))).await;
        let responses = vec![
            crate::provider::ChatResponse {
                prompt_usage: None,
                text: None,
                tool_calls: vec![crate::provider::ToolCall { id: "c0".into(), name: name.into(), input }],
                stop_reason: None,
            },
            crate::provider::ChatResponse {
                prompt_usage: None,
                text: Some("done".into()),
                tool_calls: Vec::new(),
                stop_reason: None,
            },
        ];
        let ((agent, sent, _typing), _log) =
            build_test_agent_with_tools(responses, false, Some((cfg, tools)));
        agent.handle_message(&incoming("go")).await.unwrap();
        (sent.lock().unwrap().clone(), root, probe)
    }

    /// The honest instrument: it records the pair the surrounding tool execution was scoped
    /// with, which is only observable from inside a tool. `group: None`, so no group and no
    /// capability gates on it, and it is offered on both loops. Its own `impl Tool` plus the
    /// `impl Tool for Arc<Self>` forwarding, exactly as `AdminProfileProbe` does today, and its
    /// `execute` writes `*self.seen.lock().unwrap() = Some((current_session_profile().expect("a turn scopes its session profile"), current_permission_profile().expect("and its permission profile")));`.
    /// `seen()` is what the assertions read: `None` means the call never reached execution,
    /// which is what "not even offered" means.
    #[cfg(test)]
    pub(crate) struct GateProbe {
        seen: std::sync::Mutex<Option<(String, String)>>,
        spec: crate::provider::ToolSpec,   // name `"gate_probe"`, `kind: ToolKind::Read`, `group: None`
    }
```

The tests:

```rust
    /// N2/R2, through `handle_message`: a room whose block names a profile listing `fs` ran
    /// `file_write` unasked — a channel turn has nobody to ask, so it ran because it was
    /// allowed — and carried both resolved names into the execution scope.
    #[tokio::test]
    async fn a_channel_turn_under_default_runs_file_write_unasked() {
        let (sent, root, probe) = gate_channel_turn("file_write", json!({"path": "notes/x.md", "content": "written by the turn"}), gate_config("default", "default", &["\"fs\""])).await;
        assert_eq!(std::fs::read_to_string(root.join("notes/x.md")).unwrap(), "written by the turn");
        assert_eq!(probe.seen(), Some(("default".into(), "default".into())));
        assert!(sent.iter().any(|m| m.content == "done"), "the turn completed: {sent:?}");
    }

    /// Under `chat` even `fs` is invisible: registered (some profile lists it somewhere), and
    /// this turn's profile does not list it.
    #[tokio::test]
    async fn a_turn_under_chat_does_not_even_see_fs() {
        let (sent, root, probe) = gate_channel_turn("file_write", json!({"path": "notes/x.md", "content": "x"}), gate_config("chat", "chat", &[])).await;
        assert_eq!(probe.seen(), None, "the call never reached execution");
        assert!(!root.join("notes/x.md").exists(), "and so it wrote nothing");
        assert!(sent.iter().any(|m| m.content.contains("file_write") && m.content.contains("permission_profile")), "{sent:?}");
    }

    /// `shell` refuses under `default` and runs under `admin`; and a turn under `default` cannot
    /// see the admin tools even where the `admin` preset is what made them registered.
    #[tokio::test]
    async fn shell_refuses_under_default_and_runs_under_admin() {
        let (sent, _root, probe) = gate_channel_turn("shell", json!({"command": "true"}), gate_config("default", "default", &["\"fs\""])).await;
        assert_eq!(probe.seen(), None, "not listed, so not offered");
        assert!(sent.iter().any(|m| m.content.contains("shell") && m.content.contains("permission_profile")), "{sent:?}");

        let (_sent, _root, probe) = gate_channel_turn("heartbeat_config", json!({"action": "list"}), gate_config("default", "default", &["\"fs\""])).await;
        assert_eq!(probe.seen(), None, "registered — the `admin` preset lists it somewhere — and still invisible here");

        let (_sent, _root, probe) = gate_channel_turn("shell", json!({"command": "true"}), gate_config("admin", "admin", &["\"shell\""])).await;
        assert_eq!(probe.seen(), Some(("admin".into(), "admin".into())), "and a profile that names it runs it");
    }

    /// The same function, the other loop: `run_llm_turn` resolves the pin `session/new` wrote
    /// and hands it to `allow`. Listing ⇒ the call ran and the probe recorded both names;
    /// not listing ⇒ the name was never offered, and the refusal that reached the stored
    /// history names the setting. Proved through execution, not by reading a field.
    #[tokio::test]
    async fn the_acp_loop_resolves_its_pin_through_the_same_allow() {
        for (profile, groups, ran) in [("chat", &[] as &[&str], false), ("ops", &["\"fs\""], true)] {
            let mut state = ServeState::for_test_scripted(true, vec![tool_round("call-1"), final_round("done")]);
            Arc::get_mut(&mut state).expect("a fresh state has one owner").config = gate_config(profile, profile, groups);
            let probe = GateProbe::new();
            state.tools.register_tool(Box::new(Arc::clone(&probe))).await;
            let sid = format!("gate-{profile}");
            // What `session/new` writes for an ACP session: that it is one, and the name.
            state.acp_sessions.lock().await.insert(sid.clone());
            state.session_profiles.lock().await.insert(sid.clone(), profile.into());
            run_llm_turn(Arc::clone(&state), sid.clone(), ChatMessage::user("go"), Arc::new(NullProgress), None).await;
            assert_eq!(probe.seen().is_some(), ran, "{profile}: offered to this turn's profile, or not");
            if ran {
                assert_eq!(probe.seen(), Some((profile.into(), profile.into())));
            } else {
                let stored = persisted(&state, &sid).await;
                assert!(stored.iter().any(|m| texts_of(m).iter().any(|t| t.contains("file_write") && t.contains("permission_profile"))), "{profile}: {stored:?}");
            }
        }
    }

    /// R1: a name arriving from a declaration rather than a room block — an unattended task's
    /// frontmatter (Task 5's loader carried it here) — wins verbatim.
    #[tokio::test]
    async fn an_unattended_turn_carries_the_profile_its_task_declared() {
        let (_d, root, _ws) = config_tools::test_workspace();
        std::fs::write(&root.join("heartbeat/morning_call.md"), "---\nschedule: \"0 8 * * *\"\nroom_id: \"test-room\"\nprofile: \"ops\"\nenabled: true\n---\n\n# Morning Call\n").unwrap();
        let tasks = crate::heartbeat_config::load_heartbeat_dir(&root.join("heartbeat"));
        let cfg = gate_config("default", "default", &["\"fs\""]);
        assert_eq!(cfg.session_profile_name(tasks[0].meta.profile.as_deref(), "test-room"), "ops");
    }

    /// The guard, end to end: `file_write` refuses `heartbeat/morning_call.md` naming
    /// `heartbeat_config` and wrote nothing, `file_read` reads it, and only `heartbeat_config`
    /// **modifies** it — proved by what is on disk afterwards, not by a returned string.
    #[tokio::test]
    async fn only_the_dedicated_tool_modifies_a_definition() {
        let (dir, root, ws) = config_tools::test_workspace();
        std::fs::create_dir_all(root.join("heartbeat")).unwrap();
        let task_file = root.join("heartbeat/morning_call.md");
        std::fs::write(&task_file, "---\nschedule: \"0 8 * * *\"\n---\n\nold\n").unwrap();
        let write = crate::tools::builtin_tools::FileWriteTool::new(Arc::clone(&ws));
        let err = write.execute(&json!({"path": "heartbeat/morning_call.md", "content": "new"})).await.unwrap_err();
        assert!(err.to_string().contains("heartbeat_config"), "{err}");
        assert_eq!(std::fs::read_to_string(&task_file).unwrap(), "---\nschedule: \"0 8 * * *\"\n---\n\nold\n", "the refused write wrote nothing");
        let read = crate::tools::builtin_tools::FileReadTool::new(Arc::clone(&ws));
        assert!(read.execute(&json!({"path": "heartbeat/morning_call.md"})).await.unwrap().contains("old"));

        let admin = ConfigTool::new(ConfigDir::Heartbeat, root.clone(), gate_config("admin", "admin", &["\"admin\""]), Arc::clone(&ws), None, std::sync::Weak::new());
        assert!(admin.execute(&json!({"action": "write", "name": "morning_call", "content": "new"})).await.is_ok());
        assert_eq!(std::fs::read_to_string(&task_file).unwrap(), "new", "the dedicated tool modified it");
        let _ = dir;
    }
```

- [ ] **Step 2: Run to verify they fail** — `cargo test -p sapphire-agent-server agent:: serve:: 2>&1 | tail -25`. Expected: FAIL (the loops still route a name through a table rather than through `allow`'s `ToolEntry`, and a turn still reaches execution with no profile scoped at all).

- [ ] **Step 3: Finish the wiring — every origin carries a resolved `String`, so no transport loses its binding.** `agent.rs`: `Origin::Channel { session_profile: self.config.session_profile_name(None, &incoming.room_id) }` (a channel message declares nothing; the room block *is* the declaration). `serve/mod.rs`: the pin clone is `Option<String>` and `Origin` needs `String`, so it is handed to `session_profile_name` **as `Option<&str>`**, which returns the resolved `String` both `Origin::Acp` and `Origin::Unattended` carry: pinned ⇒ the pinned name, unpinned ⇒ `DEFAULT_PROFILE_NAME`, and never silently a room's instead of a binding's. So `Origin::Acp { mode: progress.mode(), session_profile: state.session_profile_name(pin.as_deref(), &session_id) }` for an ACP turn, and `Origin::Unattended { session_profile: config.session_profile_name(pin.or(device_binding).as_deref(), room_id) }` for `/rpc`, voice (reverse-looked-up from the `voice/subscribe` registration) and A2A — the device/voice/unattended bindings stay sources of a resolved name (R1), not of a fallback. The turn's `entries` come from `tools.entries()` (so the profile that names no `admin` cannot see the four even though the `admin` preset registered them). `[acp].stdio_session_profile` and the `cli` block keep being the only sources for stdio ACP and the local clients — the principle of not adding sources of truth is unchanged (decision 2). `subagent` keeps inheriting the delegating turn's profile through `TurnContext`, and `TurnContext`/`TurnLoop`'s renamed `session_profile` field is set at every literal (`tools/subagent.rs`, `tools/skill_tools.rs`).

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
