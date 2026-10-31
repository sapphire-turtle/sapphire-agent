# Desktop Companion 4-2D: Sprite Renderer Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Draw the companion from user-supplied 2D standing pictures: the pose's base image, with the expression overlay laid on it and crossfaded. The plan also adds the "2D" section of the Character page, where users set up poses and expression images.

**Architecture:** Everything lives in `crates/sapphire-agent-gui/src/avatar/sprite/`. Plan 4-0 created the module and wired `SpriteRendererPlugin` into `AvatarPlugin`.
- Pure logic (`resolve.rs`) decides which images show and what to declare.
- `load.rs` loads the images through the `companion-avatar://` asset source and checks the canvas.
- `draw.rs` owns a `Camera2d` and the sprites, and writes `AvatarBounds`.
- `panel.rs` is the Character page section.

The renderer follows the contract in `avatar/mod.rs` and touches no other module (spec §14).

**Tech Stack:** Rust 2024, bevy 0.20 (`bevy_sprite`, `bevy_image` with png/webp), egui 0.37 via the bevy_egui fork.

**Spec:** `docs/superpowers/specs/2026-10-28-desktop-companion-design.md` §13 (2D), §8 (behaviour), §7 (Character page), §14 (contract), error table. Prerequisite: plan 4-0 merged. Read the renderer contract (`avatar/mod.rs` module docs), `AvatarState` (`avatar/state.rs`), `LoadedAvatar::asset_path` (`avatar/assets.rs`), `RendererPanel` / `PanelContext` / `AvatarOps::apply` / `FilePicker` (`companion/`).

## Global Constraints

- Touch only `crates/sapphire-agent-gui/src/avatar/sprite/**`. If shared code must change, stop and land that change as its own PR first (spec §14).
- One exception to that scope: add ONE line under `desktop/CHANGELOG.md`'s `[Unreleased]` describing the 2D renderer, in its own `docs(desktop)` commit after the last task. The GUI crate makes no desktop release on its own (CLAUDE.md). Plan 4-3D adds its own line there too; the trivial merge conflict between the two is accepted.
- Expression keys: `happy`, `angry`, `sad`, `relaxed` and `surprised` only (`sapphire_agent_core::SPRITE_EXPRESSIONS`). `neutral` is the pose's `base` (spec §13).
- All images of one `[sprite]` must share one pixel size, and are drawn at the same origin. A mismatch is a load error shown on the settings page, with the placeholder standing in (spec §13).
- Formats: PNG and WebP.
- Expression and pose transitions are crossfades of about **0.2 s**. `intensity` is ignored. There is no blink or mouth movement (spec §8, §13).
- Display size: fit the window height, keep the aspect ratio, centred horizontally and aligned to the bottom (spec §13). Bounds are the image rectangle.
- Declare (spec §13):
  - `poses`: every pose whose base loaded.
  - `expressions`: `neutral`, plus every expression that has a loaded image in at least one pose.
  - `actions`: none.
- Frame pacing: request frames only while a crossfade runs. An idle companion draws no frames (contract, "Frame pacing").
- Commits use the `(gui)` scope (the changelog line above: `docs(desktop)`) and end with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.
- Manual checks that need a running agent use a `serve` built from this branch only if it can run without disturbing the user's own agent (they share the IPC endpoint). Otherwise list the check for the human and say so.
- Verification: `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test -p sapphire-agent-gui`.

## Review Focus

1. **The agent asks for a pose this device doesn't have.** This happens after the pose was deleted on another device, between declare and sync. The default pose is shown and nothing panics. Test in Task 1.
2. **One expression image fails to load, or has the wrong size.** That expression is left out of the declaration and the rest still work. Only a failing default-pose base makes the renderer error out. Test in Task 2.
3. **A new sync arrives mid-crossfade, or while images are still loading.** The renderer reloads from the new `generation` without showing a mix of the old and new avatar, and without leaking entities. Test in Task 3.
4. **The window is resized.** The sprite and its bounds follow the new window height in the same frame, so clicks still land on the character. Test in Task 3.
5. **Uploading an image with the wrong canvas size from the panel.** The panel refuses it before upload, naming both sizes, so the user doesn't break a working avatar. Test in Task 4.

---

### Task 1: What to show and what to declare (pure)

**Files:**
- Create: `crates/sapphire-agent-gui/src/avatar/sprite/resolve.rs`
- Modify: `crates/sapphire-agent-gui/src/avatar/sprite/mod.rs` (`mod resolve;`)

**Interfaces:**
- Produces:
  - `pub(crate) struct Shown<'a> { pub pose: &'a str, pub base: &'a str, pub overlay: Option<&'a str> }`
  - `pub(crate) fn shown<'a>(config: &'a SpriteConfig, loaded: &impl Fn(&str) -> bool, pose: Option<&str>, expression: &str) -> Option<Shown<'a>>`
    - The pose is the requested one if it exists and its base loaded, otherwise `default_pose` if its base loaded, otherwise `None`.
    - The overlay is set when `expression != "neutral"`, the pose has an image for it, and that image loaded.
    - Paths are avatar paths: `base` and `overlay` are config values.
  - `pub(crate) fn declared(config: &SpriteConfig, loaded: &impl Fn(&str) -> bool) -> Option<AvatarCaps>`
    - `None` when the default pose's base did not load.
    - Otherwise poses sorted, expressions `neutral` first and then the loaded keys sorted, actions empty.
  - `pub(crate) fn same_canvas(sizes: &[(String, UVec2)]) -> Result<UVec2, String>`
    - The first size wins. The error names the first mismatching path and both sizes, as `"{path} is {w}×{h}, the others are {W}×{H}"`.

- [ ] **Step 1:** Write the failing tests.
  - Build a config with poses `normal` (base `n.png`, happy `nh.png`) and `arms` (base `a.png`, sad `as.png`), default `normal`. `loaded` returns true for all files except where a test says otherwise.
  - `shown_uses_the_pose_and_its_overlay`: `(Some("arms"), "sad")` gives `Shown{pose:"arms", base:"a.png", overlay:Some("as.png")}`.
  - `an_unknown_pose_falls_back_to_the_default` (Review Focus 1): `(Some("gone"), "happy")` gives `normal`, `n.png`, `Some("nh.png")`.
  - `an_expression_the_pose_lacks_shows_the_base`: `(Some("arms"), "happy")` gives overlay `None`.
  - `neutral_has_no_overlay`.
  - `an_unloaded_overlay_is_dropped`: `nh.png` not loaded, so `(None, "happy")` gives overlay `None`.
  - `no_default_base_shows_nothing`: `n.png` not loaded, so `(None, "neutral")` gives `None`, and `declared` gives `None`.
  - `declared_lists_loaded_poses_and_expressions` (Review Focus 2): `as.png` not loaded gives poses `["arms","normal"]`, expressions `["neutral","happy"]`, actions `[]`.
  - `same_canvas_reports_the_first_mismatch`: `[("a",64×128),("b",64×128),("c",32×32)]` gives `Err` containing `c is 32×32` and `64×128`. A single entry is `Ok`.
- [ ] **Step 2:** Run `cargo test -p sapphire-agent-gui sprite::resolve` and confirm it fails to compile.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run the tests again. They pass.
- [ ] **Step 5:** Commit `feat(gui): decide which sprite shows and what the 2D avatar declares`.

### Task 2: Loading the images

**Files:**
- Create: `crates/sapphire-agent-gui/src/avatar/sprite/load.rs`
- Modify: `crates/sapphire-agent-gui/src/avatar/sprite/mod.rs` (systems in `Update`, `.after(AvatarSystems::Choose)`, `run_if(renderer_is(ActiveRenderer::Sprite))`)

**Interfaces:**
- Consumes:
  - `AvatarAssets { loaded: Option<LoadedAvatar>, generation }`
  - `LoadedAvatar::asset_path(path) -> Option<AssetPath<'static>>`
  - `DeclaredAvatar`, `RendererError`
  - Task 1's `declared` and `same_canvas`
- Produces:
  - `#[derive(Resource, Default)] pub(crate) struct SpriteImages { pub generation: Option<u64>, pub handles: BTreeMap<String, Handle<Image>>, pub failed: BTreeSet<String>, pub canvas: Option<UVec2>, pub ready: bool }`, keyed by avatar path.
  - `fn load_sprites(...)`: when `AvatarAssets.generation` differs from `SpriteImages.generation`, drop the old handles and start loading every path of the `[sprite]` section.
  - `fn settle_sprites(...)`: once every handle is loaded or failed:
    - Images whose size differs from the default pose's base are marked failed.
    - If the default base failed, set `RendererError` and clear `DeclaredAvatar`. The message names the file and the cause, e.g. a size mismatch from `same_canvas`.
    - Otherwise clear `RendererError`, set `DeclaredAvatar(declared(..))` and `ready = true`.
    - Every write is compare-then-write.

- [ ] **Step 1:** Write the failing tests. Use a minimal headless App: `MinimalPlugins`, `AssetPlugin`, `ImagePlugin`, the cache asset source registered with `avatar::register_asset_source_at`-style test setup (see the test `a_synced_file_loads_through_the_asset_server` in `avatar/mod.rs`), and `SpriteRendererPlugin` with `ActiveRenderer::Sprite`. Put tiny PNGs, e.g. 2×4 and 3×3, into the cache. Add no `image` dev-dependency: commit the PNGs under `crates/sapphire-agent-gui/src/avatar/sprite/testdata/` and `include_bytes!` them, or encode them in the test from bevy's `Image` (`try_into_dynamic`, then write with `ImageFormat::Png.as_image_crate_format()`).
  - `a_complete_sprite_avatar_declares_its_cues`: after a few updates, `DeclaredAvatar` has the expected poses and expressions, and `RendererError` is `None`.
  - `a_wrong_sized_overlay_is_left_out` (Review Focus 2): the happy overlay is 3×3, so the declaration has no `happy`, and `RendererError` is `None`.
  - `a_missing_default_base_is_a_renderer_error`: the default base file is absent from the cache, so `RendererError` is `Some(..)` naming it, and `DeclaredAvatar` is `None`.
  - `a_new_generation_reloads` (Review Focus 3): bump the generation with a config whose pose set differs, and the declaration follows it.
- [ ] **Step 2:** Run the tests and confirm they fail.
- [ ] **Step 3:** Implement. While loading, request frames: `RequestRedraw` per frame until `ready` or error. Loads complete off-frame, so without that the app may sleep.
- [ ] **Step 4:** Run the tests again. They pass.
- [ ] **Step 5:** Commit `feat(gui): load the 2D avatar's images and check their canvas`.

### Task 3: Drawing, crossfade and bounds

**Files:**
- Create: `crates/sapphire-agent-gui/src/avatar/sprite/draw.rs`
- Modify: `crates/sapphire-agent-gui/src/avatar/sprite/mod.rs`

**Interfaces:**
- Consumes: `SpriteImages`, `AvatarState` (`Res`; react to `is_changed()`), `ActiveRenderer`, `AvatarSystems::{Choose, Bounds}`, `UI_CAMERA_ORDER`.
- Produces:
  - `#[derive(Component)] struct SpriteCamera` (`Camera2d`, `order: 0`, `ClearColorConfig::Custom(Color::NONE)`), spawned when the sprite renderer becomes active and ready.
  - `fn despawn_sprite_scene` without the renderer gate, `run_if(resource_changed::<ActiveRenderer>)`. It despawns the camera and all sprite entities when Sprite is no longer active, as in the contract's example. It also resets `SpriteImages` to its default, `generation: None` included, so Sprite → Placeholder → Sprite with the same generation reloads.
  - Two layers, `Current` and `Previous`, each a base sprite and an optional overlay sprite. When `shown(..)` changes, the old current becomes previous, and a new current fades in over `FADE = Duration::from_millis(200)` as previous fades out. Use `Sprite::color` alpha, with previous drawn below current. After the fade, previous is despawned.
  - Pure `pub(crate) fn fit(canvas: UVec2, window: Vec2) -> Rect`: scale = `window.y / canvas.y`, width = `canvas.x * scale`, centred horizontally, bottom at `window.y`. Logical px, top-left origin.
  - `fn write_bounds` in `AvatarSystems::Bounds`: `AvatarBounds(Some(fit(..)))` while ready, compare-then-write, and also on window resize (Review Focus 4).
  - Place the sprites in world space so that the `Camera2d` default projection shows them exactly at `fit(..)`. Mind the y-up world against the top-left logical rect.

- [ ] **Step 1:** Write the failing tests.
  - `fit_fills_the_height_and_centres` (pure): canvas 400×800, window 420×640, gives a rect of x 110..310, y 0..640.
  - `fit_follows_a_resize`: window 420×320 gives the rect 160..260 × 0..320.
  - `a_change_crossfades_and_cleans_up` (Review Focus 3): headless App, as in Task 2. After an expression change, both layers exist for under 0.2 s, and only one remains after. Advance `Time<Real>`, or drive the fade by an injected clock, and say which in the report.
  - `switching_away_despawns_the_scene`: set `ActiveRenderer::Placeholder`, so no `SpriteCamera` and no sprite entities remain. Switch back to `ActiveRenderer::Sprite` with the same generation, and the avatar loads and declares again.
- [ ] **Step 2:** Run the tests and confirm they fail.
- [ ] **Step 3:** Implement. Request redraws only while a fade runs.
- [ ] **Step 4:** Run the tests. Then run the desktop on this X11 host with a real `serve` built from this branch, and a 2-pose test sprite set uploaded through Task 4's panel or placed in `<workspace>/avatar/` by hand. Check that the character shows, that `set_expression` and `set_pose` from a chat turn change it with a fade, that it reverts after 30 s, that a window resize follows, and that idle CPU stays near 0 with the cursor away. Record the results.
- [ ] **Step 5:** Commit `feat(gui): draw the 2D avatar with crossfades and report its bounds`.

### Task 4: The 2D section of the Character page

**Files:**
- Create: `crates/sapphire-agent-gui/src/avatar/sprite/panel.rs`
- Modify: `crates/sapphire-agent-gui/src/avatar/sprite/mod.rs` (`app.add_renderer_panel(Box::new(SpritePanel::default()))` in `build`)

**Interfaces:**
- Consumes: `RendererPanel`, `PanelContext { config, files, ops, file_picker }`, `AvatarOps::apply(source, uploads, edit) -> OpTicket`, `FilePicker::pick / result`, `SPRITE_EXPRESSIONS`, Task 1's `same_canvas`.
- Produces: `pub(crate) struct SpritePanel` with title `"2D"`. Its source name for `apply` is also `"2D"`.

UI (spec §7):
- A list of poses, each row showing its name, its base, and the 5 expression slots.
- Add, rename and delete a pose. Deleting the default pose is refused until another pose is the default.
- Pick the default pose.
- "Set image…" per base or expression slot opens `file_picker.pick` with the filter PNG/WebP. On result, it reads the image size from the bytes (PNG/WebP header or bevy `Image::from_buffer`) and compares it with the size of the current default base. It refuses a mismatch with `same_canvas`'s message (Review Focus 5); the first base sets the canvas.
- Upload path: `sprite/<pose>/<slot>.<ext>`, where slot is `base` or the expression key.
- One `apply` uploads the file and edits only `config.sprite`, creating `[sprite]` with this pose as the default when it is absent. It leaves `vrm` untouched.
- Rename and delete edit the config only. Unreferenced files stay; cleanup is out of scope.

Pure helpers, testable without egui:
- `pub(crate) fn with_image(config: &mut AvatarConfig, pose: &str, slot: Slot, path: String)`
- `pub(crate) fn rename_pose(config: &mut AvatarConfig, from: &str, to: &str) -> Result<(), String>`, which refuses a name that exists or is empty and follows `default_pose`.
- `pub(crate) fn remove_pose(config: &mut AvatarConfig, pose: &str) -> Result<(), String>`, which refuses the default pose.
- `enum Slot { Base, Expression(&'static str) }`

- [ ] **Step 1:** Write the failing tests.
  - `with_image_creates_the_sprite_section_and_keeps_vrm`: a config with only `vrm`, then `with_image(.., "normal", Base, "sprite/normal/base.png")`, gives `sprite.default_pose == "normal"` with `vrm` unchanged.
  - `rename_follows_the_default`.
  - `rename_refuses_a_taken_name`.
  - `removing_the_default_pose_is_refused`.
  - `a_wrong_sized_image_is_refused_before_upload` (Review Focus 5): use the pure size check the panel calls.
- [ ] **Step 2:** Run the tests and confirm they fail.
- [ ] **Step 3:** Implement the helpers, then the panel UI around them.
- [ ] **Step 4:** Run the tests. Then do the manual check against a branch `serve`: build a 2-pose avatar from the panel alone, and confirm the companion shows it after the sync.
- [ ] **Step 5:** Commit `feat(gui): the 2D section of the Character page`.
