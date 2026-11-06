# Default Avatar on GUI Startup Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** When the GUI connects to a workspace with no avatar, the server downloads the pinned default avatar while the GUI shows a loading screen. A synced workspace with other devices waits for a button instead. The asset repo stops shipping `profile.md`.

**Architecture:** The server gets a `DefaultInstaller` in `default_avatar.rs`. It decides whether to fetch (release pinned, avatar absent, sync rule), runs the existing blocking `install` once at a time, and throttles progress. One IPC request, `sapphire-agent.avatar.install_default`, exposes it, with `install_progress` notifications sent to the caller. The GUI drives a small pure state machine: check the manifest on each new connection, request with `auto: true`, and show `Screen::Loading` while waiting or after a failure. The Character page offers the same request with `auto: false`.

**Tech Stack:** Rust (tokio, `sapphire_framework::ipc` router and client, `futures-util` `Shared`, bevy + egui), Python 3 stdlib for the asset repo.

**Spec:** `docs/superpowers/specs/2026-11-05-default-avatar-pack-design.md` (§3 and §4.2–4.3; §1 and §2 for Task 5)

## Global Constraints

- Code and comments in English, and so is GUI copy, like the rest of the GUI. The spec's Japanese wording is the meaning; the GUI strings are: loading `"Downloading your character…"`; failure buttons `"Retry"` and `"Continue without an avatar"`; page button `"Get the default avatar"`.
- Commit scopes: server `feat(workspace)`, core `feat(rpc)`, GUI `feat(gui)`. Agent-internal scopes need no `cliff.toml` change.
- `APP_API` goes from 3 to 4. Its doc comment lists what 4 added.
- `INITIAL` is `None` in this plan. Placeholder values are forbidden: no all-zero sha256 and no `placeholder-chibi-v1`. Tests build their own `AvatarRelease`.
- `install` is blocking. Run it only through `tokio::task::spawn_blocking`.
- At most one install runs per server. Concurrent requests share its answer.
- Progress notifications are sent at most once per 250 ms, plus the final one. Shape: `{ "received": u64, "total": u64 | null }`.
- Answers on the wire are snake_case strings: `installed`, `already_present`, `unavailable`, `needs_confirmation`. Download and verify failures are IPC errors carrying `install`'s message.
- Sync rule: fetch automatically only when sync is off, or when sync is on, the bridge is reachable and `peers == 0`.
- "No avatar" in the GUI means the manifest's `config.vrm` and `config.sprite` are both `None`.
- `CHARACTER.md` (§4.1) is **not** added to `TEMPLATES` here. Its content does not exist yet, and the spec says not to ship an empty one.
- No `desktop/` change. With `INITIAL = None` nothing user-visible ships. The PR that fills `INITIAL` touches `desktop/` for the release (CLAUDE.md, "A GUI-only change makes no desktop release").

## Review Focus

- **A GUI newer than its server.** If the server lacks `install_default` (METHOD_NOT_FOUND), the user expects the silhouette, not a failure screen they cannot leave. The client maps that error to `Unavailable` (Task 2 test).
- **Offline at first start.** The user expects to get past the loading screen. "Continue without an avatar" is always offered on failure, and no automatic retry happens during that connection (Task 4 tests).
- **Sync enabled but the bridge is down.** `peers` reads 0 then. The user expects no download that races the sync, so the answer is `needs_confirmation` (Task 1 test).
- **Two GUI windows or a double click.** The user expects one download, not two fighting over `avatar/`. Requests are single-flight with a shared answer (Task 1 test).
- **Reconnecting after the agent restarts** (workspace switch). The user expects the new workspace to be checked. A new client connection resets the flow (Task 4 test).

---

### Task 1: The installer behind the IPC method

**Files:**
- Modify: `crates/sapphire-agent-core/src/lib.rs` (wire types only)
- Modify: `crates/sapphire-agent-server/src/default_avatar.rs`

**Interfaces:**
- Consumes: `install(&Path, &AvatarRelease<'_>, impl FnMut(u64, Option<u64>)) -> Result<Outcome>` and the test loopback HTTP server, both already in `default_avatar.rs`.
- Produces (core):
  ```rust
  pub const AVATAR_INSTALL_DEFAULT: &str = "sapphire-agent.avatar.install_default";
  pub const AVATAR_INSTALL_PROGRESS: &str = "sapphire-agent.avatar.install_progress";
  #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
  #[serde(rename_all = "snake_case")]
  pub enum DefaultAvatarAnswer { Installed, AlreadyPresent, Unavailable, NeedsConfirmation }
  #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
  pub struct InstallProgress { pub received: u64, pub total: Option<u64> }
  ```
- Produces (server, `default_avatar`):
  ```rust
  pub(crate) const INITIAL: Option<AvatarRelease<'static>> = None;
  #[derive(Clone, Copy, Debug, PartialEq, Eq)]
  pub(crate) struct SyncView { pub enabled: bool, pub peers: usize, pub bridge_available: bool }
  pub(crate) fn auto_allowed(sync: SyncView) -> bool;
  #[async_trait::async_trait]
  pub(crate) trait SyncProbe: Send + Sync { async fn view(&self) -> SyncView; }
  pub(crate) struct DefaultInstaller { /* root, release, probe, in-flight */ }
  impl DefaultInstaller {
      pub(crate) fn new(root: PathBuf, release: Option<AvatarRelease<'static>>, probe: Arc<dyn SyncProbe>) -> Self;
      pub(crate) async fn run(
          &self,
          auto: bool,
          progress: Arc<dyn Fn(InstallProgress) + Send + Sync>,
      ) -> Result<DefaultAvatarAnswer, String>;
  }
  ```
  `run` checks in this order: `release` is `None` → `Unavailable`; `avatar/avatar.toml` exists → `AlreadyPresent`, without asking the probe; `auto && !auto_allowed(probe.view().await)` → `NeedsConfirmation`; otherwise it joins or starts the single in-flight install. The in-flight install is a `futures_util::future::Shared` held in a `Mutex<Option<…>>` and cleared when it finishes. It runs `install` in `spawn_blocking`, with progress throttled to 250 ms and the final call always sent. Only the request that started the install gets progress. Joiners get just the answer.

- [ ] **Step 1: Write the failing tests** in `default_avatar.rs`'s test module:
  - `auto_allowed_follows_the_sync_rule`: `(enabled=false, peers=3, bridge=false) → true`; `(true, 0, true) → true`; `(true, 1, true) → false`; `(true, 0, false) → false`.
  - `run_without_a_release_is_unavailable`: `release: None` → `Ok(Unavailable)`.
  - `run_with_an_avatar_is_already_present_without_probing`: `avatar/avatar.toml` exists; the probe panics if called → `Ok(AlreadyPresent)`.
  - `run_auto_with_peers_needs_confirmation_and_fetches_nothing`: the probe returns `(true, 1, true)`, `auto = true` → `Ok(NeedsConfirmation)`; the loopback server saw 0 requests; `avatar/` does not exist.
  - `run_not_auto_ignores_the_sync_rule`: the same probe, `auto = false` → `Ok(Installed)`, and `avatar/avatar.toml` exists.
  - `concurrent_runs_download_once_and_share_the_answer`: two `run(false, …)` futures joined together → both `Ok(Installed)`, and the server saw exactly 1 request.
  - `a_failed_run_reports_install_s_error_and_leaves_no_avatar`: wrong sha256 → `Err(msg)` with `msg` containing `"sha256"`; no `avatar/avatar.toml`.
  - `progress_reports_the_total_and_ends_at_the_full_size`: the last `InstallProgress` the callback saw has `received == total == Some(tar.len())`.
  - `progress_is_throttled`: the pure helper `Throttle::due(&mut self, now: Instant) -> bool` returns `true` first, `false` at +100 ms, and `true` at +260 ms.

- [ ] **Step 2: Run them to verify they fail**
  Run: `cargo test -p sapphire-agent-server default_avatar 2>&1 | tail -20`
  Expected: compile errors for `DefaultInstaller`, `SyncView`, `auto_allowed`, `Throttle`, `DefaultAvatarAnswer`.

- [ ] **Step 3: Implement.** Add the core types, then everything in "Produces (server)", plus `Throttle`. Change `INITIAL` to `Option` and set it to `None`. Keep the `FIXME(#340)` comment, saying it gets the first release's URL and `.sha256`. Keep the module's `allow(dead_code)` until Task 2 uses it.

- [ ] **Step 4: Run the tests and clippy**
  Run: `cargo test -p sapphire-agent-server default_avatar 2>&1 | tail -5 && cargo clippy -p sapphire-agent-server --all-targets -- -D warnings 2>&1 | tail -3`
  Expected: all pass; no warnings.

- [ ] **Step 5: Commit**
  ```bash
  git add crates/sapphire-agent-core/src/lib.rs crates/sapphire-agent-server/src/default_avatar.rs
  git commit -m "feat(workspace): decide and run the default avatar install once at a time"
  ```

### Task 2: `install_default` over IPC

**Files:**
- Modify: `crates/sapphire-agent-core/src/lib.rs` (`APP_API`, client method)
- Modify: `crates/sapphire-agent-server/src/serve/avatar_ipc.rs`, `crates/sapphire-agent-server/src/serve/chat_ipc.rs`, `crates/sapphire-agent-server/src/lib.rs`
- Modify: `crates/sapphire-agent-server/src/default_avatar.rs` (`RuntimeProbe`; drop `allow(dead_code)`)

**Interfaces:**
- Consumes: Task 1's `DefaultInstaller`, `SyncProbe`, `SyncView`, `DefaultAvatarAnswer`, `InstallProgress`, and the constants.
- Produces:
  ```rust
  // core
  pub const APP_API: u32 = 4;
  impl ChatClient {
      /// `Unavailable` when the server predates the method (METHOD_NOT_FOUND).
      pub async fn avatar_install_default(
          &self, auto: bool, on_progress: impl FnMut(InstallProgress),
      ) -> Result<DefaultAvatarAnswer>;
  }
  // server
  pub(crate) struct RuntimeProbe { pub runtime: Arc<SyncRuntime>, pub root: PathBuf } // impl SyncProbe via runtime.status(&root)
  impl ChatTunnel { pub fn attach_default_avatar(&self, installer: DefaultInstaller); }
  ```
  - The client method follows the shape of `send`: subscribe to notifications before the call, read `AVATAR_INSTALL_PROGRESS` params, and drain any left after the answer. Map an `RpcError` whose code is `codes::METHOD_NOT_FOUND` to `Ok(Unavailable)` before converting other errors to `anyhow`. Put that mapping in a small pure function so it can be tested.
  - The server registers `AVATAR_INSTALL_DEFAULT` in `avatar_ipc::register`, from a new `Arc<OnceLock<Arc<DefaultInstaller>>>` cell on `ChatTunnel`. An empty cell gets the same "still starting" error as `run`. The `progress` closure sends `AVATAR_INSTALL_PROGRESS` through `ctx.peer.notify` and is best-effort: errors are ignored. Because the callback is sync, spawn the notify or send it through a channel.
  - In `lib.rs`, clone `sync_runtime` before it moves into `Placement::synced`. Attach `DefaultInstaller::new(served_root.clone(), default_avatar::INITIAL, Arc::new(RuntimeProbe { … }))` next to `chat_tunnel.attach(state)`.

- [ ] **Step 1: Write the failing tests**
  - In `avatar_ipc.rs` tests (fixture: `attach_default_avatar` with a test release pointing at the loopback server, and a fixed probe):
    - `install_default_installs_and_reports_progress`: `chat.avatar_install_default(false, |p| seen.push(p))` → `Installed`; `seen` is non-empty and its last item has `total == Some(len)`; then `chat.avatar_manifest()` has `config.sprite.is_some()` (use a sprite-only test tar).
    - `install_default_auto_with_peers_needs_confirmation`: probe `(true, 2, true)` → `NeedsConfirmation`.
    - `install_default_before_attach_is_an_error`: no installer attached → `Err` whose text contains `"starting"`.
  - In core tests: `method_not_found_means_unavailable` uses the pure mapper: `METHOD_NOT_FOUND` → `Ok(Unavailable)`; `INTERNAL_ERROR` → `Err`.

- [ ] **Step 2: Run them to verify they fail**
  Run: `cargo test -p sapphire-agent-server avatar_ipc 2>&1 | tail -20; cargo test -p sapphire-agent-core 2>&1 | tail -10`
  Expected: compile errors for `avatar_install_default` and `attach_default_avatar`.

- [ ] **Step 3: Implement** everything in "Produces". Update `APP_API`'s doc comment: "4: added `sapphire-agent.avatar.install_default` and its `install_progress` notification."

- [ ] **Step 4: Run the workspace tests and clippy**
  Run: `cargo test --workspace 2>&1 | grep -E '^test result|FAILED|panicked' | sort | uniq -c; cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -3`
  Expected: every `test result: ok`; no warnings.

- [ ] **Step 5: Commit** in two commits, so each scope routes its changelog:
  ```bash
  git add crates/sapphire-agent-core && git commit -m "feat(rpc): avatar.install_default and its progress, API 4"
  git add crates/sapphire-agent-server && git commit -m "feat(workspace): serve avatar.install_default"
  ```

### Task 3: The GUI's flow, as a pure state machine

**Files:**
- Create: `crates/sapphire-agent-gui/src/avatar/default_install.rs` (declare it `pub(crate) mod default_install;` in `avatar/mod.rs`)

**Interfaces:**
- Consumes: Task 2's `DefaultAvatarAnswer` and `InstallProgress`.
- Produces:
  ```rust
  #[derive(Clone, Debug, PartialEq)]
  pub(crate) enum Flow {
      /// Connected, the manifest not yet checked.
      Checking,
      Loading { received: u64, total: Option<u64> },
      Failed { message: String },
      /// The normal screen. `offer`: the Character page shows "Get the default avatar".
      Done { offer: bool },
  }
  /// A request to send: `install_default { auto }`.
  #[derive(Clone, Copy, Debug, PartialEq, Eq)]
  pub(crate) struct Request { pub auto: bool }
  impl Flow {
      pub(crate) fn new() -> Self;                                    // Checking
      pub(crate) fn checked(&mut self, has_avatar: bool) -> Option<Request>;
      pub(crate) fn progress(&mut self, p: InstallProgress);
      /// Returns whether to resync the avatar now.
      pub(crate) fn answered(&mut self, a: Result<DefaultAvatarAnswer, String>) -> bool;
      pub(crate) fn retry(&mut self) -> Option<Request>;              // Failed → Loading, auto: false
      pub(crate) fn continue_without(&mut self);                     // Failed → Done { offer: true }
      pub(crate) fn get(&mut self) -> Option<Request>;                // Done { offer: true } → Loading, auto: false
      pub(crate) fn blocks_screen(&self) -> bool;                     // Loading | Failed
  }
  ```
  Transitions:
  - `checked(true)` → `Done { offer: false }`, no request.
  - `checked(false)` → `Loading { 0, None }` and `Request { auto: true }`.
  - `answered(Ok(Installed | AlreadyPresent))` → `Done { offer: false }`, returns `true`.
  - `answered(Ok(NeedsConfirmation))` → `Done { offer: true }`.
  - `answered(Ok(Unavailable))` → `Done { offer: false }`.
  - `answered(Err(m))` → `Failed { message: m }`.
  - Calls that don't fit the current state are no-ops returning `None` / `false`. `progress` applies only in `Loading`.

- [ ] **Step 1: Write the failing tests** in the new file:
  - `an_avatar_present_needs_nothing`: `checked(true)` returns `None`; the state is `Done { offer: false }`; `blocks_screen()` is false.
  - `no_avatar_loads_automatically`: `checked(false)` returns `Some(Request { auto: true })`; `blocks_screen()` is true.
  - `installed_resyncs_and_unblocks`: then `answered(Ok(Installed))` returns `true`; the state is `Done { offer: false }`.
  - `needs_confirmation_offers_the_button`: the state becomes `Done { offer: true }`, and `get()` returns `Some(Request { auto: false })` and goes to `Loading`.
  - `unavailable_offers_nothing`: the state is `Done { offer: false }`; `get()` returns `None`.
  - `failure_blocks_until_retry_or_continue`: `answered(Err("offline"))` gives `Failed`, which blocks the screen. `retry()` returns `Some(auto: false)`. Then fail again, and `continue_without()` gives `Done { offer: true }`, which doesn't block.
  - `no_automatic_retry_after_continuing`: after `continue_without()`, `checked(false)` returns `None`. Only a new `Flow::new()` (a new connection) re-checks.
  - `progress_only_while_loading`: progress in `Done` leaves it unchanged. In `Loading` it updates `received` and `total`.

- [ ] **Step 2: Run them to verify they fail**
  Run: `cargo test -p sapphire-agent-gui default_install 2>&1 | tail -10`
  Expected: compile errors for `Flow`.

- [ ] **Step 3: Implement** `Flow` per the transition list.

- [ ] **Step 4: Run the tests**
  Run: `cargo test -p sapphire-agent-gui default_install 2>&1 | tail -5`
  Expected: all pass.

- [ ] **Step 5: Commit**
  ```bash
  git add crates/sapphire-agent-gui/src/avatar && git commit -m "feat(gui): the default avatar's first-run flow"
  ```

### Task 4: Wire the flow: the loading screen and the Character page button

**Files:**
- Modify: `crates/sapphire-agent-gui/src/avatar/default_install.rs` (bevy resource and systems)
- Modify: `crates/sapphire-agent-gui/src/avatar/mod.rs` (register the systems)
- Modify: `crates/sapphire-agent-gui/src/settings.rs` (`Screen::Loading`)
- Modify: `crates/sapphire-agent-gui/src/companion/view.rs` (draw it)
- Modify: `crates/sapphire-agent-gui/src/companion/character_page.rs` (the button)

**Interfaces:**
- Consumes: Task 3's `Flow` and `Request`; `ChatBridge::client() -> Option<Arc<ChatClient>>`; `SyncRequested` (set `.0 = true` to resync the avatar); `ChatClient::avatar_manifest` and `avatar_install_default`.
- Produces:
  ```rust
  #[derive(Resource)]
  pub(crate) struct DefaultAvatar { /* flow: Flow, the client it belongs to, runtime + waker, event channel */ }
  impl DefaultAvatar {
      pub(crate) fn flow(&self) -> &Flow;
      pub(crate) fn retry(&mut self);
      pub(crate) fn continue_without(&mut self);
      pub(crate) fn get(&mut self);
  }
  pub(crate) fn drive_default_avatar(/* Res<ChatBridge>, ResMut<DefaultAvatar>, ResMut<SyncRequested> */);
  // settings.rs
  pub(crate) enum Screen { Companion, Settings, Loading }
  ```
  - `drive_default_avatar` runs in `Update`, before `AvatarSystems::Choose`.
    - When `bridge.client()` is a different `Arc` from the one stored (`Arc::ptr_eq`), including the first one, it resets to `Flow::new()` and spawns a job on the GUI runtime: `avatar_manifest()`, then `has_avatar = config.vrm.is_some() || config.sprite.is_some()`. A failed manifest read counts as "has an avatar", so the GUI doesn't block on an unrelated error.
    - Each `Request` spawns `avatar_install_default(auto, …)`, which forwards progress and the answer as events.
    - Events are drained each frame into `Flow`. When `answered` returns `true`, it sets `SyncRequested`.
    - It wakes the GUI with the `Waker` the way `AvatarSync` does.
  - `Layout::of` is unchanged. In `view::ui`, check `default_avatar.flow().blocks_screen()` first: if it's true, draw `loading_screen` full-window and return before the settings and the overlay.
  - `loading_screen` (a free function taking `&egui::Context` and `&mut DefaultAvatar`):
    - Centred, with `"Downloading your character…"`.
    - An `egui::ProgressBar`: the fraction when `total` is known, else animated, with `received` in MB as its text.
    - On `Failed`: the message plus `"Retry"` and `"Continue without an avatar"`.
  - Character page: when `flow()` is `Done { offer: true }`, show a `"Get the default avatar"` button under the display mode. Clicking it calls `get()`, and the next frame shows the loading screen. Pass the resource into `CharacterModel::refresh` the way `ops` is passed.

- [ ] **Step 1: Write the failing tests**
  - `default_install.rs`: `a_new_client_restarts_the_check`, a bevy `App` test with a fake source and two client generations: after the second, the flow is `Checking` again. If faking `ChatClient` is not practical, test the pure helper `fn connection_changed(stored: Option<&Arc<T>>, live: Option<&Arc<T>>) -> bool` for (None, Some) → true, (a, a) → false, (a, b) → true, (a, None) → false.
  - `view.rs` / `settings.rs`: `blocking_flow_takes_the_whole_window`. Test the pure choice `fn screen(layout: Layout, blocks: bool) -> Screen`: `blocks = true` → `Loading` even when `layout.screen == Settings`; `false` → `layout.screen`.

- [ ] **Step 2: Run them to verify they fail**
  Run: `cargo test -p sapphire-agent-gui 2>&1 | tail -10`
  Expected: compile errors for `connection_changed` and `screen`.

- [ ] **Step 3: Implement** everything in "Produces" and wire it in.

- [ ] **Step 4: Run the workspace tests and clippy**
  Run: `cargo test --workspace 2>&1 | grep -E '^test result|FAILED|panicked' | sort | uniq -c; cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -3`
  Expected: every `test result: ok`; no warnings.

- [ ] **Step 5: Commit**
  ```bash
  git add crates/sapphire-agent-gui && git commit -m "feat(gui): a loading screen while the default avatar downloads"
  ```

### Task 5: The asset repo stops shipping `profile.md`

Repository: `/home/fluo10/Documents/Dev/project-sapphire/sapphire-agent-avatars` (its own branch and PR; tests: `python -m unittest discover -s tests -t .`).

**Files:**
- Modify: `tools/avatars/pack.py` (no `avatar/profile.md` entry), `tools/avatars/avatar.py` (`check(dir)` drops the `profile` parameter and its UTF-8 and total accounting), `tools/avatars/repo.py` (drop `profile_path` and its use)
- Delete: `character/profile.md` (and `character/` if empty)
- Modify: `README.md` line 15 (drop "plus `profile.md`"), `tests/fixtures.py`, `tests/test_avatar.py`, `tests/test_hardening.py`, `tests/test_pack.py`

**Interfaces:**
- Produces: `avatar.check(dir: Path) -> list[str]`. A tar's entries are `avatar/` and the avatar's own files, nothing else.

- [ ] **Step 1: Change the tests first**
  - In `test_pack.py`, replace the `assertIn("avatar/profile.md", names)` line with `assertNotIn("avatar/profile.md", names)`, and assert that every name starts with `avatar/`.
  - Delete `test_profile_not_utf8_fails`.
  - Update the `check(self.dir, self.profile)` calls to `check(self.dir)`.
  - Drop `profile.md` from the fixtures.

- [ ] **Step 2: Run them to verify they fail**
  Run: `python -m unittest discover -s tests -t . 2>&1 | tail -5`
  Expected: FAIL/ERROR (`check()` signature, and `profile.md` still packed).

- [ ] **Step 3: Implement** the "Files" changes.

- [ ] **Step 4: Run all tests and the check script**
  Run: `python -m unittest discover -s tests -t . 2>&1 | tail -3 && scripts/check`
  Expected: `OK`; the check exits 0.

- [ ] **Step 5: Commit**
  ```bash
  git add -A && git commit -m "feat: character settings live in the agent, not in the avatar tars"
  ```
  `placeholder-chibi-v1` stays as released: releases are immutable, and it exists only to test the path.
