# Desktop Companion 4-3D: VRM Renderer Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

> **Runs on a GPU host.** The host that wrote this plan has only a virtual GPU. Read the handoff issue (linked from the PR that merged plan 4-0) before starting.

**Goal:** Draw the companion as a VRM 1.0 model with bevy_vrm1. It switches expressions, blinks, follows the cursor with its eyes, loops an idle VRMA and plays action VRMAs. The plan also adds the "3D" section of the Character page, where users upload the model and its gestures.

**Architecture:** Everything lives in `crates/sapphire-agent-gui/src/avatar/vrm/`. Plan 4-0 created the module, added `bevy_vrm1 = "0.11"`, `fastrand = "2"` and the bevy features, and wired `VrmRendererPlugin` into `AvatarPlugin`. Its `build` already adds `VrmPlugin` and `VrmaPlugin`, guarded on `bevy::gltf::GltfPlugin` being in the app (they panic under `MinimalPlugins`); `AvatarPlugin` also inits `CursorOverride`.

- Pure logic: `cues.rs` (what to declare), `face.rs` (expression blending and blink timing), `frame.rs` (camera framing and the bounds projection) and `glb.rs` (which VRM version a file is).
- `scene.rs` loads the VRM through the `companion-avatar://` asset source, owns a `Camera3d` and a light, and drives bevy_vrm1.
- `panel.rs` is the Character page section.

The renderer follows the contract in `avatar/mod.rs` and touches no other module (spec §14).

**Tech Stack:** Rust 2024, bevy 0.20 (`bevy_pbr`, `bevy_gltf`, `bevy_animation`), bevy_vrm1 0.11 (`VrmPlugin`, `VrmaPlugin`, `VrmHandle`, `VrmaHandle`, `SetExpressions`, `PlayVrma`, `LookAt`, `LoadedVrma`), egui 0.37 via the bevy_egui fork.

**Spec:** `docs/superpowers/specs/2026-10-28-desktop-companion-design.md` §6 (cues), §7 (Character page), §8 (behaviour), §14 (contract), error table. Prerequisites: plan 4-0 merged. Read:
- the renderer contract (`avatar/mod.rs` module docs)
- `AvatarState` (`avatar/state.rs`, including `pending_action` / `take_action`)
- `LoadedAvatar::asset_path` (`avatar/assets.rs`)
- `RendererPanel` / `PanelContext` / `AvatarOps::apply` / `FilePicker` (`companion/`)
- bevy_vrm1 0.11's examples: `expressions.rs`, `vrma.rs`, `look_at_target.rs`, `look_at_cursor.rs`, `simple.rs`

## Global Constraints

- **Scope:** touch only `crates/sapphire-agent-gui/src/avatar/vrm/**`. If shared code must change, stop and land it as its own PR to main first, then continue on top of it. The 2D plan runs in parallel elsewhere (spec §14).
- **Scope exceptions**, the only ones:
  - Add ONE line under `desktop/CHANGELOG.md`'s `[Unreleased]` describing the VRM renderer, in its own `docs(desktop)` commit after the last task. The GUI crate makes no desktop release on its own (CLAUDE.md). Plan 4-2D adds its own line there too; the trivial merge conflict between the two is accepted.
  - The README licence-table row for Task 2's VRM fixture, if its licence needs one (see Task 2).
- **Expressions declared:** the VRM 1.0 emotion presets the model actually has, from `neutral`, `happy`, `angry`, `sad`, `relaxed`, `surprised`. Custom expressions, `blink*`, visemes and `look*` are never declared (spec §6).
- **Actions declared:** the `[vrm.actions]` names whose VRMA loaded. Poses: none. 3D still poses are out of scope (spec §6, §13).
- **Expression behaviour:** blend to the target in about **0.2 s** with `intensity` (spec §8). `AvatarState` already reverts to `neutral` after the turn, using the companion's revert setting.
- **Blink:** a random interval of **2–6 s**, drawn with `fastrand` (no other rng crate). Respect the expression's `overrideBlink`, which bevy_vrm1 applies (spec §8).
- **Look-at:**
  - On the desktop, follow the cursor. The window may be click-through, so take the cursor from the GUI's `CursorOverride` (`At(pos)`, `Outside`, `None`). Fall back to `Window::cursor_position()` only for `None`.
  - Use `LookAt::Target` with an entity you move, not `LookAt::Cursor`: that reads only the window's own cursor.
  - On `Outside`, look toward where the cursor is in the global position only if the GUI provides it. Otherwise look forward (spec §8).
- **Spring bones:** left to bevy_vrm1 (spec §8).
- **Idle and actions:** loop `idle` if set, else stand still. Play an action once, then return to idle. An action arriving mid-action is ignored (spec §8).
- **Frame pacing:** an idle loop, spring bones and blinks need frames. While the VRM is shown, request frames at a capped rate of **30 fps**, using a `GuiWaker` timer on the GUI runtime and not `RequestRedraw` every frame. While nothing moves, request none. Measure idle CPU and report it (contract, "Frame pacing").
- **Camera:** a `Camera3d` at `order: 0` with `ClearColorConfig::Custom(Color::NONE)`, plus one `DirectionalLight` and soft ambient light. Frame the model to fit the window height with a small margin, using the model's bounding box after it initialises.
- **Commits:** `(gui)` scope (the changelog line: `docs(desktop)`), each ending with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.
- **Verification:** `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test -p sapphire-agent-gui`, and the manual checks on the GPU host.

## Review Focus

1. **A VRM 0.x or broken file.** bevy_vrm1 reads only VRM 1.0, but it loads a VRM 0.x as plain glTF: the asset is `Loaded`, the VRM never gets `Initialized`, and `load_state == Failed` never fires. So check the cached file's version before spawning, and treat a model that loaded but did not initialise in time as a failure too. Set `RendererError` with a message saying VRM 1.0 is required. The placeholder stands in, and the Character page shows the error. Test in Task 2.
2. **A model missing some presets.** Only the presets it has are declared, so the agent can't pick a face the model lacks. Test in Task 1.
3. **The agent sends an action while one is playing, or one whose VRMA failed to load.** The action is ignored, with no panic and no stuck pose. Test in Task 3.
4. **A sync replaces the model mid-animation.** Despawn the old VRM, its VRMAs and its observers before the new one spawns, so no orphaned entities keep animating. Test in Task 2.
5. **An idle companion's CPU and GPU use.** With nothing animating it renders no frames; with the idle loop it renders at most 30 fps. Measure in Task 4.

---

### Task 1: Pure logic: cues, face blending, blink timing, framing

**Files:**
- Create: `crates/sapphire-agent-gui/src/avatar/vrm/cues.rs`, `face.rs`, `frame.rs`, `glb.rs`
- Modify: `crates/sapphire-agent-gui/src/avatar/vrm/mod.rs`

**Interfaces:**
- **`cues.rs`:** `pub(crate) fn declared(model_presets: &[&str], actions_loaded: &[&str]) -> AvatarCaps`
  - The model's presets intersected with the 6 emotion presets, in the order `neutral, happy, angry, sad, relaxed, surprised`.
  - Actions sorted; poses empty.
  - `neutral` is always included, even if the model has no explicit `neutral`, because it means "all off".
- **`face.rs`:**
  - `pub(crate) struct Face { weights: BTreeMap<String, f32> }` with `pub(crate) fn step(&mut self, target: &str, intensity: f32, dt: Duration) -> bool`.
  - `step` moves each emotion weight linearly toward its target over `BLEND = Duration::from_millis(200)`: the target expression goes to `intensity`, the others to 0, and `neutral` means all 0. It returns whether anything is still moving.
  - `pub(crate) fn weights(&self) -> impl Iterator<Item = (&str, f32)>`.
- **`face.rs`:** `pub(crate) struct Blink` with `pub(crate) fn next_interval(rng: &mut impl FnMut() -> f32) -> Duration` (uniform between 2 s and 6 s) and `pub(crate) fn weight_at(since_start: Duration) -> f32`. The closing-and-opening curve lasts 0.15 s, 0→1→0, linear.
- **`frame.rs`:**
  - `pub(crate) fn camera_for(model_aabb: Aabb3d, aspect: f32, fov_y: f32, margin: f32) -> Transform`: a camera on +Z looking at the model's centre, at a distance where the height plus margin fills the view.
  - `pub(crate) fn screen_rect(corners: [Vec3; 8], project: impl Fn(Vec3) -> Option<Vec2>) -> Option<Rect>`: the bounding rect of the projected corners in logical px, or `None` if any corner is behind the camera.
- **`glb.rs`:** `pub(crate) fn vrm_version(bytes: &[u8]) -> Result<VrmVersion, String>` reads the GLB JSON chunk's `extensionsUsed`. Parse it with the `gltf` crate bevy_gltf re-exports, `bevy::gltf::gltf` (`gltf::Glb::from_slice`, then `gltf::json::Root::from_slice` on its `json`), not `serde_json`. Task 2 checks the model with it before spawning; Task 5's panel before uploading.

- [ ] **Step 1:** Write the failing tests:
  - `declared_keeps_only_presets_the_model_has` (Review Focus 2): presets `["happy","blink","aa","Joy"]` and actions `["wave","peace"]` give expressions `["neutral","happy"]` and actions `["peace","wave"]`.
  - `the_face_reaches_the_target_in_200ms`: steps of 50 ms ×4 bring `happy` to `0.8` with intensity 0.8, and the return value goes false after that.
  - `switching_fades_the_old_out`: after `happy`, stepping toward `sad` brings happy to 0 and sad to 1 in 200 ms.
  - `blink_intervals_stay_in_2_to_6_s`: with rng returning 0.0 → 2 s and 1.0 → 6 s.
  - `blink_curve_closes_and_opens`: 0 ms → 0, 75 ms → 1, 150 ms → 0.
  - `camera_frames_the_model_height`: an AABB of height 1.6 m with fov 30° and margin 0.1 m puts the camera at `(1.7/2)/tan(15°)` ± 1e-3 from the centre.
  - `screen_rect_spans_the_projected_corners`.
  - `vrm_version_reads_the_extension`: a hand-built minimal GLB (its JSON a valid glTF root, `"asset":{"version":"2.0"}` included) with `VRMC_vrm` gives V1; with `VRM` it gives `Err` mentioning "VRM 0.x"; a non-GLB gives `Err`.
- [ ] **Step 2:** Run `cargo test -p sapphire-agent-gui vrm::` and confirm it fails.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run the tests and confirm they pass.
- [ ] **Step 5:** Commit `feat(gui): the VRM renderer's cues, face blending, blink timing, framing and version check`.

### Task 2: Loading the VRM and its VRMAs; the scene

**Files:**
- Create: `crates/sapphire-agent-gui/src/avatar/vrm/scene.rs`
- Modify: `crates/sapphire-agent-gui/src/avatar/vrm/mod.rs`:
  - `build` already adds `VrmPlugin` and `VrmaPlugin` (plan 4-0, guarded on `GltfPlugin`); keep the guard
  - systems are gated by `renderer_is(ActiveRenderer::Vrm)` and run `.after(AvatarSystems::Choose)`
  - add the ungated teardown

**Interfaces:**
- **Consumes:** `AvatarAssets { loaded, generation }`, `LoadedAvatar::asset_path`, `DeclaredAvatar`, `RendererError`, Task 1's `declared`, `camera_for` and `vrm_version`.
- **Resource and markers:** `#[derive(Resource, Default)] pub(crate) struct VrmScene { generation: Option<u64>, root: Option<Entity>, actions: BTreeMap<String, Entity>, idle: Option<Entity>, ready: bool }`, and the markers `VrmCamera`, `VrmLight`.
- **On a generation change:**
  - Despawn the old root recursively, with its VRMA children and observers (Review Focus 4).
  - Check the cached model with `vrm_version` before spawning: read its bytes from `LoadedAvatar::files` (a check, not a load: the model itself still loads through the asset server), off the frame on bevy's `IoTaskPool`. On `Err`, set `RendererError` to `"cannot load {model}: {cause} (VRM 1.0 is required)"`, declare nothing and spawn nothing (Review Focus 1).
  - Spawn `VrmHandle(asset_server.load(asset_path(model)))` with `LookAt::Target(target_entity)`.
  - Spawn a child `VrmaHandle` per action and for idle.
  - Record each `LoadedVrma` via an observer.
- **Once the VRM initialises:**
  - Find the bevy_vrm1 event or component that marks this; check its source rather than guessing.
  - Read its expression names.
  - Set `DeclaredAvatar(declared(..))` from the loaded VRMAs, clear `RendererError`, frame the camera, set `ready`.
- **On a load failure** of the model (`AssetServer::load_state` = `Failed`): set `RendererError` to `"cannot load {model}: {cause} (VRM 1.0 is required)"`, and declare nothing (Review Focus 1). A failed VRMA is left out of the actions and logged.
- **Loaded but never initialised:** bevy_vrm1 loads a VRM 0.x (or any plain glTF) as glTF without `Initialized`. If the model is `Loaded` and not `Initialized` within `INIT_TIMEOUT = Duration::from_secs(5)` of real time, it is a failure as above, with the cause "not a VRM 1.0 model" (Review Focus 1).
- **Teardown:** `fn despawn_vrm_scene` without the renderer gate, `run_if(resource_changed::<ActiveRenderer>)`. It despawns the root, camera, light and look target when VRM is no longer active. It also resets `VrmScene` to its default, `generation: None` included, so Vrm → Placeholder → Vrm with the same generation reloads.
- **Frames while loading:** request frames until ready, failed or timed out, then stop. Loads finish off-frame.

- [ ] **Step 1:** Write the failing tests. Use a headless App that follows bevy_vrm1's own `tests::test_app` pattern (`MinimalPlugins`, `AssetPlugin`, `ImagePlugin`, `WindowPlugin`) and register the companion asset source as the 2D plan's load tests do. Use a small VRM 1.0 sample under `crates/sapphire-agent-gui/src/avatar/vrm/testdata/` with a permissive licence: VRoid's sample or bevy_vrm1's test assets, if their licence allows redistribution. bevy_vrm1's crates.io package excludes `assets/`, so take them from its GitHub repository and check the licence there, not only the crate's `MIT OR Apache-2.0`. Record the licence in a file next to the fixture. If it is CC0 or MIT, that note is enough; otherwise also add a row to the README licence table, the allowed scope exception above.
  - `a_vrm_loads_and_declares_its_presets`.
  - `a_non_vrm_file_is_a_renderer_error` (Review Focus 1): the model path points at a PNG.
  - `a_vrm_0x_file_is_a_renderer_error` (Review Focus 1): the model is a minimal GLB declaring `VRM`. `RendererError` names VRM 1.0, and no VRM root is spawned.
  - `a_gltf_that_never_initialises_is_a_renderer_error` (Review Focus 1): a plain glTF that passes the version check (or the timeout decision as a pure function, if a headless load is impossible), after `INIT_TIMEOUT`.
  - `a_new_generation_replaces_the_scene` (Review Focus 4): after two generations, exactly one VRM root exists.
  - `switching_away_despawns_the_scene`: then switching back with the same generation loads the scene again.
  - If a headless VRM load proves impossible without a GPU, do this instead: cover the error and teardown paths headless, cover loading in the manual check, and say so in the report.
- [ ] **Step 2:** Run the tests and confirm they fail.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run the tests. They pass.
- [ ] **Step 5:** Commit `feat(gui): load the VRM avatar and its gestures into a scene of its own`.

### Task 3: Expressions, blink, look-at, idle and actions; bounds

**Files:**
- Create: `crates/sapphire-agent-gui/src/avatar/vrm/animate.rs`
- Modify: `crates/sapphire-agent-gui/src/avatar/vrm/mod.rs`

**Interfaces:**
- **Consumes:** `AvatarState` (`Res`; `pending_action()` to peek, `take_action()` only when starting playback), `CursorOverride`, `VrmScene`, Task 1's `Face`, `Blink` and `screen_rect`, bevy_vrm1's `SetExpressions` and `PlayVrma`.
- **Expressions:** each frame while the face moves, `Face::step(state.expression, state.intensity, dt)`. Push the weights with `SetExpressions` (or `ModifyExpressions`, whichever applies weights additively for emotions in 0.11; check the source).
- **Blink:** driven by `Blink`, with `fastrand::f32` as its rng, applied as the `blink` expression. bevy_vrm1 blocks or blends it per the active expression's `overrideBlink`.
- **Look-at:** move the look target entity to the cursor ray's point at the model's head depth: `At(pos)` → there; `Outside`/`None` → straight ahead at head height.
- **Idle:** `PlayVrma { repeat: Forever }` on the idle VRMA once ready.
- **Actions:**
  - When `pending_action()` names a loaded action and none is playing, take it and play it once with a 0.2 s transition, then return to idle with a 0.2 s transition when it ends.
  - Detect the end through bevy_vrm1's event, or the animation player's finished state; check the source.
  - An unknown action, or one arriving while another plays, is taken and dropped, so it does not linger (Review Focus 3).
- **Bounds:** `fn write_bounds` in `AvatarSystems::Bounds` sets `AvatarBounds(screen_rect(model AABB corners, camera.world_to_viewport))`, compare-then-write.

- [ ] **Step 1:** Write the failing tests. Run them headless where Task 2 managed a headless VRM; otherwise make them pure over a small adapter trait, `trait VrmDriver { fn set_weights(..); fn play(..); fn playing(&self) -> bool }`, with a fake.
  - `a_second_action_while_playing_is_dropped` (Review Focus 3).
  - `an_unloaded_action_is_dropped`.
  - `the_face_follows_state_changes_only_when_they_happen`: no `set_weights` calls once the blend settles.
- [ ] **Step 2:** Run the tests and confirm they fail.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run the tests, then the manual GPU-host checks:
  - The model shows, framed.
  - `set_expression` from a chat turn blends, and reverts after the revert time.
  - The eyes follow the cursor, including while the window is click-through.
  - Idle loops.
  - `play_action` plays once and returns to idle.
  - Blink varies.
  - Clicking the model opens the input box, which shows the bounds are right.
- [ ] **Step 5:** Commit `feat(gui): animate the VRM avatar — expressions, blink, gaze, idle and gestures`.

### Task 4: Frame pacing

**Files:**
- Modify: `crates/sapphire-agent-gui/src/avatar/vrm/animate.rs` (or a new `pace.rs` in `avatar/vrm/`)

**Interfaces:**
- **Consumes:** `GuiWaker`, `GuiRuntime`.
- **Produces:** `fn pace(..)`. While anything animates (an idle loop, an action, a face blend, a blink, a look target moving, or spring bones settling after a move), it keeps a single pending 33 ms `GuiWaker` timer: at most one in flight, which gives 30 fps. When nothing animates, it schedules none.
  - **Spring bones:** treat them as animating for 1 s after any of the above stops.
  - **Pure decision:** `pub(crate) fn wants_frame(animating: bool, since_last_motion: Duration) -> bool`, testable alone.

- [ ] **Step 1:** Write the failing test `pace_stops_one_second_after_motion` (pure).
- [ ] **Step 2:** Run it and confirm it fails.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Measure on the GPU host (Review Focus 5) and record the numbers:
  - idle loop on: fps, CPU %, GPU %
  - no idle VRMA and the face settled: frames per minute ≈ 0
- [ ] **Step 5:** Commit `feat(gui): pace the VRM avatar's frames to its motion`.

### Task 5: The 3D section of the Character page

**Files:**
- Create: `crates/sapphire-agent-gui/src/avatar/vrm/panel.rs`
- Modify: `crates/sapphire-agent-gui/src/avatar/vrm/mod.rs` (`app.add_renderer_panel(Box::new(VrmPanel::default()))`)

**Interfaces:**
- **Consumes:** `RendererPanel`, `PanelContext { config, files, ops, file_picker }`, `AvatarOps::apply`, `FilePicker`, Task 1's `vrm_version`.
- **Produces:** `pub(crate) struct VrmPanel`, with title `"3D"` and the source name `"3D"`.
- **UI** (spec §7):
  - "Choose model…": `.vrm` uploads to `vrm/model.vrm` and sets `vrm.model`. Before uploading, check it with `vrm_version`. Refuse VRM 0.x (`VRM` extension) with a clear message.
  - "Idle…": `.vrma` uploads to `vrm/idle.vrma` and sets `vrm.idle`. Add a "None" option.
  - **Actions:** a list of name → file. Add a name plus a `.vrma` file (uploaded to `vrm/actions/<name>.vrma`), rename, and delete.
  - Every change goes through one `apply` that edits only `config.vrm` and leaves `sprite` untouched. The edit closure returns `Result<(), String>` (#356): return `Err` when the change no longer fits the agent's current config (e.g. renaming an action that another device removed), so the op reports it.
- **Pure helpers:**
  - `fn with_model(..)`
  - `fn with_idle(..)`
  - `fn add_action(config, name, path) -> Result<(), String>` refuses empty or duplicate names.
  - `fn rename_action(..)`
  - `fn remove_action(..)`

- [ ] **Step 1:** Write the failing tests:
  - `with_model_creates_the_vrm_section_and_keeps_sprite`
  - `action_names_must_be_unique_and_non_empty`
  - `rename_and_remove_actions`
- [ ] **Step 2:** Run the tests and confirm they fail.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run the tests. Then on the GPU host, set up a model, an idle and two actions from the panel alone. Confirm the companion shows them after the sync, and that the agent's next turn offers the two actions.
- [ ] **Step 5:** Commit `feat(gui): the 3D section of the Character page`.
