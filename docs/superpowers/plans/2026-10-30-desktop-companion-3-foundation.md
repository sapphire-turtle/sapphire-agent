# Desktop Companion 3: Shared Foundation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build everything the 2D (sprite) and 3D (VRM) renderers share — the reshaped `avatar.toml`, `set_pose`, avatar sync and cache, avatar state, display-mode choice, the renderer contract with a placeholder renderer, gestures, the companion overlay UI, the transparent click-through desktop window and the "Character" settings page frame — so plans 4-2D and 4-3D can proceed in parallel touching only `avatar/sprite` / `avatar/vrm`.

**Architecture:** Server side, `AvatarConfig` becomes `{vrm?, sprite?}` and a third avatar tool, `set_pose`, joins `set_expression` / `play_action`. GUI side, `crates/sapphire-agent-gui/src/avatar/` gains renderer-independent modules; a renderer is a bevy plugin gated on `ActiveRenderer`, reading `AvatarState` + `AvatarAssets` and writing `DeclaredAvatar`, `AvatarBounds` and load errors. The chat view becomes a companion overlay (bubble, input, history, menu) driven by gestures over `AvatarBounds`; the desktop bin turns the window into a transparent, borderless, always-on-top, click-through companion window.

**Tech Stack:** Rust 2024, bevy 0.20, bevy_egui fork (egui 0.37), sapphire-framework-gui 0.23 (`Settings`, `SettingsSection`), tokio, serde/toml, sha2, corundum-framework-platform 0.3 (`dirs::AppDirs`).

**Spec:** `docs/superpowers/specs/2026-10-28-desktop-companion-design.md` — this plan is work-order step 6 (§3, §4, §5, §6 `set_pose`, §7, §8, §9, §10, §14; §13 only where the foundation must know it: sprite config shape and validation).

## Global Constraints

- Naming: docs say "desktop companion"; code identifiers and comments say `companion`; never "mate" in new code or comments (rename the three existing "desktop mate" comments in Task 1/2).
- `APP_API` stays **3**; only its doc history changes (spec §10): "3: `chat.send` takes `attachments` and `client`; `chat.progress` carries `expression`, `action` and `pose`; added `set_pose` and `sapphire-agent.avatar.*`".
- `avatar.toml` shape (spec §7), no reading of the old top-level `model`/`idle`/`actions` form.
- Sprite expression keys: only `happy`, `angry`, `sad`, `relaxed`, `surprised` (spec §13).
- Revert timer default **30 s**, shared by expression and pose (spec §8); expression/pose transitions ≈ **0.2 s** (renderer concern).
- Manifest poll: on connect, after own upload, every **5 min** (spec §7).
- Nothing in `crates/sapphire-agent-gui` is `cfg(target_os)`; desktop-only window behavior lives in `desktop/` (spec §3).
- App directories come from corundum: `corundum_framework_platform::dirs::AppDirs::new(sapphire_agent_core::APP)` (`.cache()` / `.config()`), never `dirs` / `directories` directly, and the app name only from core's `APP` (`"sapphire-agent"`, the same one the server and CLI use). GUI files under them: cache `companion-avatar/`, config `companion.toml` — names that cannot collide with the server's own files there (`config.toml`, `keys.toml`, `images/`, …).
- Pure logic (state, mode choice, gestures, manifest diff, bubble timing, prefs clamping) has no bevy dependency beyond `#[derive(Resource)]` and is unit-tested without starting bevy (spec §3, Tests).
- Commit scopes: `(rpc)` core, `(chat)` / `(tools)` cli, `(gui)` gui crate, `(desktop)` desktop. Commit messages end with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.
- Verification per task: `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, and the task's tests; full `cargo test --workspace` before the last commit of a task that touches cli or core.

## Review Focus

1. **Cached file changed on the server between `manifest` and `read`** (or a truncated download) → the bytes' sha256 ≠ manifest's; must be discarded, not cached under the manifest hash, and the previous good avatar kept until the next cycle. Test in Task 3.
2. **Click-through: cursor leaves the character with hit-testing off, then comes back** → the window must take clicks again; with hit-testing off winit sends no cursor events, so the presenter must poll the global cursor. On a Wayland session (no global cursor) click-through stays off rather than leaving an unclickable character. Test (pure `wants_pointer`) in Task 5, behavior in Task 6.
3. **A reply arrives while the input box is open and the user is mid-draft** → the bubble shows, the draft and focus stay. Test in Task 5.
4. **No avatar configured at all** (empty `avatar/`, `{files: []}` manifest, or the chosen mode's section missing) → placeholder renderer, no `client.avatar` declared, so the turn offers no avatar tools. Tests in Tasks 3 and 4.
5. **Saved window position on a monitor that is gone** → the window opens on a visible monitor, not off-screen. Test in Task 6.

---

### Task 1: Reshape `avatar.toml` into `[vrm]` / `[sprite]` (core + cli store)

**Files:**
- Modify: `crates/sapphire-agent-core/src/lib.rs` (`AvatarConfig` ~L435–446, module doc ~L10–31, `APP_API` doc ~L47–53, tests)
- Modify: `cli/src/avatar_store.rs` (`set_config` ~L354, `remove` ~L384, module doc L1–15, tests)
- Modify: `cli/src/serve/avatar_ipc.rs` (tests that build `AvatarConfig` ~L215–250)
- Modify: `cli/src/main.rs:56` (`APP_CTX` named by `sapphire_agent_core::APP`)

**Interfaces:**
- Produces (core, all `Clone, Debug, Default, Serialize, Deserialize, PartialEq`; `Option`s and empty maps skipped when serializing):
  - `pub struct AvatarConfig { pub vrm: Option<VrmConfig>, pub sprite: Option<SpriteConfig> }`
  - `pub struct VrmConfig { pub model: String, pub idle: Option<String>, pub actions: BTreeMap<String, String> }`
  - `pub struct SpriteConfig { pub default_pose: String, pub poses: BTreeMap<String, SpritePose> }`
  - `pub struct SpritePose { pub base: String, pub expressions: BTreeMap<String, String> }`
  - `pub const SPRITE_EXPRESSIONS: [&str; 5] = ["happy", "angry", "sad", "relaxed", "surprised"];`
  - `impl AvatarConfig { pub fn paths(&self) -> Vec<&str>; pub fn check(&self) -> Result<(), String> }` — `paths` lists every file the config references (vrm model, idle, actions; every pose's base and expressions); `check` enforces the structural rules below without touching disk, so the GUI can use it before uploading.
- `AvatarManifest { #[serde(flatten)] config, files }` is unchanged in Rust; on the wire it becomes `{vrm?, sprite?, files}`.

`check` rules (error text names the offending key): `sprite.poses` non-empty; `default_pose` ∈ `poses`; no empty pose, action or expression name; every sprite expression key ∈ `SPRITE_EXPRESSIONS` (so `neutral` is refused too — the base is neutral).

- [ ] **Step 1: Write the failing core tests** in `lib.rs` `mod tests`:

```rust
#[test]
fn avatar_toml_round_trips_both_sections() {
    let text = r#"
[vrm]
model = "vrm/model.vrm"
[vrm.actions]
peace = "vrm/peace.vrma"
[sprite]
default_pose = "normal"
[sprite.poses.normal]
base = "sprite/normal.png"
[sprite.poses.normal.expressions]
happy = "sprite/normal-happy.png"
"#;
    let cfg: AvatarConfig = toml::from_str(text).unwrap();
    assert_eq!(cfg.vrm.as_ref().unwrap().model, "vrm/model.vrm");
    assert_eq!(cfg.sprite.as_ref().unwrap().poses["normal"].expressions["happy"], "sprite/normal-happy.png");
    let mut paths = cfg.paths();
    paths.sort();
    assert_eq!(paths, ["sprite/normal-happy.png", "sprite/normal.png", "vrm/model.vrm", "vrm/peace.vrma"]);
    assert_eq!(toml::from_str::<AvatarConfig>(&toml::to_string(&cfg).unwrap()).unwrap(), cfg);
    assert_eq!(cfg.check(), Ok(()));
}

#[test]
fn sprite_rules_are_checked() {
    let pose = |base: &str, ex: &[(&str, &str)]| SpritePose {
        base: base.into(),
        expressions: ex.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
    };
    let sprite = |default: &str, poses: Vec<(&str, SpritePose)>| AvatarConfig {
        vrm: None,
        sprite: Some(SpriteConfig {
            default_pose: default.into(),
            poses: poses.into_iter().map(|(k, p)| (k.to_string(), p)).collect(),
        }),
    };
    assert!(sprite("normal", vec![]).check().unwrap_err().contains("poses"));
    assert!(sprite("missing", vec![("normal", pose("a.png", &[]))]).check().unwrap_err().contains("missing"));
    assert!(sprite("normal", vec![("normal", pose("a.png", &[("neutral", "b.png")]))]).check().unwrap_err().contains("neutral"));
    assert!(sprite("normal", vec![("normal", pose("a.png", &[("smug", "b.png")]))]).check().unwrap_err().contains("smug"));
    assert!(sprite("", vec![("", pose("a.png", &[]))]).check().is_err());
    assert_eq!(AvatarConfig::default().check(), Ok(()));
}
```

`toml` is a dev-dependency of core only if not already present; add `toml = "1"` under `[dev-dependencies]` if needed.

- [ ] **Step 2: Run** `cargo test -p sapphire-agent-core avatar_toml sprite_rules` — Expected: FAIL to compile (`VrmConfig` etc. undefined).
- [ ] **Step 3: Implement** the types, `paths`, `check` and `SPRITE_EXPRESSIONS` in core; update the module doc's avatar bullet and the `APP_API` history text (Global Constraints). Fix the existing core test(s) that built the old `AvatarConfig`.
- [ ] **Step 4: Run** core tests — Expected: PASS.
- [ ] **Step 5: Write the failing store tests** in `avatar_store.rs` `mod tests` (reuse `Fx` / `put`):
  - `set_config_checks_every_sprite_and_vrm_path`: with only `sprite/normal.png` present, a config whose pose `normal` has `expressions.happy = "sprite/h.png"` is refused with a message containing `sprite/h.png`; after `put(&f, "sprite/h.png", ..)` it is accepted and `manifest().config` equals it.
  - `set_config_refuses_structural_errors_before_touching_disk`: a config with `default_pose = "x"` and pose `normal` returns `AvatarError::Invalid(m)` with `m` containing `x`, and `avatar.toml` does not exist afterwards.
  - `remove_refuses_a_file_any_section_uses`: with `vrm.model = "m.vrm"` and pose base `p.png`, removing either is refused with "in use by avatar.toml"; removing an unreferenced file succeeds.
  - Update the existing tests that build `AvatarConfig { model, idle, actions }` to the new shape.
- [ ] **Step 6: Run** `cargo test -p sapphire-agent avatar_store` — Expected: FAIL.
- [ ] **Step 7: Implement:** `set_config` runs `config.check()` (→ `AvatarError::Invalid`), then validates and checks existence of each `config.paths()` entry; `remove` uses `cfg.paths()`. Replace "desktop mate" in the module doc with "desktop companion" and describe the two sections. Update `avatar_ipc.rs` tests to the new shape. In `cli/src/main.rs:56` name the app by core's constant: `AppContext::new(sapphire_agent_core::APP)` (same string, one definition).
- [ ] **Step 8: Run** `cargo test --workspace` (cli + core) — Expected: PASS.
- [ ] **Step 9: Commit** (two commits: `refactor(rpc): split the avatar config into [vrm] and [sprite]` for core, `refactor(chat): check every vrm and sprite path the avatar config names` for cli).

---

### Task 2: `set_pose` and the `pose` progress event

**Files:**
- Modify: `crates/sapphire-agent-core/src/lib.rs` (`AvatarCaps`, `KIND_POSE`, `ChatEvent::Pose`, `parse_progress`, module doc, tests)
- Modify: `cli/src/tools/avatar_tools.rs` (whole module: `SET_POSE`, `is_avatar_tool`, `AvatarSink::pose`, `specialize_specs`, `SetPoseTool`, tests; module doc wording "desktop companion")
- Modify: `cli/src/serve/chat_ipc.rs` (`IpcAvatarSink::pose` ~L324–350; tests near L559–640)
- Modify: `cli/src/main.rs:961-964` (register `SetPoseTool`)
- Modify: `cli/src/serve/mod.rs:1977` (comment wording) and the hidden-tools test ~L6294 (register `SetPoseTool` too)

**Interfaces:**
- Consumes: Task 1's core types.
- Produces:
  - core `AvatarCaps { expressions, actions, #[serde(default)] poses: Vec<String> }`
  - core `pub const KIND_POSE: &str = "pose";` and `ChatEvent::Pose { name: String }` (`ChatEvent` stays `#[non_exhaustive]`)
  - cli `pub(crate) const SET_POSE: &str = "set_pose";`, `AvatarSink::pose(&self, name: &str)`, `pub(crate) struct SetPoseTool`
  - wire: `{session_id, kind: "pose", pose}`

Tool spec text (spec §6): description "Switch your avatar's stance (pose). It holds until you change it again, independent of the expression. Call it as often as you like within one turn."; schema `{"pose": {"type":"string","enum":[]}}`, required `["pose"]`; `ToolKind::Think`; no per-turn limit.

- [ ] **Step 1: Write failing tests.**
  - core `progress_kinds_parse_into_events`: add `{"session_id":"s1","kind":"pose","pose":"arms_crossed"}` → `Some(ChatEvent::Pose { name: "arms_crossed".into() })`.
  - avatar_tools: `a_declared_pose_is_sent_and_answers_ok` (sink records `"pose arms"`), `an_unknown_pose_is_refused_with_the_declared_list` (error contains `"normal, arms"`, sink empty), `set_pose_has_no_per_turn_limit` (two calls both `ok`), and extend the `specialize_specs` tests: caps with empty `poses` drops `set_pose`; caps with `poses = ["normal","normal","arms"]` keeps it with enum `["normal","arms"]`; no caps drops all three.
  - chat_ipc: a declared avatar with `poses: ["normal"]` offers `["set_expression", "set_pose"]` (existing helper `avatar_tools_offered`; register `SetPoseTool` in the test registry).
- [ ] **Step 2: Run** `cargo test --workspace pose` — Expected: FAIL to compile.
- [ ] **Step 3: Implement** as in Interfaces; `IpcAvatarSink::pose` notifies `PROGRESS` with `{session_id, kind: KIND_POSE, pose}`; register the tool in `main.rs` next to the other two. Update `RecordingSink` in tests.
- [ ] **Step 4: Run** `cargo test --workspace` — Expected: PASS.
- [ ] **Step 5: Commit** (`feat(rpc): declare poses and parse the pose cue`, `feat(tools): set_pose, for an avatar that holds a stance`).

---

### Task 3: GUI avatar sync and cache (`avatar/assets`)

**Files:**
- Create: `crates/sapphire-agent-gui/src/avatar/mod.rs` (module root; `pub mod assets;` for now)
- Create: `crates/sapphire-agent-gui/src/avatar/assets.rs`
- Modify: `crates/sapphire-agent-gui/src/lib.rs` (`pub mod avatar;`, systems, resources in `finish`)
- Modify: `crates/sapphire-agent-gui/src/chat/bridge.rs` (expose the live client)
- Modify: `crates/sapphire-agent-gui/Cargo.toml` (`sha2 = "0.10"`, `corundum-framework-platform = "0.3"` — the version corundum-framework-gui 0.3 already pulls, `async-trait = "0.1"`, dev `tempfile = "3"`)

**Interfaces:**
- Consumes: core `ChatClient::{avatar_manifest, avatar_download}`, `AvatarManifest`, `AvatarFile`, `AvatarConfig`.
- Produces:
  - `#[async_trait] pub trait AvatarSource: Send + Sync { async fn manifest(&self) -> anyhow::Result<AvatarManifest>; async fn fetch(&self, path: &str) -> anyhow::Result<Vec<u8>>; }` — implemented for `ChatClient`. This is the one swap point spec §7 asks for (#340 adds a bundled source).
  - `pub struct Cache { dir: PathBuf }` with `Cache::new(dir)`, `Cache::default_dir() -> Option<PathBuf>` (`AppDirs::new(APP)?.cache().join("companion-avatar")`), `fn path_of(&self, sha256: &str) -> PathBuf`, `fn has(&self, sha256: &str) -> bool`, `fn put(&self, expected_sha256: &str, bytes: &[u8]) -> anyhow::Result<PathBuf>` (refuses a hash mismatch; writes via temp + rename).
  - `pub fn missing<'a>(manifest: &'a AvatarManifest, cache: &Cache) -> Vec<&'a AvatarFile>` — files the config references whose sha256 the cache lacks (files the config does not reference are not fetched).
  - `pub async fn sync(source: &dyn AvatarSource, cache: &Cache) -> anyhow::Result<LoadedAvatar>`.
  - `#[derive(Clone, Debug, PartialEq)] pub struct LoadedAvatar { pub config: AvatarConfig, pub files: BTreeMap<String, PathBuf> }` — `files` maps each referenced avatar path to its cached file.
  - bevy: `#[derive(Resource, Default)] pub struct AvatarAssets { pub loaded: Option<LoadedAvatar>, pub error: Option<String>, pub generation: u64 }` (generation bumps on every change so renderers reload); `pub struct AvatarSync` (runtime handle + waker + channel, like `RpcBridge`) with `submit(&self, source: Arc<dyn AvatarSource>)`; systems `poll_sync` (every 5 min and on `BridgeEvent::SessionReady`) and `drain_sync`.
  - `RpcBridge::client(&self) -> Option<Arc<ChatClient>>`.

A sync failure keeps `loaded` as it was and sets `error`; a success clears `error`, replaces `loaded` only if it differs, and bumps `generation` then.

- [ ] **Step 1: Write failing tests** in `assets.rs` with a `FakeSource { manifest, files: HashMap<String, Vec<u8>>, fetches: Mutex<Vec<String>> }` and `tempfile::tempdir()`:
  - `sync_fetches_only_referenced_files_missing_from_the_cache`: manifest references `a.png`, `b.png`, lists also `unused.png`; `b.png`'s bytes pre-`put` in the cache → fetches == `["a.png"]`, `loaded.files` has both referenced paths pointing at existing files.
  - `a_download_whose_hash_differs_is_not_cached` (Review Focus 1): source returns bytes whose sha differs from the manifest → `sync` errs mentioning the path, and `cache.has(manifest_sha)` is false.
  - `an_empty_manifest_loads_an_empty_avatar` (Review Focus 4): `{files: []}`, default config → `Ok(LoadedAvatar { config: default, files: empty })`, no fetches.
  - `assets_keep_the_last_good_avatar_when_a_sync_fails`: apply `Ok(x)` then `Err(..)` to `AvatarAssets` via its `apply(result)` method → `loaded == Some(x)`, `error.is_some()`, generation bumped once.
- [ ] **Step 2: Run** `cargo test -p sapphire-agent-gui assets` — Expected: FAIL to compile.
- [ ] **Step 3: Implement** per Interfaces. Wire in `lib.rs`: `AvatarSync` resource built in `finish` (same runtime/waker), `drain_sync` in `Update`, a 300 s repeating `Timer` plus a trigger on `SessionReady` (have `drain_bridge_events` set a `SyncRequested` flag resource rather than coupling modules).
- [ ] **Step 4: Run** gui tests — Expected: PASS.
- [ ] **Step 5: Commit** `feat(gui): sync the avatar's files from the agent into a hash-named cache`.

---

### Task 4: Avatar state, display mode, renderer contract, placeholder

**Files:**
- Create: `crates/sapphire-agent-gui/src/avatar/state.rs`, `mode.rs`, `placeholder.rs`
- Modify: `crates/sapphire-agent-gui/src/avatar/mod.rs` (contract resources, `AvatarPlugin`, `ui_camera` setup)
- Modify: `crates/sapphire-agent-gui/src/chat/bridge.rs` (`BridgeEvent::{Expression, Action, Pose}`; `submit_chat(content, caps: ClientCaps)`)
- Modify: `crates/sapphire-agent-gui/src/chat/state.rs` (avatar tool names don't set `last_status`)
- Modify: `crates/sapphire-agent-gui/src/chat/mod.rs` (`drain_bridge_events` routes cues to `AvatarState`, turn start/end to it)
- Modify: `crates/sapphire-agent-gui/src/lib.rs`, `desktop/src/main.rs` (camera moves into the GUI)

**Interfaces:**
- Consumes: Task 2's `ChatEvent::{Expression, Action, Pose}`, `AvatarCaps`; Task 3's `AvatarAssets`.
- Produces:
  - `state.rs`: `#[derive(Resource, Debug, Clone, PartialEq)] pub struct AvatarState { pub expression: String, pub intensity: f32, pub pose: Option<String>, pending_action: Option<String>, revert_at: Option<Duration> }` (`pose: None` = the renderer's default pose). Methods: `new() -> Self` (`"neutral"`, 1.0), `expression(&mut self, name, intensity)`, `pose(&mut self, name)`, `action(&mut self, name)`, `take_action(&mut self) -> Option<String>` (renderers consume it once), `turn_started(&mut self)` (cancels a pending revert), `turn_finished(&mut self, now: Duration, after: Duration)`, `tick(&mut self, now: Duration)` (at/after `revert_at`: expression `"neutral"`, intensity 1.0, pose `None`, `revert_at` cleared), `reset(&mut self)`. `now` is `Time::elapsed()` in the system.
  - `mode.rs`: `#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)] pub enum DisplayMode { #[default] Auto, Vrm, Sprite }`; `#[derive(Resource, Clone, Copy, Debug, PartialEq, Eq)] pub enum ActiveRenderer { Placeholder, Vrm, Sprite }`; `pub fn choose(mode: DisplayMode, config: Option<&AvatarConfig>) -> ActiveRenderer` implementing the spec §14 table; `pub fn renderer_is(r: ActiveRenderer) -> impl Fn(Res<ActiveRenderer>) -> bool` run condition.
  - `mod.rs` contract resources: `#[derive(Resource, Default)] pub struct DeclaredAvatar(pub Option<AvatarCaps>)`; `#[derive(Resource, Default, Clone, Copy)] pub struct AvatarBounds(pub Option<bevy::math::Rect>)` (logical window pixels, origin top-left, same space as egui points); `#[derive(Resource, Default)] pub struct RendererError(pub Option<String>)`. On `ActiveRenderer` change: `AvatarState::reset`, clear all three. `pub struct AvatarPlugin` adds the systems; `AgentGuiPlugin` adds it.
  - Cameras: the GUI spawns its own UI camera (`Camera2d`, high `order`, `ClearColorConfig::None`) carrying `bevy_egui::PrimaryEguiContext`, with `EguiGlobalSettings::auto_create_primary_context = false`; renderers spawn and despawn their own scene camera at a lower order with `ClearColorConfig::Custom(Color::NONE)`. Remove `spawn_camera` from `desktop/src/main.rs`.
  - `placeholder.rs`: active when `ActiveRenderer::Placeholder`; draws a neutral silhouette (egui painter, `egui::Order::Background` `Area`, a head circle + body rounded rect in `visuals.weak_text_color()` at ~40 % alpha), centered horizontally, bottom-aligned, height 80 % of the window; writes `AvatarBounds` to that rect; `DeclaredAvatar(None)`.
  - Bridge: `submit_chat(&self, content: String, caps: ClientCaps)` passes `Turn { content, attachments: &[], client: &caps }`; `BridgeEvent::{Expression { name, intensity }, Action { name }, Pose { name }}` forwarded from `ChatEvent`.
  - The view passes `ClientCaps { avatar: declared.0.clone() }` (it is `Default` for `None`).

- [ ] **Step 1: Write failing tests.**
  - state: `an_expression_holds_until_revert_after_the_turn` (expression "happy" at t=0; `turn_finished(10s, 30s)`; `tick(39s)` still happy; `tick(40s)` neutral and pose None); `a_new_turn_cancels_the_pending_revert` (`turn_finished(0, 30s)`, `turn_started()`, `tick(100s)` → still happy); `an_action_is_taken_once`; `pose_reverts_with_the_expression` (`pose("arms")`, finish+tick → `pose == None`).
  - mode: `choose_follows_the_spec_table` — Auto+both→Vrm; Auto+sprite only→Sprite; Auto+vrm only→Vrm; Sprite+vrm only→Placeholder; Vrm+both→Vrm; Sprite+both→Sprite; any+`None`→Placeholder; Auto+default config→Placeholder.
  - chat state: `avatar_tool_calls_do_not_show_as_thinking` — `ToolStart { name: "set_pose" }` (and the other two) leave `last_status` `None`.
  - bridge: `submit_chat_without_connection_reports_and_wakes` updated to the new signature.
- [ ] **Step 2: Run** `cargo test -p sapphire-agent-gui` — Expected: FAIL to compile.
- [ ] **Step 3: Implement** per Interfaces. `drain_bridge_events` also calls `state.turn_started()` when a submission is taken (do it where `take_submission` succeeds), `turn_finished(now, REVERT_AFTER)` on `ChatText`/`ChatError` with a `const REVERT_AFTER: Duration = Duration::from_secs(30)` (Task 5 replaces it with `CompanionPrefs::revert_secs`), and forwards cues. `ActiveRenderer` is recomputed each frame from `choose(mode, assets.loaded.as_ref().map(|l| &l.config))` with `DisplayMode::Auto` until Task 5 adds prefs, and only written when it changes.
- [ ] **Step 4: Run** gui tests; then `cargo run -p sapphire-agent-desktop` against a running `serve` built from this branch with no `avatar/` — Expected: the silhouette and chat UI show; a turn offers no avatar tools (check the agent log or `chat.progress`).
- [ ] **Step 5: Commit** `feat(gui): avatar state, display mode and the renderer contract, with a placeholder renderer`.

---

### Task 5: Gestures and the companion overlay UI

**Files:**
- Create: `crates/sapphire-agent-gui/src/companion/mod.rs`, `gesture.rs`, `bubble.rs`, `prefs.rs`, `view.rs`
- Delete: `crates/sapphire-agent-gui/src/chat/view.rs` (its pieces move into `companion/view.rs`; the Settings toggle / banner logic in `settings.rs::Layout` is reused or adapted)
- Modify: `crates/sapphire-agent-gui/src/lib.rs`, `src/settings.rs`, `Cargo.toml` (`serde = { version = "1", features = ["derive"] }`, `toml = "1"`)

**Interfaces:**
- Consumes: `AvatarBounds`, `AvatarState`, `DeclaredAvatar`, `ChatState`, `ChatBridge`, `GuiSettings`, `DisplayMode`.
- Produces:
  - `gesture.rs` (pure): `pub struct PointerSample { pub pos: Vec2, pub over_avatar: bool, pub over_ui: bool }`; `pub enum Press { Primary, Secondary }`; `pub enum Gesture { OpenInput, OpenHistory, OpenMenu, StartDrag, CloseAll }`; `pub struct GestureTracker` with `press(&mut self, Press, PointerSample)`, `moved(&mut self, PointerSample) -> Option<Gesture>` (primary held on the avatar and moved > `DRAG_PX = 4.0` → `StartDrag`, once), `release(&mut self, Press, PointerSample) -> Option<Gesture>` (primary pressed+released on the avatar without dragging → `OpenInput`; secondary on the avatar → `OpenMenu`), `scroll(&mut self, PointerSample, delta: f32) -> Option<Gesture>` (on the avatar, |delta| > 0 → `OpenHistory`), `escape(&mut self) -> Gesture` (`CloseAll`). Presses with `over_ui` are ignored (egui owns them).
  - `pub fn wants_pointer(cursor: Option<Vec2>, bounds: Option<Rect>, egui_wants: bool) -> bool` — true iff egui wants the pointer or the cursor is inside `bounds`; `None` cursor → false.
  - `bubble.rs` (pure): `pub fn bubble_duration(text: &str) -> Duration` = clamp(3 s + 60 ms × chars, 3 s, 20 s); `pub struct Bubble { pub text: String, pub until: Duration }`, `pub enum BubbleContent { Thinking, Reply(String) }`.
  - `prefs.rs`: `#[derive(Resource, Serialize, Deserialize, Clone, Debug, PartialEq)] pub struct CompanionPrefs { pub display_mode: DisplayMode, pub revert_secs: u32, pub show_buttons: bool, pub window: Option<WindowPlacement> }` with defaults `Auto`, `30`, `false`, `None`; `pub struct WindowPlacement { pub x: i32, pub y: i32, pub width: u32, pub height: u32 }`; `load(path) -> Self` (missing or unreadable → default, logged), `save(&self, path) -> anyhow::Result<()>` (temp + rename), `default_path()` = `AppDirs::new(APP)?.config().join("companion.toml")`; `revert_secs` clamped to 5..=600 on load. Saved whenever it changes (compare with last saved).
  - `#[derive(Resource, Default)] pub struct Overlay { pub input: bool, pub history: bool, pub menu: bool }` and `#[derive(Message)] pub struct CompanionRequest` enum `{ StartDrag, Quit }` — the presenter (Task 6) handles `StartDrag`; the GUI handles `Quit` by `AppExit`.
  - `#[derive(Resource, Clone, Copy, Default)] pub struct PointerWanted(pub bool)` — written each frame from `wants_pointer`, read by the desktop presenter.

View (spec §5): bubble anchored above `AvatarBounds` (or top-center if none) showing `Thinking` while a turn is `Sending` and the latest reply for `bubble_duration`; input box near the character's lower half when `Overlay.input` (focus on open, Enter sends, Esc closes); history panel (semi-transparent, over the character, scroll, closes on Esc or click outside) when `Overlay.history`; menu (`egui::Popup`/`Area` at the click) with Settings, Reconnect (only when failed), Quit; with `show_buttons`, three buttons (input, history, menu) beside the character. Settings opens as an opaque full-window panel with a close button (reuse `Layout`). The status banner shows above the bubble when `Health::Problem`. With `ActiveRenderer::Placeholder` and no turn in flight, the bubble shows "No character is set up yet" with a "Character settings" button that opens Settings on the Character page (Task 7). Clicking the silhouette still opens the input — ruling against spec §8's "click opens the Character page", because without buttons that would leave no way to chat before a character exists. Keyboard: Esc closes; Ctrl+Enter (or Enter on an empty overlay) opens input.

- [ ] **Step 1: Write failing tests.**
  - gesture: `a_click_on_the_avatar_opens_input`; `a_click_off_the_avatar_does_nothing`; `dragging_starts_once_past_four_pixels_and_suppresses_the_click`; `secondary_click_opens_the_menu`; `scroll_on_the_avatar_opens_history`; `presses_over_egui_are_ignored`.
  - `wants_pointer_is_true_on_the_avatar_or_egui_only` (Review Focus 2): inside bounds → true; outside + egui false → false; outside + egui true → true; cursor `None` → false; bounds `None` + egui false → false.
  - bubble: `duration_grows_with_text_and_is_clamped` (`""` → 3 s, 50 chars → 6 s, 1000 chars → 20 s; count `chars()`, so 50 Japanese chars → 6 s too).
  - prefs: `prefs_round_trip_and_clamp` (save/load equal; a file with `revert_secs = 1` loads as 5; garbage file → default).
  - overlay: `a_reply_while_typing_keeps_the_draft_and_the_input_open` (Review Focus 3) — pure helper `on_reply(&mut Overlay, &mut ChatState, text)` or the existing `ChatState::apply` + overlay rule: after `ChatText` with `draft = "abc"` and `input = true`, draft is `"abc"` and `input` still true.
- [ ] **Step 2: Run** `cargo test -p sapphire-agent-gui` — Expected: FAIL to compile.
- [ ] **Step 3: Implement** the pure modules, then the view and systems: gesture input from bevy (`ButtonInput<MouseButton>`, `MouseWheel` messages, the primary window's `cursor_position()`, `ctx.egui_wants_pointer_input()`/`is_pointer_over_egui()`), `PointerWanted`, prefs load in `finish` / save on change, and replace the Task 4 revert constant and `DisplayMode::Auto` with `CompanionPrefs`.
- [ ] **Step 4: Run** tests, then the desktop (still a normal window until Task 6) — Expected: click on the silhouette opens input; Enter sends; bubble shows "thinking" then the reply; scroll opens history; right-click opens the menu; Settings opens and closes; Esc closes overlays.
- [ ] **Step 5: Commit** `feat(gui): the companion overlay — gestures, bubble, input, history and menu`.

---

### Task 6: Desktop companion window (presenter)

**Files:**
- Create: `desktop/src/companion_window.rs`
- Modify: `desktop/src/main.rs`, `desktop/Cargo.toml` (`device_query` for the global cursor; comments say "companion", not "mate")
- Modify: `crates/sapphire-agent-gui/src/lib.rs` (let the host pick the update mode: `AgentGuiPlugin { app, update_mode: Option<WinitSettings> }` or a field with a default; spec deferred note)

**Interfaces:**
- Consumes: `PointerWanted`, `CompanionRequest::StartDrag`, `CompanionPrefs.window`.
- Produces: `pub struct CompanionWindowPlugin;` and pure `pub fn place_on_screen(saved: Option<WindowPlacement>, monitors: &[MonitorRect], default_size: UVec2) -> WindowPlacement` where `MonitorRect { x, y, width, height }`.

Window (spec §4): `transparent: true`, `decorations: false`, `window_level: WindowLevel::AlwaysOnTop`, `skip_taskbar: true`, `composite_alpha_mode: CompositeAlphaMode::PostMultiplied` where supported (else `Auto`), `ClearColor(Color::NONE)`; default size 420×640 logical, placed at the bottom-right of the primary monitor with a 32 px margin when nothing is saved. Title stays `sapphire-agent-desktop`.

Click-through: each frame set `window.cursor_options.hit_test = PointerWanted || dragging || overlay open`. While `hit_test` is off winit reports no cursor, so poll the global cursor with `device_query::DeviceState::get_mouse()`, convert to window-logical coordinates with the window position and scale factor, and feed that position into the gesture/`PointerWanted` computation instead of `cursor_position()` (expose a `CursorOverride(Option<Vec2>)` resource in the GUI the presenter writes). Polling needs frames: use `UpdateMode::reactive_low_power(Duration::from_millis(50))` for both focused and unfocused. On a Wayland session (`WAYLAND_DISPLAY` set and winit chose Wayland — check `raw_window_handle` / bevy's `WindowWrapper` or simply `std::env::var_os("WAYLAND_DISPLAY").is_some() && std::env::var_os("DISPLAY").is_none()`), keep `hit_test = true` always and log once that click-through is unavailable (Review Focus 2).

Drag: on `CompanionRequest::StartDrag` call `window.start_drag_move()`. Position save: when the window's position settles (no change for 500 ms) write `CompanionPrefs.window`.

- [ ] **Step 1: Write failing tests** for `place_on_screen` (Review Focus 5): saved inside monitor 2 of two → kept; saved on a monitor that no longer exists → moved onto the primary (first) monitor at its bottom-right with the 32 px margin; saved size larger than the monitor → clamped to fit; no monitors → saved or default as is.
- [ ] **Step 2: Run** `cargo test -p sapphire-agent-desktop` — Expected: FAIL to compile.
- [ ] **Step 3: Implement** the plugin and wire it in `main.rs`.
- [ ] **Step 4: Run** tests; then manual on this host (X11): the window is transparent and borderless, stays on top, does not show in the taskbar; clicks outside the silhouette reach the app below; moving back over the silhouette takes clicks again; drag moves the window; restart restores the position.
- [ ] **Step 5: Commit** `feat(desktop): the transparent, click-through companion window`.

---

### Task 7: "Character" settings page frame

**Files:**
- Create: `crates/sapphire-agent-gui/src/companion/character_page.rs`
- Modify: `crates/sapphire-agent-gui/src/settings.rs` (`GuiSettings::spawn` adds the page via `Settings::with_app_sections("sapphire-agent", vec![..])`), `src/lib.rs`, `src/avatar/assets.rs` (an upload/set-config path)

**Interfaces:**
- Consumes: `CompanionPrefs`, `AvatarAssets`, `ActiveRenderer`, `RendererError`, `AvatarSync`, `RpcBridge::client`.
- Produces:
  - `pub trait RendererPanel: Send { fn title(&self) -> &str; fn ui(&mut self, ui: &mut egui::Ui, cx: &mut PanelContext<'_>); }` — what plans 4-2D / 4-3D implement for their section.
  - `pub struct PanelContext<'a> { pub config: Option<&'a AvatarConfig>, pub files: Option<&'a BTreeMap<String, PathBuf>>, pub ops: &'a AvatarOps, pub error: Option<&'a str> }`.
  - `#[derive(Clone)] pub struct AvatarOps` with `upload(&self, path: String, bytes: Vec<u8>)` and `set_config(&self, config: AvatarConfig)` — both run on the GUI runtime through the live client, report into a shared `OpsStatus { busy: bool, last_error: Option<String> }`, and request a sync on success. `set_config` runs `config.check()` first and reports its error without calling the agent.
  - `#[derive(Resource, Default)] pub struct RendererPanels(Vec<Box<dyn RendererPanel>>)` — renderer plugins push theirs in `build`; `AgentGuiPlugin::finish` moves them into the page.
  - `pub(crate) struct CharacterPage` implementing `SettingsSection` (label "Character" / i18n not required now), holding `Arc<Mutex<CharacterModel>>` that a bevy system refreshes each frame from the resources (config, files, error, active renderer, prefs) and reads back (prefs edits) — `Settings` owns the page as a plain trait object, so state crosses through the mutex.

Page layout: display mode (radio: Auto / 3D / 2D, with "(in use)" next to the active one), revert seconds (slider 5–600), "Show buttons" checkbox, the renderer error if any, `OpsStatus`, then each `RendererPanel` under its title. With no panels registered (this plan) show "No renderer sections yet".

- [ ] **Step 1: Write failing tests:** `set_config_with_a_structural_error_reports_without_calling_the_agent` (`AvatarOps::new(runtime_handle, client: Arc<dyn Fn() -> Option<Arc<ChatClient>> + Send + Sync>, sync_request)` with a client lookup that panics if called; `set_config` of a sprite config with bad `default_pose` → `status().last_error` contains the pose name, `busy` false); `prefs_edited_on_the_page_reach_the_resource` (pure `CharacterModel::take_prefs_edit` returns `Some(new)` once after an edit, then `None`).
- [ ] **Step 2: Run** gui tests — Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** tests; manual: Settings shows a "Character" page under the app heading; changing the display mode to 2D with no `[sprite]` shows the placeholder; revert seconds and Show buttons persist across restart.
- [ ] **Step 5: Commit** `feat(gui): the Character settings page and the renderer panel slot`.

---

## After this plan

Per spec §14, once this merges: write plan 4-2D and plan 4-3D on this host, then open the 3D handoff issue (read: spec, plan 4-3D, plans 1–3 rulings; scope: `avatar/vrm` + its `RendererPanel` only; bevy_vrm1 0.11 from crates.io, fork `sapphire-turtle/bevy_vrm1` kept; human checks; subagent-driven execution).
