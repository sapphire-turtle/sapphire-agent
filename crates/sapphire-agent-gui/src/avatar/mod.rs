//! The companion's avatar: its files, synced from the agent into a local
//! cache ([`assets`]), and the bevy side that keeps them current; what it
//! shows ([`state`]); and which renderer draws it ([`mode`]).
//!
//! # The renderer contract (spec §14)
//!
//! Each renderer is one bevy plugin whose systems run only while
//! [`ActiveRenderer`] names it ([`renderer_is`]). It reads [`AvatarState`],
//! [`AvatarAssets`] and the cached files, and writes [`DeclaredAvatar`],
//! [`AvatarBounds`] and [`RendererError`]. A renderer that needs a scene
//! camera spawns its own, at an `order` below [`UI_CAMERA_ORDER`] and with
//! `ClearColorConfig::Custom(Color::NONE)`, and despawns it when it stops
//! being chosen. The renderers know nothing of each other, and nothing
//! outside them knows what they draw. [`placeholder`] follows the same
//! contract.

pub mod assets;
pub mod mode;
pub mod placeholder;
pub mod state;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bevy::prelude::*;
use bevy_egui::{EguiGlobalSettings, EguiPrimaryContextPass, PrimaryEguiContext};
use sapphire_agent_core::AvatarCaps;
use tokio::runtime::Handle;
use tokio::sync::mpsc;

use crate::Waker;
use crate::chat::{ChatBridge, SyncRequested};
use crate::companion::CompanionPrefs;
pub use assets::{AvatarAssets, AvatarOps, AvatarSource, Cache, LoadedAvatar, OpsStatus};
pub use mode::{ActiveRenderer, DisplayMode, choose, renderer_is};
pub use state::AvatarState;

/// The cues the active renderer can show, as the chat declares them to the
/// agent with each turn. `None`: no avatar, so the agent offers no avatar
/// tools.
#[derive(Resource, Default)]
pub struct DeclaredAvatar(pub Option<AvatarCaps>);

/// Where the avatar is on screen, in logical window pixels with the origin
/// at the top left: the space egui's points are in. Gestures and click
/// pass-through hit-test against it. `None` until a renderer draws.
#[derive(Resource, Default, Clone, Copy)]
pub struct AvatarBounds(pub Option<Rect>);

/// Why the active renderer cannot draw the avatar, for the settings page
/// and the speech bubble.
#[derive(Resource, Default)]
pub struct RendererError(pub Option<String>);

/// The camera the GUI's egui draws with. It sits above every renderer's
/// scene camera.
pub const UI_CAMERA_ORDER: isize = 100;

/// Marks the GUI's own camera, the one carrying the primary egui context.
#[derive(Component)]
pub struct UiCamera;

/// The avatar and its renderers, and the GUI's camera. Add it after
/// `bevy_egui::EguiPlugin`; [`crate::AgentGuiPlugin`] does.
pub struct AvatarPlugin;

impl Plugin for AvatarPlugin {
    fn build(&self, app: &mut App) {
        // The GUI's camera carries the primary egui context, not whichever
        // camera a renderer happens to spawn first.
        match app.world_mut().get_resource_mut::<EguiGlobalSettings>() {
            Some(mut settings) => settings.auto_create_primary_context = false,
            None => {
                app.insert_resource(EguiGlobalSettings {
                    auto_create_primary_context: false,
                    ..default()
                });
            }
        }
        app.insert_resource(ActiveRenderer::Placeholder)
            .init_resource::<AvatarState>()
            .init_resource::<DeclaredAvatar>()
            .init_resource::<AvatarBounds>()
            .init_resource::<RendererError>()
            .add_systems(Startup, spawn_ui_camera)
            .add_systems(
                Update,
                (choose_renderer, tick_state, clear_without_scene)
                    .chain()
                    .after(drain_sync),
            )
            .add_systems(
                EguiPrimaryContextPass,
                placeholder::draw
                    .run_if(renderer_is(ActiveRenderer::Placeholder))
                    // In the theme the settings just applied, and before the
                    // gestures hit-test against the bounds it reports. Its
                    // area is `Order::Background` and the overlay's above
                    // it, so the overlay paints over it either way.
                    .after(crate::settings::follow)
                    .before(crate::companion::pointer_input),
            );
    }
}

/// The GUI's camera: it draws egui over whatever the renderers drew and
/// clears nothing itself, unless [`clear_without_scene`] finds nothing to
/// draw over.
fn spawn_ui_camera(mut commands: Commands) {
    commands.spawn((
        Camera2d,
        Camera {
            order: UI_CAMERA_ORDER,
            clear_color: ClearColorConfig::None,
            ..default()
        },
        PrimaryEguiContext,
        UiCamera,
    ));
}

/// With no active scene camera (the placeholder draws in egui, and a
/// renderer swap can leave a frame between one camera and the next),
/// nothing else clears the window, so the UI camera clears it with the
/// app's `ClearColor`; otherwise it draws over the scene.
fn clear_without_scene(
    mut ui: Single<&mut Camera, With<UiCamera>>,
    scene: Query<&Camera, Without<UiCamera>>,
) {
    let has_scene = scene.iter().any(|camera| camera.is_active);
    let clears = !matches!(ui.clear_color, ClearColorConfig::None);
    if has_scene == clears {
        ui.clear_color = if has_scene {
            ClearColorConfig::None
        } else {
            ClearColorConfig::Default
        };
    }
}

/// Picks the renderer for the avatar that synced, in the display mode the
/// companion's prefs hold. When it changes, the new
/// one starts from nothing: neutral, nothing declared, no bounds or error.
fn choose_renderer(
    assets: Res<AvatarAssets>,
    prefs: Res<CompanionPrefs>,
    mut active: ResMut<ActiveRenderer>,
    mut state: ResMut<AvatarState>,
    mut declared: ResMut<DeclaredAvatar>,
    mut bounds: ResMut<AvatarBounds>,
    mut error: ResMut<RendererError>,
) {
    let chosen = choose(
        prefs.display_mode,
        assets.loaded.as_ref().map(|l| &l.config),
    );
    if chosen == *active {
        return;
    }
    *active = chosen;
    state.reset();
    declared.0 = None;
    bounds.0 = None;
    error.0 = None;
}

/// Reverts the avatar once a finished turn's revert time comes. On the
/// wall clock (`Time<Real>`): virtual time barely moves while the GUI
/// idles between wakes.
fn tick_state(time: Res<Time<Real>>, mut state: ResMut<AvatarState>) {
    let mut next = state.clone();
    next.tick(time.elapsed());
    // Renderers react to a change, so most frames must not make one.
    state.set_if_neq(next);
}

/// How often the agent is asked whether the avatar changed.
pub const POLL_INTERVAL: Duration = Duration::from_secs(300);

/// Runs syncs on the GUI's runtime and hands their results to
/// [`drain_sync`]. One sync runs at a time: a request made while one is in
/// flight is dropped, since that sync reads the agent's newest manifest
/// anyway or the next poll will.
#[derive(Resource)]
pub struct AvatarSync {
    runtime: Handle,
    waker: Waker,
    cache_dir: Option<std::path::PathBuf>,
    in_flight: Arc<AtomicBool>,
    tx: mpsc::UnboundedSender<anyhow::Result<LoadedAvatar>>,
    rx: Mutex<mpsc::UnboundedReceiver<anyhow::Result<LoadedAvatar>>>,
}

impl AvatarSync {
    pub fn new(runtime: Handle, waker: Waker) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        Self {
            runtime,
            waker,
            cache_dir: Cache::default_dir(),
            in_flight: Arc::new(AtomicBool::new(false)),
            tx,
            rx: Mutex::new(rx),
        }
    }

    /// Sync from `source` unless a sync is already running. Returns whether
    /// one was started; on `false` the caller keeps its request and retries.
    pub fn submit(&self, source: Arc<dyn AvatarSource>) -> bool {
        if self.in_flight.swap(true, Ordering::AcqRel) {
            return false;
        }
        let cache_dir = self.cache_dir.clone();
        let in_flight = Arc::clone(&self.in_flight);
        let tx = self.tx.clone();
        let waker = Arc::clone(&self.waker);
        self.runtime.spawn(async move {
            let result = match cache_dir {
                Some(dir) => assets::sync(source.as_ref(), &Cache::new(dir)).await,
                None => Err(anyhow::anyhow!("this platform has no cache directory")),
            };
            in_flight.store(false, Ordering::Release);
            let _ = tx.send(result);
            waker();
        });
        true
    }

    fn try_recv(&self) -> Option<anyhow::Result<LoadedAvatar>> {
        self.rx.lock().ok()?.try_recv().ok()
    }
}

/// Whether a sync is wanted: one was requested, or [`POLL_INTERVAL`] of
/// real time passed since the last one started. Measured on the wall clock
/// (`Time<Real>`), not frames, because the GUI idles between wakes.
fn wanted(now: Duration, last_started: Duration, requested: bool) -> bool {
    requested || now.saturating_sub(last_started) >= POLL_INTERVAL
}

/// Starts a sync when one is [`wanted`]; needs the live connection, so it
/// waits for one. A request stays pending until a sync actually starts, so
/// one made during a running sync is retried on later frames.
pub(crate) fn poll_sync(
    time: Res<Time<Real>>,
    mut last_started: Local<Duration>,
    mut requested: ResMut<SyncRequested>,
    bridge: Res<ChatBridge>,
    sync: Res<AvatarSync>,
) {
    let now = time.elapsed();
    if !wanted(now, *last_started, requested.0) {
        return;
    }
    if let Some(client) = bridge.client()
        && sync.submit(client)
    {
        requested.0 = false;
        *last_started = now;
    }
}

/// Folds finished syncs into [`AvatarAssets`].
pub(crate) fn drain_sync(sync: Res<AvatarSync>, mut assets: ResMut<AvatarAssets>) {
    while let Some(result) = sync.try_recv() {
        if let Err(e) = &result {
            tracing::warn!("avatar sync failed: {e:#}");
        }
        assets.apply(result);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Idle;

    #[async_trait::async_trait]
    impl AvatarSource for Idle {
        async fn manifest(&self) -> anyhow::Result<sapphire_agent_core::AvatarManifest> {
            Ok(Default::default())
        }
        async fn fetch(&self, _: &str) -> anyhow::Result<Vec<u8>> {
            unreachable!()
        }
    }

    #[test]
    fn the_poll_is_due_after_300_seconds_of_real_time() {
        let s = Duration::from_secs;
        assert!(!wanted(s(299), s(0), false));
        assert!(wanted(s(300), s(0), false));
        // Measured from the last start, however few frames ran between.
        assert!(!wanted(s(500), s(300), false));
        assert!(wanted(s(600), s(300), false));
        assert!(wanted(s(1), s(0), true));
    }

    #[test]
    fn a_request_made_during_a_sync_survives_and_runs_after_it() {
        // Not driven: the spawned sync stays in flight until the runtime runs.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let sync = AvatarSync::new(runtime.handle().clone(), Arc::new(|| {}));
        let source: Arc<dyn AvatarSource> = Arc::new(Idle);

        assert!(sync.submit(Arc::clone(&source)));
        // A new request arrives while that sync runs: it is refused, so
        // `poll_sync` leaves `SyncRequested` set and asks again.
        assert!(!sync.submit(Arc::clone(&source)));

        runtime.block_on(async {
            while sync.in_flight.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        });
        assert!(sync.submit(source), "runs once the first sync is done");
    }
}
