//! The companion's avatar: its files, synced from the agent into a local
//! cache ([`assets`]), and the bevy side that keeps them current; what it
//! shows ([`state`]); and which renderer draws it ([`mode`]).
//!
//! # The renderer contract (spec §14)
//!
//! Each renderer is one bevy plugin whose systems run only while
//! [`ActiveRenderer`] names it ([`renderer_is`]), after
//! [`AvatarSystems::Choose`]. The renderers know nothing of each other, and
//! nothing outside them knows what they draw. [`placeholder`] follows the
//! same contract.
//!
//! **Reads.** [`AvatarState`]: what to show. Read it with `Res`; react to
//! `is_changed()`, which the GUI keeps for real changes. Peek at the
//! action with [`AvatarState::pending_action`] and take
//! [`AvatarState::take_action`] (`ResMut`) only when playing it.
//! [`AvatarAssets`]: the synced config, and the cached files, which load
//! through bevy's `AssetServer` from [`LoadedAvatar::asset_path`] (never the
//! absolute paths); reload when its `generation` changes.
//!
//! **Writes.** [`DeclaredAvatar`]: the cues it loaded and can show.
//! [`AvatarBounds`]: where the avatar is. [`RendererError`]: why it cannot
//! draw; while set, it draws, declares and bounds nothing and the
//! placeholder stands in, and it clears it when a (re)load succeeds. Write
//! each only when the value differs (`set_if_neq`, or compare first): other
//! systems react to their change.
//!
//! **Bounds timing.** The gestures and click-through hit-test
//! [`AvatarBounds`] in the egui pass (`PostUpdate`), so a renderer writes
//! them in the same frame, before it: one measuring its scene writes them
//! in [`AvatarSystems::Bounds`] (`PostUpdate`, after transforms propagate
//! and cameras update); one drawing in egui, as the placeholder does,
//! writes them in `EguiPrimaryContextPass` before
//! `companion::pointer_input`. There is no frame of lag to allow for.
//!
//! **Cameras.** A renderer that needs a scene camera spawns its own, at an
//! `order` below [`UI_CAMERA_ORDER`] and with
//! `ClearColorConfig::Custom(Color::NONE)`, and despawns it when it stops
//! being chosen. [`ActiveRenderer`] is a resource, not a `State`, so there
//! is no `OnExit`: the teardown is a system *without* the `renderer_is`
//! gate, run when the choice changes:
//!
//! ```ignore
//! app.add_systems(
//!     Update,
//!     despawn_sprite_camera
//!         .run_if(resource_changed::<ActiveRenderer>)
//!         .after(AvatarSystems::Choose),
//! );
//!
//! fn despawn_sprite_camera(
//!     active: Res<ActiveRenderer>,
//!     cameras: Query<Entity, With<SpriteCamera>>,
//!     mut commands: Commands,
//! ) {
//!     if *active != ActiveRenderer::Sprite {
//!         for camera in &cameras {
//!             commands.entity(camera).despawn();
//!         }
//!     }
//! }
//! ```
//!
//! The UI camera sees no render layer, so the renderer's entities are drawn
//! by its camera alone, whatever layer they are on.
//!
//! **Frame pacing.** The GUI runs under `WinitSettings::desktop_app()`: a
//! frame comes only on input, a [`crate::Waker`] call or a redraw request.
//! A renderer asks for frames only while it animates (a transition, an
//! action, an idle loop): a `RequestRedraw` message each frame for full
//! rate, or, where fewer frames do, a [`crate::GuiWaker`] call after a
//! timer on the [`crate::GuiRuntime`] (`tokio::time::sleep`, then the
//! waker) to cap it. A still avatar asks for none, so an idle companion
//! draws no frames.

pub mod assets;
pub mod mode;
pub mod placeholder;
pub mod sprite;
pub mod state;
pub mod vrm;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bevy::camera::visibility::RenderLayers;
use bevy::camera::{CameraUpdateSystems, NormalizedRenderTarget, RenderTarget};
use bevy::prelude::*;
use bevy::transform::TransformSystems;
use bevy::window::PrimaryWindow;
use bevy_egui::{EguiGlobalSettings, EguiPrimaryContextPass, PrimaryEguiContext};
use sapphire_agent_core::AvatarCaps;
use tokio::runtime::Handle;
use tokio::sync::mpsc;

use crate::Waker;
use crate::chat::{ChatBridge, SyncRequested};
use crate::companion::CompanionPrefs;
pub use assets::{
    AvatarAssets, AvatarEditor, AvatarOps, AvatarSource, Cache, LoadedAvatar, OpTicket, OpsStatus,
};
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
/// and the speech bubble. While it is set the renderer draws nothing,
/// declares nothing and leaves [`AvatarBounds`] alone: the placeholder
/// stands in ([`placeholder::stands_in`]). The renderer clears it when a
/// (re)load succeeds, then declares and bounds what it draws.
#[derive(Resource, Default)]
pub struct RendererError(pub Option<String>);

/// The camera the GUI's egui draws with. It sits above every renderer's
/// scene camera.
pub const UI_CAMERA_ORDER: isize = 100;

/// Marks the GUI's own camera, the one carrying the primary egui context.
#[derive(Component)]
pub struct UiCamera;

/// Where the avatar's shared systems run, for renderers to order against.
#[derive(SystemSet, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AvatarSystems {
    /// `Update`: the renderer is chosen for the avatar that synced and the
    /// state reverts when due. A renderer's `Update` systems go `.after`
    /// it, so they see this frame's choice.
    Choose,
    /// `PostUpdate`, after transform propagation and the camera update and
    /// before the egui pass: where a renderer measuring its scene writes
    /// [`AvatarBounds`] for the frame's gestures.
    Bounds,
}

/// Registers the [`assets::ASSET_SOURCE`] asset source, rooted at the
/// avatar cache, so renderers can load cached files through bevy's
/// `AssetServer` ([`LoadedAvatar::asset_path`]). The host calls this on
/// its `App` before adding `DefaultPlugins`: bevy builds its sources when
/// `AssetPlugin` is added and takes none after. Where the platform has no
/// cache directory there is nothing to serve, and nothing is registered.
pub fn register_asset_source(app: &mut App) {
    match Cache::default_dir() {
        Some(dir) => register_asset_source_at(app, dir),
        None => tracing::warn!("no cache directory; the avatar's files cannot be loaded"),
    }
}

fn register_asset_source_at(app: &mut App, dir: std::path::PathBuf) {
    use bevy::asset::AssetApp as _;
    use bevy::asset::io::AssetSourceBuilder;
    use bevy::asset::io::file::FileAssetReader;

    // Read-only, unwatched: the cache changes only by syncs, which bump
    // `AvatarAssets::generation` for renderers to reload on.
    app.register_asset_source(
        assets::ASSET_SOURCE,
        AssetSourceBuilder::new(move || Box::new(FileAssetReader::new(dir.clone()))),
    );
}

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
            .init_resource::<AvatarAssets>()
            // The defaults until `CompanionPlugin` loads the saved ones, or
            // for good in an app without it.
            .init_resource::<CompanionPrefs>()
            .configure_sets(Update, AvatarSystems::Choose.after(drain_sync))
            .configure_sets(
                PostUpdate,
                AvatarSystems::Bounds
                    .after(TransformSystems::Propagate)
                    .after(CameraUpdateSystems)
                    .before(bevy_egui::EguiPostUpdateSet::EndPass),
            )
            .add_plugins((sprite::SpriteRendererPlugin, vrm::VrmRendererPlugin))
            .add_systems(Startup, spawn_ui_camera)
            .add_systems(
                Update,
                (choose_renderer, tick_state)
                    .chain()
                    .in_set(AvatarSystems::Choose),
            )
            // After `Update`'s commands, so a camera a renderer spawned or
            // despawned this frame counts; before rendering picks up the
            // clear colour.
            .add_systems(PostUpdate, clear_without_scene)
            .add_systems(
                EguiPrimaryContextPass,
                placeholder::draw
                    .run_if(placeholder::stands_in)
                    // In the theme the settings just applied, and before the
                    // gestures hit-test against the bounds it reports. Its
                    // area is `Order::Background` and the overlay's above
                    // it, so the overlay paints over it either way.
                    .after(crate::settings::follow)
                    .before(crate::companion::pointer_input),
            );
    }

    fn finish(&self, app: &mut App) {
        if let Some(server) = app.world().get_resource::<AssetServer>()
            && server.get_source(assets::ASSET_SOURCE).is_err()
        {
            tracing::warn!(
                "no `{}` asset source: the renderers cannot load the avatar's files \
                 (the host calls `avatar::register_asset_source` before `DefaultPlugins`)",
                assets::ASSET_SOURCE
            );
        }
    }
}

/// The GUI's camera: it draws egui over whatever the renderers drew and
/// clears nothing itself, unless [`clear_without_scene`] finds nothing to
/// draw over.
///
/// It sees no render layer ([`RenderLayers::none`]): a `Camera2d` on the
/// default layer 0 would draw every sprite on that layer a second time,
/// over the renderer's own camera. egui is not drawn as entities, so it
/// does not depend on the layers.
fn spawn_ui_camera(mut commands: Commands) {
    commands.spawn((
        Camera2d,
        Camera {
            order: UI_CAMERA_ORDER,
            clear_color: ClearColorConfig::None,
            ..default()
        },
        RenderLayers::none(),
        PrimaryEguiContext,
        UiCamera,
    ));
}

/// With no active scene camera drawing to the window (the placeholder
/// draws in egui, and a renderer swap can leave a frame between one camera
/// and the next), nothing else clears the window, so the UI camera clears
/// it with the app's `ClearColor`; otherwise it draws over the scene. A
/// camera rendering to an image or another window is no scene here.
fn clear_without_scene(
    mut ui: Single<&mut Camera, With<UiCamera>>,
    scene: Query<(&Camera, &RenderTarget), Without<UiCamera>>,
    primary: Query<Entity, With<PrimaryWindow>>,
) {
    let primary = primary.single().ok();
    let has_scene = scene
        .iter()
        .any(|(camera, target)| camera.is_active && draws_to(target, primary));
    let clears = !matches!(ui.clear_color, ClearColorConfig::None);
    if has_scene == clears {
        ui.clear_color = if has_scene {
            ClearColorConfig::None
        } else {
            ClearColorConfig::Default
        };
    }
}

/// Whether `target` is the primary window, `primary`.
fn draws_to(target: &RenderTarget, primary: Option<Entity>) -> bool {
    matches!(
        target.normalize(primary),
        Some(NormalizedRenderTarget::Window(window)) if Some(window.entity()) == primary
    )
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
    fn the_avatar_plugin_runs_without_the_companion_plugin() {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, AvatarPlugin));
        // Its `Update` systems read the prefs, which only the companion
        // plugin loaded before; it now starts from the defaults.
        app.update();
        assert!(app.world().contains_resource::<CompanionPrefs>());
        assert_eq!(
            *app.world().resource::<ActiveRenderer>(),
            ActiveRenderer::Placeholder
        );
    }

    #[test]
    fn both_renderer_plugins_are_installed() {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, AvatarPlugin));
        app.update();
        assert!(app.is_plugin_added::<sprite::SpriteRendererPlugin>());
        assert!(app.is_plugin_added::<vrm::VrmRendererPlugin>());
    }

    #[test]
    fn only_a_camera_drawing_to_the_window_counts_as_a_scene() {
        use bevy::ecs::system::RunSystemOnce as _;

        let mut world = World::new();
        world.spawn((Window::default(), PrimaryWindow));
        let ui = world
            .spawn((
                Camera {
                    clear_color: ClearColorConfig::None,
                    ..default()
                },
                UiCamera,
            ))
            .id();
        let clear = |world: &mut World| {
            world.run_system_once(clear_without_scene).unwrap();
            world.get::<Camera>(ui).unwrap().clear_color
        };

        // An off-screen camera draws nothing beneath the UI: it clears.
        world.spawn((
            Camera::default(),
            RenderTarget::Image(bevy::asset::Handle::<Image>::default().into()),
        ));
        assert!(matches!(clear(&mut world), ClearColorConfig::Default));

        // An inactive camera on the window is no scene either.
        let scene = world
            .spawn((
                Camera {
                    is_active: false,
                    ..default()
                },
                RenderTarget::default(),
            ))
            .id();
        assert!(matches!(clear(&mut world), ClearColorConfig::Default));

        // An active one is: the UI draws over it.
        world.get_mut::<Camera>(scene).unwrap().is_active = true;
        assert!(matches!(clear(&mut world), ClearColorConfig::None));
    }

    /// A text file as an asset: enough to see a cached file load.
    #[derive(Asset, TypePath)]
    struct Text(String);

    #[derive(TypePath)]
    struct TextLoader;

    impl bevy::asset::AssetLoader for TextLoader {
        type Asset = Text;
        type Settings = ();
        type Error = std::io::Error;

        async fn load(
            &self,
            reader: &mut dyn bevy::asset::io::Reader,
            _: &(),
            _: &mut bevy::asset::LoadContext<'_>,
        ) -> Result<Text, std::io::Error> {
            let mut bytes = Vec::new();
            reader.read_to_end(&mut bytes).await?;
            Ok(Text(String::from_utf8_lossy(&bytes).into_owned()))
        }

        fn extensions(&self) -> &[&str] {
            &["txt"]
        }
    }

    #[test]
    fn a_synced_file_loads_through_the_asset_server() {
        use bevy::asset::{AssetApp as _, AssetPlugin, LoadState};

        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::new(dir.path().to_path_buf());
        let manifest = sapphire_agent_core::AvatarManifest {
            config: sapphire_agent_core::AvatarConfig {
                vrm: Some(sapphire_agent_core::VrmConfig {
                    model: "Hello.TXT".into(),
                    ..Default::default()
                }),
                sprite: None,
            },
            files: vec![sapphire_agent_core::AvatarFile {
                path: "Hello.TXT".into(),
                size: 5,
                sha256: assets::sha256_hex(b"hello"),
            }],
        };
        struct One(sapphire_agent_core::AvatarManifest);
        #[async_trait::async_trait]
        impl AvatarSource for One {
            async fn manifest(&self) -> anyhow::Result<sapphire_agent_core::AvatarManifest> {
                Ok(self.0.clone())
            }
            async fn fetch(&self, _: &str) -> anyhow::Result<Vec<u8>> {
                Ok(b"hello".to_vec())
            }
        }
        let loaded = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(assets::sync(&One(manifest), &cache))
            .unwrap();

        let mut app = App::new();
        register_asset_source_at(&mut app, dir.path().to_path_buf());
        app.add_plugins((TaskPoolPlugin::default(), AssetPlugin::default()))
            .init_asset::<Text>()
            .register_asset_loader(TextLoader);

        let path = loaded.asset_path("Hello.TXT").unwrap();
        let handle: bevy::asset::Handle<Text> = app.world().resource::<AssetServer>().load(path);
        for _ in 0..500 {
            app.update();
            match app.world().resource::<AssetServer>().load_state(&handle) {
                LoadState::Loaded => break,
                LoadState::Failed(e) => panic!("the cached file failed to load: {e}"),
                _ => std::thread::sleep(Duration::from_millis(5)),
            }
        }
        let text = app
            .world()
            .resource::<Assets<Text>>()
            .get(&handle)
            .map(|t| t.0.clone());
        assert_eq!(text.as_deref(), Some("hello"));
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
