//! sapphire-agent-desktop entry point.
//!
//! Wires together:
//! - bevy app + bevy_egui plugin
//! - tokio RPC bridge ([`bridge::RpcBridge`]) as a `NonSend` resource
//!   (the bridge isn't `Sync` because it owns a `Runtime`)
//! - one drain system that pumps `BridgeEvent`s into `AppState`
//! - cpal-backed [`audio::AudioPlayer`] for server-streamed TTS PCM and
//!   [`audio::MicRecorder`] for the mic button's voice-input path
//! - one of `ui::chat::ui` / `ui::settings::ui` runs per frame
//!   depending on the current screen

mod audio;
mod bridge;
mod config;
mod state;
mod ui;

use std::sync::{Arc, Mutex};

use bevy::prelude::*;
use bevy_egui::egui::{FontData, FontDefinitions, FontFamily};
use bevy_egui::{EguiPlugin, EguiPrimaryContextPass};
use tokio::sync::mpsc::UnboundedReceiver;
use tracing::{info, warn};
use tracing_subscriber::{EnvFilter, fmt};

use audio::{AudioPlayer, MicRecorder, MicState};
use bridge::{BridgeEvent, RpcBridge};
use config::DesktopConfig;
use state::{ChatEntry, ChatRole, MicUiState, Screen, Session, TurnState};

/// All UI-driven state in one resource. Pure (no bevy types beyond
/// `Resource`) so the contents are easy to move to a shared GUI crate
/// later when mobile lands.
#[derive(Resource)]
pub struct AppState {
    pub config: DesktopConfig,
    pub screen: Screen,
    pub session: Session,
    pub turn: TurnState,
    pub mic: MicUiState,
    pub history: Vec<ChatEntry>,
    pub draft: String,
    pub last_status: Option<String>,
    /// Filled at startup by [`DesktopConfig::ensure_device_id`]. Used
    /// as the `voice/pipeline_run` routing key.
    pub device_id: String,
}

/// Wraps the bridge receiver in a `Mutex` so it can be a bevy
/// `Resource` (which requires `Send + Sync`). Only one system reads it.
#[derive(Resource)]
struct BridgeEventQueue(Mutex<UnboundedReceiver<BridgeEvent>>);

/// Optional TTS playback handle. Wrapped in `Option` because the
/// output device may be unavailable on headless boxes — we drop TTS
/// chunks silently in that case rather than failing chat too.
#[derive(Resource, Default)]
pub struct AudioState {
    pub player: Option<AudioPlayer>,
}

/// In-flight mic capture, if any. The handle owns the cpal stream +
/// VAD worker; dropping it cancels. Inserted as a NonSend resource
/// because [`MicRecorder`]'s sample receiver isn't `Sync` — only one
/// thread (the bevy main loop) ever reads from it anyway.
#[derive(Default)]
pub struct ActiveMic(pub Option<MicRecorder>);

fn main() {
    fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("warn,sapphire_call_desktop=info")),
        )
        .init();

    let (bridge, event_rx) = match RpcBridge::new() {
        Ok(pair) => pair,
        Err(e) => {
            eprintln!("failed to start tokio runtime: {e:#}");
            std::process::exit(1);
        }
    };

    let cfg = match DesktopConfig::default_path() {
        Some(p) if p.exists() => DesktopConfig::load(&p).unwrap_or_else(|e| {
            eprintln!("config load failed ({e:#}); starting with empty settings");
            DesktopConfig::default()
        }),
        _ => DesktopConfig::default(),
    };

    // Shared with the CLI satellite — the two clients aren't expected
    // to run side-by-side, and reusing the same id lets users move
    // between sapphire-agent-cli and sapphire-agent-desktop without the
    // server treating each launch as a new device.
    let device_id = match sapphire_agent_client::device_id::ensure_device_id() {
        Ok(id) => id,
        Err(e) => {
            eprintln!("failed to resolve device id: {e:#}");
            std::process::exit(1);
        }
    };

    // Open the cpal output stream once at startup so TTS playback is
    // ready the moment a chat reply arrives. A missing output device
    // is non-fatal: log + drop audio chunks silently.
    let audio_player = match AudioPlayer::start() {
        Ok(p) => Some(p),
        Err(e) => {
            warn!("audio output unavailable: {e:#}; TTS playback disabled this session");
            None
        }
    };

    let mut state = AppState {
        screen: if cfg.is_complete() {
            Screen::Chat
        } else {
            Screen::Settings
        },
        session: Session::Disconnected,
        turn: TurnState::Idle,
        mic: MicUiState::Idle,
        history: Vec::new(),
        draft: String::new(),
        last_status: None,
        config: cfg,
        device_id,
    };

    // If config is already complete, kick off the initial connection so
    // the chat screen has a session by the time the window paints.
    if state.config.is_complete()
        && let (Some(url), Some(token)) = (
            state.config.server.url.clone(),
            state.config.server.token.clone(),
        )
    {
        state.session = Session::Initializing;
        bridge.submit_initialize(
            bridge::Endpoint { base: url, token },
            state.config.server.session.clone(),
            None,
        );
    }

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
        .insert_non_send(ActiveMic::default())
        .insert_resource(AudioState {
            player: audio_player,
        })
        .insert_non_send(bridge)
        .add_systems(Startup, spawn_camera)
        .add_systems(Update, (drain_bridge_events, poll_mic_recorder))
        .add_systems(
            EguiPrimaryContextPass,
            (setup_fonts, setup_image_loaders, route_ui).chain(),
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
fn drain_bridge_events(
    mut state: ResMut<AppState>,
    queue: Res<BridgeEventQueue>,
    audio: Res<AudioState>,
) {
    let Ok(mut rx) = queue.0.lock() else {
        return;
    };
    while let Ok(evt) = rx.try_recv() {
        match evt {
            BridgeEvent::SessionReady {
                session_id,
                display_id,
            } => {
                state.session = Session::Ready {
                    session_id,
                    display_id,
                };
                state.screen = Screen::Chat;
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
            }
            BridgeEvent::ChatError { message } => {
                state.history.push(ChatEntry {
                    role: ChatRole::System,
                    text: format!("error: {message}"),
                });
                state.turn = TurnState::Idle;
            }
            BridgeEvent::TtsError { message } => {
                state.history.push(ChatEntry {
                    role: ChatRole::System,
                    text: format!("TTS: {message}"),
                });
                // Discard whatever PCM may already be queued — the
                // server stopped mid-stream, so leftover fragments
                // would play out of context.
                if let Some(p) = audio.player.as_ref() {
                    p.drain();
                }
            }
            BridgeEvent::AssistantTextPreview { .. } => {
                // The final ChatText event carries the same string; we
                // currently land the assistant message in history only
                // once, on completion. Future revisions may swap this
                // for an in-place "streaming" entry that mutates as
                // text arrives.
            }
            BridgeEvent::ToolStart { name } => {
                state.last_status = Some(format!("tool: {name}…"));
            }
            BridgeEvent::ToolEnd { name } => {
                state.last_status = Some(format!("tool: {name} done"));
            }
            BridgeEvent::AudioChunk { pcm } => {
                if let Some(p) = audio.player.as_ref() {
                    p.push_pcm_16khz(&pcm);
                }
            }
            BridgeEvent::VoiceTranscript { text } => {
                if !text.trim().is_empty() {
                    state.history.push(ChatEntry {
                        role: ChatRole::User,
                        text,
                    });
                }
            }
            BridgeEvent::VoiceAssistantText { text } => {
                if !text.trim().is_empty() {
                    state.history.push(ChatEntry {
                        role: ChatRole::Assistant,
                        text,
                    });
                }
            }
            BridgeEvent::VoiceDone { .. } => {
                state.mic = MicUiState::Idle;
                state.turn = TurnState::Idle;
            }
            BridgeEvent::VoiceError { message } => {
                state.history.push(ChatEntry {
                    role: ChatRole::System,
                    text: format!("voice: {message}"),
                });
                state.mic = MicUiState::Idle;
                state.turn = TurnState::Idle;
                if let Some(p) = audio.player.as_ref() {
                    p.drain();
                }
            }
        }
    }
}

/// Per-frame poller for the active mic capture (if any). Marries
/// `MicState` events from the audio worker thread to UI state + a
/// `voice/pipeline_run` submission once VAD auto-stops or the user
/// presses the stop button.
fn poll_mic_recorder(
    mut state: ResMut<AppState>,
    mut mic: NonSendMut<ActiveMic>,
    bridge: NonSend<RpcBridge>,
) {
    let Some(recorder) = mic.0.as_mut() else {
        return;
    };
    while let Some(evt) = recorder.poll() {
        match evt {
            MicState::Recording { speech_detected } => {
                state.mic = MicUiState::Recording { speech_detected };
            }
            MicState::Done { pcm } => {
                submit_voice(&mut state, &bridge, pcm);
            }
            MicState::Failed { message } => {
                state.history.push(ChatEntry {
                    role: ChatRole::System,
                    text: format!("mic: {message}"),
                });
                state.mic = MicUiState::Idle;
            }
        }
    }
    if recorder.is_finished() {
        mic.0 = None;
    }
}

fn submit_voice(state: &mut AppState, bridge: &RpcBridge, pcm: Vec<i16>) {
    if pcm.is_empty() {
        state.mic = MicUiState::Idle;
        return;
    }
    let (Some(url), Some(token)) = (
        state.config.server.url.clone(),
        state.config.server.token.clone(),
    ) else {
        state.mic = MicUiState::Idle;
        return;
    };
    state.mic = MicUiState::Uploading;
    state.turn = TurnState::Sending;
    bridge.submit_voice(
        bridge::Endpoint { base: url, token },
        state.device_id.clone(),
        pcm,
        None,
    );
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

fn route_ui(
    contexts: bevy_egui::EguiContexts,
    state: ResMut<AppState>,
    bridge: NonSend<RpcBridge>,
    mic: NonSendMut<ActiveMic>,
) {
    match state.screen {
        Screen::Settings => ui::settings::ui(contexts, state, bridge),
        Screen::Chat => ui::chat::ui(contexts, state, bridge, mic),
    }
}
