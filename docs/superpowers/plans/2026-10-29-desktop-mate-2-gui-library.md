# Desktop Mate Plan 2: GUI library, forks and Settings — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Move the desktop's chat into a new `crates/sapphire-agent-gui` library on egui 0.37 (a bevy_egui fork), show sapphire-framework-gui 0.23's `Settings` in it, and (originally) port bevy_vrm1 to bevy 0.20 for plan 3 — superseded: upstream bevy_vrm1 0.11.0 targets bevy 0.20, so plan 3 uses it from crates.io. Throughout, the desktop keeps behaving as it does today.

**Architecture:** `sapphire-agent-gui` owns everything a future `mobile/` shares:
- `app`: one tokio runtime, a waker that rouses bevy's event loop from other threads, fonts and theme.
- `chat`: a pure state reducer, the IPC bridge and the egui view.
- `settings`: `FrameworkClient` plus `Settings`.

`desktop/` shrinks to its window, camera and `AgentGuiPlugin`. The whole workspace moves to sapphire-framework 0.23, because framework-gui 0.23 requires it, so only one copy of the framework is linked.

**Tech Stack:**
- Rust 2024
- bevy 0.20
- bevy_egui: fork of 0.43.1 on egui 0.37
- sapphire-framework(-gui) 0.23
- corundum-framework-gui 0.3
- bevy_vrm1: upstream 0.11 from crates.io (the fork in Task 5 is superseded; it is kept only for future bevy bumps that run ahead of upstream)

**Spec:** `docs/superpowers/specs/2026-10-28-desktop-companion-design.md` (formerly `desktop-mate-design.md`). This plan covers work-order steps 1–3 and §1, §2, §3 and §11.

## Global Constraints

- Forks go under the `sapphire-turtle` GitHub org. The user authorised this ("ガンガンフォークしてgit参照して構わない").
- Each fork gets a named branch: `egui-0.37` for bevy_egui and `bevy-0.20` for bevy_vrm1.
- Reference the forks with `git = "https://github.com/sapphire-turtle/<repo>", branch = "<branch>"` and commit `Cargo.lock`.
- `sapphire-agent-gui` uses `publish = false`, with `release = false` in `release-plz.toml` (no tag, no release).
- Add `gui` to the `cliff.toml` skip alternation in the same commit as the first `(gui)` commit.
- Commit scopes:
  - `(gui)` for `crates/sapphire-agent-gui`
  - `(desktop)` for `desktop/`
  - `(deps)` for the framework bump
- Do not put `cfg(target_os)` in `sapphire-agent-gui`. Window concerns (size, title, camera, and later transparency) stay in `desktop/`.
- Do not use framework-gui's `Shell`, `app::run` or `eframe` feature. Use `Settings` on its own, with `default-features = false`, so neither eframe nor winit-from-eframe is linked.
- The desktop's visible behaviour must stay as it is through Tasks 1–3. Task 4 adds only the Settings screen.
- These checks must pass at the end of every task:
  - `cargo test --workspace`
  - `cargo clippy --workspace --all-targets -- -D warnings`
  - `cargo fmt --all --check`
- The app identity the GUI talks to the framework with is `AppIdentity { app_name: "sapphire-agent", version: env!("CARGO_PKG_VERSION") }`, using the desktop's version.

## Review Focus

1. **The agent is not running at startup.** Both the chat bar ("not connected" plus Reconnect) and Settings (its own status banner) must say so. Nothing may panic. Starting the agent later must let Reconnect and Settings recover. Pinned by Task 3's `submit_chat_without_connection_reports_and_wakes` test and a Task 4 manual check.
2. **A reply arrives while the user is not touching the window.** With bevy in reactive mode, it must still appear without moving the mouse. Pinned by Task 3's waker tests.
3. **Two copies of sapphire-framework get linked**, from a mismatch between core's 0.22 and gui's 0.23. Pinned by the `cargo tree -d` check in Tasks 1 and 4.
4. **The theme or language changes on another device while Settings is closed.** It must apply at once, so `poll` and `follow_theme` run every frame and not only while Settings is shown. Pinned by Task 4's system ordering and a manual check.
5. **The window is closed mid-turn.** The process must exit promptly, without hanging on the runtime. The runtime is dropped with `shutdown_background`. Manual check in Task 3.

---

### Task 1: sapphire-framework 0.23 across the workspace

**Files:**
- Modify: `cli/Cargo.toml:120,126`
- Modify: `crates/sapphire-agent-core/Cargo.toml:12`
- Modify: `Cargo.lock`

**Interfaces:**
- Produces: the workspace on `sapphire-framework` 0.23.0 and `sapphire-framework-bridge-client` 0.23.0. `server-api` stays at 5.0.0 and `bridge-api` at 3.0.0, the same versions 0.23 uses.

- [ ] **Step 1:** Change both `0.22.0` requirements to `0.23.0` and run `cargo update -p sapphire-framework -p sapphire-framework-bridge-client`.
- [ ] **Step 2:** Run `cargo tree --workspace -d -e normal | grep -E '^sapphire-framework|^grain-id'`. Expected: no output, meaning no duplicates.
- [ ] **Step 3:** Run the three Global checks. Expected: all pass, with test counts no lower than before (cli 942, core 5). Fix any compile errors from 0.23's API at the call site, without changing behaviour.
- [ ] **Step 4: Commit** `chore(deps): sapphire-framework 0.23.0`

### Task 2: bevy_egui fork on egui 0.37; desktop moves to it

**Files:**
- Outside this repo: fork `vladbat00/bevy_egui` to `sapphire-turtle/bevy_egui` and clone it to `../forks/bevy_egui`, next to the agent's worktrees.
- Modify: `desktop/Cargo.toml`, the bevy_egui and egui_extras entries
- Modify: `desktop/src/**/*.rs`, only where egui 0.37 breaks it
- Modify: `Cargo.lock`

**Interfaces:**
- Produces: `bevy_egui = { git = "https://github.com/sapphire-turtle/bevy_egui", branch = "egui-0.37" }` and `egui_extras = { version = "0.37", features = ["svg"] }`.
- Later tasks get egui through `bevy_egui::egui`.

- [ ] **Step 1:** Fork and clone:
  - `gh repo fork vladbat00/bevy_egui --org sapphire-turtle --clone=false`
  - clone it, then `git checkout -b egui-0.37 v0.43.1`
- [ ] **Step 2:** In the fork, raise `egui` to `0.37`. Use the same diff as upstream's open PR #508, plus whatever code fixes egui 0.37 needs. Run `cargo test` and `cargo clippy --all-targets`. Expected: both pass.
- [ ] **Step 3:** Commit `Update egui to 0.37` in the fork and push the branch. Put one line at the top of the fork's README saying why the fork exists and when to drop it ("until upstream releases egui 0.37").
- [ ] **Step 4:** Point `desktop/Cargo.toml` at the fork and egui_extras 0.37, and update the comment that names "egui 0.36.x". Run `cargo build -p sapphire-agent-desktop` and fix any egui 0.37 API breaks in `desktop/src`.
- [ ] **Step 5:** Run `cargo tree -p sapphire-agent-desktop -i egui`. Expected: one `egui v0.37.x`.
- [ ] **Step 6:** Manual check: `cargo run -p sapphire-agent-desktop` with the agent running. The window paints, Japanese text renders (not tofu), the send icon shows, and a turn round-trips. Record the result in the report.
- [ ] **Step 7: Commit** `chore(desktop): take egui 0.37 through a bevy_egui fork`

### Task 3: `crates/sapphire-agent-gui`, with the chat moved in

**Files:**
- Create: `crates/sapphire-agent-gui/Cargo.toml`
  - `publish = false`
  - `license-file = "LICENSE"`
  - copy `desktop/LICENSE` and `desktop/LICENSE-App-Store-Exception`
- Create: `crates/sapphire-agent-gui/src/lib.rs`, holding `AgentGuiPlugin`
- Create: `crates/sapphire-agent-gui/src/app.rs`: runtime, `Waker`, fonts and image loaders
- Create: `crates/sapphire-agent-gui/src/chat/mod.rs`
- Create: `crates/sapphire-agent-gui/src/chat/state.rs`, from `desktop/src/state.rs` plus `ChatState` and its reducer
- Create: `crates/sapphire-agent-gui/src/chat/bridge.rs`, from `desktop/src/bridge.rs`
- Create: `crates/sapphire-agent-gui/src/chat/view.rs`, from `desktop/src/ui/chat.rs` and `root_ui`
- Move: `desktop/assets/{NotoSansJP-Regular.otf,LICENSE-OFL.txt,icons/}` → `crates/sapphire-agent-gui/assets/`
- Modify: `desktop/src/main.rs`, which becomes logging, window, camera and the plugin
- Delete: `desktop/src/{bridge.rs,state.rs,ui/}`
- Modify:
  - `desktop/Cargo.toml`: depends on `sapphire-agent-gui`, and drops tokio, egui_extras and sapphire-agent-core if they become unused
  - `Cargo.toml`: add the member
  - `release-plz.toml`: a `[[package]]` with `name = "sapphire-agent-gui"`, `release = false`
  - `cliff.toml:57`: add `gui` to the alternation
  - `CLAUDE.md`: add rows to the scope and layout tables

**Interfaces:**
- Produces, used by Task 4 and plan 3:
  - `pub struct AgentGuiPlugin { pub app: AppIdentity }`. `AppIdentity` is re-exported from `sapphire_framework_gui::client`. Task 3 already depends on sapphire-framework-gui with `default-features = false`, only for this type.
  - `AgentGuiPlugin` adds `EguiPlugin` and every gui system. The caller adds `DefaultPlugins`, its window and its camera.
  - `pub struct GuiRuntime(pub Arc<tokio::runtime::Runtime>)`, a `Resource`. On `AppExit` the runtime is shut down with `shutdown_background`, never with a blocking drop.
  - `pub type Waker = Arc<dyn Fn() + Send + Sync>`, plus `pub struct GuiWaker(pub Waker)`, a `Resource`.
    - The plugin builds it from bevy_winit's `EventLoopProxyWrapper`, sending `WinitUserEvent::WakeUp`, or bevy 0.20's equivalent.
    - It sets `WinitSettings::desktop_app()`, so the window idles when nothing happens.
  - `chat::ChatState`: the `Resource` that today is `AppState`, with the same fields.
    - `fn new() -> ChatState` starts as `Session::Connecting` with empty history.
    - `fn apply(&mut self, event: BridgeEvent)` is the body of today's `drain_bridge_events`, unchanged.
    - `fn take_submission(&mut self) -> Option<String>` returns `None` unless the session is `Ready`, the turn is `Idle` and the trimmed draft is non-empty. Otherwise it takes the draft, pushes the `User` entry, sets `Sending`, clears the status and returns the text.
  - `chat::RpcBridge::new(runtime: tokio::runtime::Handle, waker: Waker) -> (RpcBridge, UnboundedReceiver<BridgeEvent>)`. Every event it sends is followed by one `waker()` call. `submit_connect` and `submit_chat` keep their behaviour.

- [ ] **Step 1: Write the failing tests** in `chat/state.rs` and `chat/bridge.rs`:

```rust
#[test] fn reply_appends_assistant_and_idles() {
    let mut s = ChatState::new();
    s.turn = TurnState::Sending; s.last_status = Some("tool: x…".into());
    s.apply(BridgeEvent::ChatText { text: "hi".into() });
    assert_eq!(s.history.last().unwrap().role, ChatRole::Assistant);
    assert_eq!(s.history.last().unwrap().text, "hi");
    assert_eq!(s.turn, TurnState::Idle); assert!(s.last_status.is_none());
}
#[test] fn error_appends_system_entry() { /* ChatError{message:"boom"} → System, text "error: boom", Idle */ }
#[test] fn tool_events_set_status() { /* ToolStart{name:"search"} → "tool: search…"; ToolEnd → "tool: search done" */ }
#[test] fn session_events_set_session() { /* SessionReady{display_id:"abc"} → Ready{"abc"}; SessionFailed{message:"m"} → Failed{"m"} */ }
#[test] fn take_submission_requires_ready_idle_and_text() {
    let mut s = ChatState::new(); s.draft = "hello".into();
    assert_eq!(s.take_submission(), None);                 // Connecting
    s.session = Session::Ready { display_id: "a".into() };
    s.draft = "   ".into(); assert_eq!(s.take_submission(), None);
    s.draft = "hello".into();
    assert_eq!(s.take_submission().as_deref(), Some("hello"));
    assert_eq!(s.turn, TurnState::Sending); assert!(s.draft.is_empty());
    assert_eq!(s.history.last().unwrap().role, ChatRole::User);
    s.draft = "again".into(); assert_eq!(s.take_submission(), None); // Sending
}
#[test] fn submit_chat_without_connection_reports_and_wakes() {
    // runtime built in the test; waker counts calls in an AtomicUsize
    // bridge.submit_chat("x") → rx yields ChatError{message:"not connected"}, wakes == 1
}
```

- [ ] **Step 2:** Run `cargo test -p sapphire-agent-gui`. Expected: FAIL, because the crate and types don't exist yet. Scaffold the crate first so the failure is a missing item, not a missing crate.
- [ ] **Step 3:** Move the code and implement the Interfaces above.
  - Keep the existing comments that still hold, and fix the ones that name the old file layout.
  - Fonts and image loaders keep their one-shot `EguiPrimaryContextPass` systems with `Local<bool>`.
  - The view calls `state.take_submission()` and then `bridge.submit_chat(text)`.
  - The reconnect button keeps `state.session = Connecting; bridge.submit_connect()`.
  - The plugin calls `submit_connect()` once at startup.
- [ ] **Step 4:** Run `cargo test -p sapphire-agent-gui` and then the three Global checks. Expected: all pass.
- [ ] **Step 5:** Manual check with `cargo run -p sapphire-agent-desktop`. Record each result in the report:
  - same window and behaviour as after Task 2
  - a reply shows up while the mouse is outside the window, which proves the waker works
  - closing the window mid-turn exits in under 1 s
  - starting with the agent stopped shows "not connected", and after starting it, Reconnect works
- [ ] **Step 6: Commit**, two commits:
  - `feat(gui): sapphire-agent-gui, the GUI shared by desktop and mobile, with the chat` (crate, cliff, release-plz, CLAUDE.md)
  - `refactor(desktop): run on AgentGuiPlugin`
  - If the move can't compile in two steps, make one `(gui)` commit.

### Task 4: framework Settings in the GUI

**Files:**
- Create: `crates/sapphire-agent-gui/src/settings.rs`
- Modify: `crates/sapphire-agent-gui/Cargo.toml`
  - `sapphire-framework-gui = { version = "0.23", default-features = false }`
  - `corundum-framework-gui = { version = "0.3", default-features = false, features = ["serde"] }`
- Modify: `crates/sapphire-agent-gui/src/{lib.rs,chat/view.rs}`

**Interfaces:**
- Consumes: `GuiRuntime`, `GuiWaker` and `AgentGuiPlugin.app` from Task 3.
- Produces:
  - `pub struct GuiSettings { pub settings: sapphire_framework_gui::Settings, pub open: bool }`, inserted as a NonSend resource. `open` switches between the chat and Settings, and plan 3's menu will set it.

- [ ] **Step 1:** Startup, in `AgentGuiPlugin::build`:
  - `ClientConfig::new(self.app)`. On error, log it and skip Settings: no resource, and the chat still works.
  - `FrameworkClient::spawn(&runtime.handle(), config, waker.clone())`
  - `Settings::new(client)`. No app sections yet: the "キャラクター" page is plan 3.
- [ ] **Step 2:** Add the look store. On the first `EguiPrimaryContextPass`, in the same one-shot style as fonts:
  - `corundum_framework_gui::store::attach(ctx, Store::for_app("sapphire-agent-desktop")?)`
  - If the store's `state(ctx).look` is `Some`, `look::apply` it, so the window opens in the last look before the bridge answers.
  - Use the desktop's own app name for the store, so it doesn't share a file with the agent CLI.
- [ ] **Step 3:** Add the per-frame system in `EguiPrimaryContextPass`, ordered before the chat view:
  - `register(ctx, RESOURCES)` for i18n
  - `settings.poll()`
  - `settings.follow_theme(ctx)`
  - These run whether or not Settings is open.
- [ ] **Step 4:** Make the view changes:
  - A "Settings" button in the top bar toggles `open`.
  - When `open`, the central area shows `settings.ui(ui)` in place of the history, with a "Back to chat" button in the top bar.
  - The input bar is hidden while open.
  - When `settings.health() == Health::Problem` and Settings is closed, the top bar draws `settings.status_ui(ui)` under the session line.
  - Strings stay in English, as in the existing chat. Localising the agent's own strings is out of scope.
- [ ] **Step 5:** Run `cargo tree -p sapphire-agent-desktop -i eframe`. Expected: an error saying the package `eframe` was not found, meaning it is not in the graph. Run `cargo tree -d -e normal -p sapphire-agent-desktop | grep -E '^(egui|sapphire-framework|corundum)'`. Expected: no output.
- [ ] **Step 6:** Run the three Global checks. Expected: all pass.
- [ ] **Step 7:** Manual check with the agent and bridge running. Record each result in the report:
  - Settings opens and shows Workspace, Devices, Appearance and About.
  - Changing the theme on Appearance restyles the chat at once.
  - The look survives a restart.
  - With the agent stopped, the banner appears on the chat screen.
- [ ] **Step 8: Commit** `feat(gui): the framework's settings, and the theme and language they hold`

### Task 5: bevy_vrm1 fork on bevy 0.20

> **Superseded.** Upstream bevy_vrm1 0.11.0 (2026-10-29, upstream PR #73) targets bevy 0.20, so plan 3 uses `bevy_vrm1 = "0.11"` from crates.io and this fork is not needed. `sapphire-turtle/bevy_vrm1` (branch `bevy-0.20`) stays only for future bevy bumps that run ahead of upstream. The text below is kept as written.

**Files:**
- Outside this repo: fork `not-elm/bevy_vrm1` to `sapphire-turtle/bevy_vrm1`, clone it to `../forks/bevy_vrm1`, and create branch `bevy-0.20` from upstream `main` (380076d, which is 0.10.0 plus the partial-extension fix).
- This repo is untouched. Plan 3 adds the dependency.

**Interfaces:**
- Produces: `bevy_vrm1 = { git = "https://github.com/sapphire-turtle/bevy_vrm1", branch = "bevy-0.20" }`, with features `serde` and `log` as upstream has them. It builds against the same bevy 0.20 as the desktop.

- [ ] **Step 1:** Fork, clone and branch. Raise `bevy` to `0.20` in both `[dependencies]` and `[dev-dependencies]`, and `bevy_panorbit_camera` to its bevy 0.20 release. `bevy_test_helper` is a git dev-dependency on branch `v0.18`: use a bevy 0.20 branch if upstream has one. Otherwise vendor what the tests use into `tests/support`, and say so in the report.
- [ ] **Step 2:** Port to bevy 0.20 following bevy's 0.19→0.20 migration guide until both of these pass: `cargo build --all-features` and `cargo clippy --all-targets --all-features -- -D warnings`.
- [ ] **Step 3:** Run `cargo test --all-features`. Expected: the same pass count as upstream on bevy 0.19 (81 unit tests plus 10 doctests at 380076d). Run upstream's count first on `main` to confirm the baseline.
- [ ] **Step 4:** Manual check: run the crate's basic VRM example, `cargo run --example <the one that loads a .vrm>`, with a VRM 1.0 model (VRoid's AvatarSample is fine). The model shows with MToon shading, blinks and expressions work, and spring bones move. Record the result in the report.
- [ ] **Step 5:** Commit `Port to bevy 0.20` in the fork, add the README note (as in Task 2) and push the branch.
