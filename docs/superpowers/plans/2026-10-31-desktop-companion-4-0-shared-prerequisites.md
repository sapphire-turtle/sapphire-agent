# Desktop Companion 4-0: Shared Prerequisites Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Put every shared-code change that plans 4-2D (sprite) and 4-3D (VRM) both need into main before either starts, so the two can then run in parallel on different hosts touching only `avatar/sprite*` and `avatar/vrm*` (spec §14).

**Architecture:** Two small tasks. The first adds the dependencies and bevy features both renderers need, plus empty `SpriteRendererPlugin` / `VrmRendererPlugin` modules already wired into `AvatarPlugin`. Each parallel plan then only fills in its own module. The second adds a host-provided file picker, so a renderer panel can ask the user for a file without `cfg(target_os)` in the GUI crate.

**Tech Stack:** Rust 2024, bevy 0.20, bevy_vrm1 0.11 (crates.io), rfd 0.17 (desktop only).

**Spec:** `docs/superpowers/specs/2026-10-28-desktop-companion-design.md` (§3, §7 GUI side, §13, §14). Builds on plan 3 (`docs/superpowers/plans/2026-10-30-desktop-companion-3-foundation.md`, PR #347): read the renderer contract in `crates/sapphire-agent-gui/src/avatar/mod.rs` and the panel contract in `crates/sapphire-agent-gui/src/companion/character_page.rs`.

## Global Constraints

- Nothing in `crates/sapphire-agent-gui` is `cfg(target_os)`; platform code lives in `desktop/` (spec §3).
- Code identifiers and comments say `companion`, never "mate".
- Commit scopes: `(gui)` for the GUI crate, `(desktop)` for desktop/; each commit ends with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.
- The desktop binary must keep starting without libX11 or GTK installed at link time (CLAUDE.md, #182): any new Linux dependency is dlopen'd or optional.
- Verification: `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`, `cargo deny check licenses bans sources`.

## Review Focus

1. **The user cancels the file dialog, or picks a file that cannot be read.** The pick resolves to `None` or an error string, and nothing is uploaded. Test in Task 2.
2. **Two panels ask for a file at once.** Each gets its own result, matched by ticket. Test in Task 2.
3. **Linux without a GTK/portal backend.** The desktop still starts; picking a file reports that no picker is available, and nothing panics. Verify in Task 2 with `ldd`, and by reading rfd's backend selection.
4. **The empty renderer plugins are registered, but no renderer is chosen.** No systems run and nothing changes for the user. The existing tests stay green (Task 1).
5. **bevy_vrm1 pulls in bevy_pbr/gltf/animation.** The headless edition must not gain them: only the GUI crate depends on bevy_vrm1. Check with `cargo tree -p sapphire-agent-headless | grep -c bevy` (Task 1).

---

### Task 1: Renderer dependencies and empty renderer modules

**Files:**
- Modify: `crates/sapphire-agent-gui/Cargo.toml`:
  - add `bevy_vrm1 = "0.11"`
  - add the bevy features `bevy_sprite`, `bevy_image`, `png`, `webp`, `bevy_pbr`, `bevy_gltf`, `bevy_animation`
  - keep `default-features = false`
  - add `fastrand = "2"`, for 4-3D's blink timing
- Create: `crates/sapphire-agent-gui/src/avatar/sprite/mod.rs` (`pub struct SpriteRendererPlugin;` whose `build` does nothing yet, plus a module doc stating that plan 4-2D owns this directory)
- Create: `crates/sapphire-agent-gui/src/avatar/vrm/mod.rs` (`pub struct VrmRendererPlugin;`, the same, owned by plan 4-3D; its `build` adds `bevy_vrm1::prelude::{VrmPlugin, VrmaPlugin}` only when `app.is_plugin_added::<bevy::gltf::GltfPlugin>()`: they panic under `MinimalPlugins`, which the GUI's headless tests use. Test `avatar::vrm::tests::bevy_vrm1_is_skipped_without_gltf`)
- Modify: `crates/sapphire-agent-gui/src/avatar/mod.rs`:
  - `pub mod sprite; pub mod vrm;`
  - `AvatarPlugin::build` adds both plugins, and inits `CursorOverride` (as `CompanionPlugin` does) for renderers in an app without the companion
- Modify: `desktop/Cargo.toml` bevy features, only if the desktop's own feature list must match. Cargo unifies features, so prefer leaving it.

**Interfaces:**
- Produces: `avatar::sprite::SpriteRendererPlugin` and `avatar::vrm::VrmRendererPlugin`, both added by `AvatarPlugin`. Plans 4-2D and 4-3D fill in only these modules, and their own panels registered from them.

- [ ] **Step 1:** Write the test `avatar::tests::both_renderer_plugins_are_installed`. Build a minimal App the way the existing `the_avatar_plugin_runs_without_the_companion_plugin` test does, run `app.update()`, and assert `app.is_plugin_added::<SpriteRendererPlugin>()` and `app.is_plugin_added::<VrmRendererPlugin>()`.
- [ ] **Step 2:** Run `cargo test -p sapphire-agent-gui both_renderer_plugins` and confirm it fails to compile.
- [ ] **Step 3:** Implement as listed under Files.
- [ ] **Step 4:** Run the gui tests, the full verification list, and `cargo tree -p sapphire-agent-headless -e normal | grep -E "bevy_vrm1|bevy_pbr"`. That last command must print nothing (Review Focus 5). Then run a 10 s X11 smoke launch of the desktop. Expected: no panic, and the companion looks as before.
- [ ] **Step 5:** Commit `feat(gui): the renderer dependencies and empty sprite and vrm renderer modules`.

### Task 2: Host-provided file picker for renderer panels

**Files:**
- Create: `crates/sapphire-agent-gui/src/companion/file_pick.rs`
- Modify: `crates/sapphire-agent-gui/src/companion/character_page.rs` (`PanelContext` gains `pub file_picker: &'a FilePicker`)
- Modify: `crates/sapphire-agent-gui/src/companion/mod.rs` and `src/lib.rs` (resource wiring)
- Create: `desktop/src/file_dialog.rs`
- Modify: `desktop/Cargo.toml`:
  - `rfd = { version = "0.17", default-features = false, features = ["xdg-portal"] }`: the portal backend talks D-Bus and links neither GTK nor X11; `gtk3` would link GTK
- Modify: `desktop/src/main.rs` (install it on the window path)

**Interfaces:**
- Produces, in the GUI crate:
  - `pub struct FileFilter { pub name: &'static str, pub extensions: &'static [&'static str] }`
  - `pub struct PickedFile { pub name: String, pub bytes: Vec<u8> }`
  - `pub type PickOutcome = Result<Option<PickedFile>, String>;` and `pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;` (the crate has no `futures`)
  - `pub trait FileDialog: Send + Sync { fn pick(&self, title: String, filters: Vec<FileFilter>) -> BoxFuture<PickOutcome>; }`
    - `Ok(None)` means the user cancelled.
  - `#[derive(Resource, Clone)] pub struct FilePicker`, holding `Option<Arc<dyn FileDialog>>`, the runtime `Handle` and the `Waker`. Methods:
    - `pick(&self, title: impl Into<String>, filters: Vec<FileFilter>) -> PickTicket` runs the dialog on the GUI runtime and wakes the GUI when done.
    - `result(&self, ticket: &PickTicket) -> Option<Result<Option<PickedFile>, String>>` takes the outcome once. It is `None` while the dialog is open.
    - `available(&self) -> bool`.
    - With no dialog installed, `pick` resolves at once to `Err("no file dialog on this platform")`.
  - `pub trait FilePickerApp { fn set_file_dialog(&mut self, dialog: Arc<dyn FileDialog>) -> &mut Self; }` on `App`, safe in any plugin order.
  - `PanelContext::file_picker: &'a FilePicker`
- Produces, in desktop: `file_dialog::RfdDialog` implementing `FileDialog` with `rfd::AsyncFileDialog`. It reads the picked file's bytes, rejects anything over `sapphire_agent_core::AVATAR_MAX_FILE` with an error naming the limit, and returns `name` as the file name without its directory.

- [ ] **Step 1:** Write tests in `file_pick.rs` with a fake dialog:
  - `a_pick_resolves_once_by_ticket`: the fake returns `Ok(Some(PickedFile{name:"a.png",bytes:vec![1]}))`. After the runtime runs, `result(&t)` returns it once, then `None`.
  - `cancel_is_ok_none` (Review Focus 1).
  - `two_picks_keep_their_own_results` (Review Focus 2).
  - `no_dialog_reports_unavailable`: `available()` is false, and the result is `Err` containing "no file dialog".
- [ ] **Step 2:** Run them and confirm they fail.
- [ ] **Step 3:** Implement the GUI side, then the desktop `RfdDialog`. Run the dialog future on the GUI runtime. rfd's async dialog on Linux/portal needs no main thread; on macOS and Windows rfd requires the dialog to be created on the main thread for the sync API only. Use the async API, and say in the report what rfd's docs require.
- [ ] **Step 4:** Run the verification list, then check:
  - `ldd target/debug/sapphire-agent-desktop | grep -iE "gtk|x11"` prints nothing (Review Focus 3).
  - Manual X11 check: temporarily wire a debug key or button to `pick` and observe the dialog open, or report that the portal is missing on this host. Do not commit the debug wiring.
- [ ] **Step 5:** Commit `feat(gui): a host-provided file picker for renderer panels` and `feat(desktop): pick avatar files with the system dialog`.

## After this plan

Open its PR and merge it before plans 4-2D and 4-3D start. Then open the 3D handoff issue that points at plan 4-3D.
