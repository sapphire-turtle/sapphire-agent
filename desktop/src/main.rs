//! sapphire-agent-desktop entry point.
//!
//! Wires together:
//! - bevy app + bevy_egui plugin
//! - tokio bridge ([`bridge::RpcBridge`]) to the agent running on this
//!   host, over its local IPC endpoint, as a `NonSend` resource (the
//!   bridge isn't `Sync` because it owns a `Runtime`)
//! - one drain system that pumps `BridgeEvent`s into `AppState`
//! - `ui::chat::ui` per frame

mod bridge;
mod state;
mod ui;

use std::sync::{Arc, Mutex};

use bevy::prelude::*;
use bevy_egui::egui::{FontData, FontDefinitions, FontFamily};
use bevy_egui::{EguiPlugin, EguiPrimaryContextPass};
use tokio::sync::mpsc::UnboundedReceiver;
use tracing::{info, warn};
use tracing_subscriber::{EnvFilter, fmt};

use bridge::{BridgeEvent, RpcBridge};
use state::{ChatEntry, ChatRole, Session, TurnState};

/// All UI-driven state in one resource. Pure (no bevy types beyond
/// `Resource`) so the contents are easy to move to a shared GUI crate
/// later when mobile lands.
#[derive(Resource)]
pub struct AppState {
    pub session: Session,
    pub turn: TurnState,
    pub history: Vec<ChatEntry>,
    pub draft: String,
    pub last_status: Option<String>,
}

/// Wraps the bridge receiver in a `Mutex` so it can be a bevy
/// `Resource` (which requires `Send + Sync`). Only one system reads it.
#[derive(Resource)]
struct BridgeEventQueue(Mutex<UnboundedReceiver<BridgeEvent>>);

fn main() {
    fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("warn,sapphire_agent_desktop=info")),
        )
        .init();

    let (bridge, event_rx) = match RpcBridge::new() {
        Ok(pair) => pair,
        Err(e) => {
            eprintln!("failed to start tokio runtime: {e:#}");
            std::process::exit(1);
        }
    };

    // Connect right away so the chat screen has a session by the time the
    // window paints.
    bridge.submit_connect();
    let state = AppState {
        session: Session::Connecting,
        turn: TurnState::Idle,
        history: Vec::new(),
        draft: String::new(),
        last_status: None,
    };

    App::new()
        .add_plugins(DefaultPlugins.set(WindowPlugin {
            primary_window: Some(Window {
                title: "sapphire-agent-desktop".to_string(),
                resolution: (640u32, 720u32).into(),
                ..default()
            }),
            ..default()
        }))
        .add_plugins(EguiPlugin::default())
        .insert_resource(state)
        .insert_resource(BridgeEventQueue(Mutex::new(event_rx)))
        .insert_non_send(bridge)
        .add_systems(Startup, spawn_camera)
        .add_systems(Update, drain_bridge_events)
        .add_systems(
            EguiPrimaryContextPass,
            (setup_fonts, setup_image_loaders, ui::chat::ui).chain(),
        )
        .run();
}

/// bevy_egui renders onto a camera-bound surface — without an active
/// camera the window paints black and no UI shows up. A plain `Camera2d`
/// is enough until we introduce the 3D avatar (swap for Camera3d then).
fn spawn_camera(mut commands: Commands) {
    commands.spawn(Camera2d);
}

/// Bundled Noto Sans JP Regular (~4 MB, SIL OFL — see
/// `assets/LICENSE-OFL.txt`). Bundling instead of probing system fonts
/// keeps the desktop binary self-contained: no extra package install
/// step on fresh machines, identical glyph rendering across hosts.
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
fn setup_fonts(mut contexts: bevy_egui::EguiContexts, mut done: Local<bool>) {
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

/// Drains every event the tokio side has produced this frame and
/// folds it into `AppState`. Idempotent if the queue is empty.
fn drain_bridge_events(mut state: ResMut<AppState>, queue: Res<BridgeEventQueue>) {
    let Ok(mut rx) = queue.0.lock() else {
        return;
    };
    while let Ok(evt) = rx.try_recv() {
        match evt {
            BridgeEvent::SessionReady { display_id } => {
                state.session = Session::Ready { display_id };
            }
            BridgeEvent::SessionFailed { message } => {
                state.session = Session::Failed { message };
            }
            BridgeEvent::ChatText { text } => {
                state.history.push(ChatEntry {
                    role: ChatRole::Assistant,
                    text,
                });
                state.turn = TurnState::Idle;
                state.last_status = None;
            }
            BridgeEvent::ChatError { message } => {
                state.history.push(ChatEntry {
                    role: ChatRole::System,
                    text: format!("error: {message}"),
                });
                state.turn = TurnState::Idle;
                state.last_status = None;
            }
            BridgeEvent::ToolStart { name } => {
                state.last_status = Some(format!("tool: {name}…"));
            }
            BridgeEvent::ToolEnd { name } => {
                state.last_status = Some(format!("tool: {name} done"));
            }
        }
    }
}

/// Register egui image loaders so `egui::Image::new(include_image!(…))`
/// can decode the bundled Lucide SVGs. Runs in `EguiPrimaryContextPass`
/// (same reason as `setup_fonts`) and guards itself with a `Local<bool>`
/// so it only runs once.
fn setup_image_loaders(mut contexts: bevy_egui::EguiContexts, mut done: Local<bool>) {
    if *done {
        return;
    }
    let Ok(ctx) = contexts.ctx_mut() else {
        return;
    };
    egui_extras::install_image_loaders(ctx);
    *done = true;
}
