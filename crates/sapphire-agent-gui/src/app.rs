//! What the GUI needs from its host app beyond the screens: the tokio
//! runtime, the waker, the bundled font and the image loaders.

use std::sync::Arc;

use bevy::prelude::*;
use bevy::winit::{EventLoopProxyWrapper, WinitUserEvent};
use bevy_egui::EguiContexts;
use bevy_egui::egui::{FontData, FontDefinitions, FontFamily};
use tokio::runtime::Runtime;
use tracing::{info, warn};

/// Wakes the GUI from another thread so it draws a frame even while idle.
/// Bevy runs reactively (`WinitSettings::desktop_app`), so without this a
/// reply that arrives while nobody touches the window would wait for the
/// next input event or timeout.
pub type Waker = Arc<dyn Fn() + Send + Sync>;

/// The [`Waker`] as a resource, for anything else that produces work off
/// the main thread.
#[derive(Resource, Clone)]
pub struct GuiWaker(pub Waker);

impl GuiWaker {
    /// Wakes the GUI once `after` has passed, timed on `runtime`: an idle
    /// window draws no frame on its own to notice a deadline.
    pub fn wake_in(&self, runtime: &GuiRuntime, after: std::time::Duration) {
        let waker = self.0.clone();
        runtime.0.spawn(async move {
            tokio::time::sleep(after).await;
            waker();
        });
    }
}

/// The tokio runtime the agent client runs on. Bevy's scheduler is sync and
/// the client is async, so the GUI keeps its own. On `AppExit` it is shut
/// down with `shutdown_background`: a blocking drop could hold the window
/// open behind an in-flight turn. Don't keep clones of the `Arc` past
/// `AppExit` (hold a `Handle` instead), or shutdown falls back to leaking.
#[derive(Resource)]
pub struct GuiRuntime(pub Arc<Runtime>);

pub(crate) fn build_runtime() -> Arc<Runtime> {
    // Multi-threaded so an in-flight turn doesn't hold up a reconnect on
    // the same thread.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("sapphire-agent-gui-rt")
        .build()
        .expect("failed to start the GUI's tokio runtime");
    Arc::new(runtime)
}

/// A [`Waker`] that sends winit's wake-up event, which bevy's reactive
/// update modes answer with a frame. Without winit (a headless app) there
/// is no loop to wake, and frames come on their own.
pub(crate) fn event_loop_waker(world: &World) -> Waker {
    match world.get_resource::<EventLoopProxyWrapper>() {
        Some(proxy) => {
            let proxy = (**proxy).clone();
            Arc::new(move || {
                let _ = proxy.send_event(WinitUserEvent::WakeUp);
            })
        }
        None => {
            warn!("no winit event loop to wake; the GUI relies on frames coming on their own");
            Arc::new(|| {})
        }
    }
}

/// Shuts the runtime down without waiting for its tasks once the app is
/// exiting, so closing the window mid-turn exits at once.
pub(crate) fn shutdown_runtime(mut exits: MessageReader<AppExit>, mut commands: Commands) {
    if exits.read().next().is_none() {
        return;
    }
    commands.queue(|world: &mut World| {
        let Some(GuiRuntime(runtime)) = world.remove_resource::<GuiRuntime>() else {
            return;
        };
        match Arc::try_unwrap(runtime) {
            Ok(runtime) => runtime.shutdown_background(),
            // Someone still holds the runtime: leak it rather than block
            // on its drop. The process is on its way out.
            Err(runtime) => {
                warn!("the GUI runtime is still shared at exit; leaving it to the OS");
                std::mem::forget(runtime);
            }
        }
    });
}

/// Bundled Noto Sans JP Regular (~4 MB, SIL OFL — see
/// `assets/LICENSE-OFL.txt`). Bundling instead of probing system fonts
/// keeps the app binary self-contained: no extra package install step on
/// fresh machines, identical glyph rendering across hosts.
///
/// "Subset" here means the JP-only cut of the Noto CJK family rather
/// than per-glyph subsetting — the chat content is user-typed Japanese
/// of unknown shape, so we need the full JP block, not a fixed glyph
/// list. Pan-CJK (~20 MB) is overkill for this client; if KR/SC/TC
/// support is ever needed we can swap to NotoSansCJK-Regular.ttc.
const BUNDLED_CJK_FONT: &[u8] = include_bytes!("../assets/NotoSansJP-Regular.otf");

/// Register the bundled JP font as a fallback so kana / kanji don't
/// render as tofu. Appended last in each family so Latin glyphs keep
/// using egui's bundled font (better hinting) and only missing-glyph
/// lookups fall through to Noto.
///
/// Runs in `EguiPrimaryContextPass` (not `Startup`) because bevy_egui
/// only attaches `EguiContext` to the camera during `PreUpdate` of the
/// first frame — at `Startup` time `ctx_mut()` errors with `NoEntities`
/// and the font registration is silently skipped, leaving JP as tofu.
/// The `Local<bool>` guard makes it a one-shot.
pub(crate) fn setup_fonts(mut contexts: EguiContexts, mut done: Local<bool>) {
    if *done {
        return;
    }
    let Ok(ctx) = contexts.ctx_mut() else {
        warn!("egui primary context unavailable; skipping font setup");
        return;
    };

    let mut fonts = FontDefinitions::default();
    fonts.font_data.insert(
        "cjk".to_owned(),
        Arc::new(FontData::from_static(BUNDLED_CJK_FONT)),
    );
    if let Some(family) = fonts.families.get_mut(&FontFamily::Proportional) {
        family.push("cjk".to_owned());
    }
    if let Some(family) = fonts.families.get_mut(&FontFamily::Monospace) {
        family.push("cjk".to_owned());
    }
    ctx.set_fonts(fonts);
    info!(
        "loaded bundled Noto Sans JP Regular ({} KB)",
        BUNDLED_CJK_FONT.len() / 1024
    );
    *done = true;
}

/// Register egui image loaders so `egui::Image::new(include_image!(…))`
/// can decode the bundled Lucide SVGs. Runs in `EguiPrimaryContextPass`
/// (same reason as `setup_fonts`) and guards itself with a `Local<bool>`
/// so it only runs once.
pub(crate) fn setup_image_loaders(mut contexts: EguiContexts, mut done: Local<bool>) {
    if *done {
        return;
    }
    let Ok(ctx) = contexts.ctx_mut() else {
        return;
    };
    egui_extras::install_image_loaders(ctx);
    *done = true;
}
