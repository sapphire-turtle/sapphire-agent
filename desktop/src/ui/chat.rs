//! Chat panel: scrolling history + input row.

use bevy::prelude::*;
use bevy_egui::{EguiContexts, egui};

use crate::audio::MicRecorder;
use crate::bridge::{Endpoint, RpcBridge};
use crate::state::{ChatEntry, ChatRole, MicUiState, Screen, Session, TurnState};
use crate::{ActiveMic, AppState};

const ICON_SETTINGS: egui::ImageSource<'_> =
    egui::include_image!("../../assets/icons/settings.svg");
const ICON_MIC: egui::ImageSource<'_> = egui::include_image!("../../assets/icons/mic.svg");
const ICON_STOP: egui::ImageSource<'_> = egui::include_image!("../../assets/icons/square.svg");
const ICON_SEND: egui::ImageSource<'_> = egui::include_image!("../../assets/icons/send.svg");

/// Pixel size we hand to the SVG loader. resvg rasterises once at this
/// resolution and caches the texture, so picking a larger value than
/// the on-screen footprint avoids visible aliasing.
const ICON_PX: f32 = 18.0;

fn icon_image(src: egui::ImageSource<'_>) -> egui::Image<'_> {
    egui::Image::new(src)
        .fit_to_exact_size(egui::vec2(ICON_PX, ICON_PX))
        .tint(egui::Color32::from_gray(220))
}

pub fn ui(
    mut contexts: EguiContexts,
    mut state: ResMut<AppState>,
    bridge: NonSend<RpcBridge>,
    mut mic: NonSendMut<ActiveMic>,
) {
    let Ok(ctx) = contexts.ctx_mut() else {
        return;
    };
    let root = &mut super::root_ui(ctx);
    egui::Panel::top("chat_topbar").show(root, |ui| {
        ui.horizontal(|ui| {
            ui.heading("sapphire-agent-cli");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .add(egui::Button::image(icon_image(ICON_SETTINGS)))
                    .on_hover_text("Settings")
                    .clicked()
                {
                    state.screen = Screen::Settings;
                }
                let session_label = match &state.session {
                    Session::Ready { display_id, .. } => format!("session: {display_id}"),
                    Session::Initializing => "connecting…".to_string(),
                    Session::Failed { .. } => "connection failed".to_string(),
                    Session::Disconnected => "disconnected".to_string(),
                };
                ui.label(egui::RichText::new(session_label).small().weak());
            });
        });
    });

    egui::Panel::bottom("chat_input")
        .min_size(60.0)
        .show(root, |ui| {
            let session_ready = matches!(state.session, Session::Ready { .. });
            let can_send = session_ready && state.turn == TurnState::Idle;
            let recording = matches!(state.mic, MicUiState::Recording { .. });
            ui.horizontal(|ui| {
                let resp = ui.add_sized(
                    [ui.available_width() - 90.0, 28.0],
                    egui::TextEdit::singleline(&mut state.draft).hint_text("Type a message…"),
                );
                let enter_pressed =
                    resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));

                // Mic toggle: click to start, click again to stop early.
                // VAD also auto-stops on silence — see `poll_mic_recorder`
                // in main.rs. Disabled while a chat turn is mid-flight so
                // we don't race two utterances against the same session.
                let mic_enabled = session_ready && state.turn == TurnState::Idle;
                let mic_icon = if recording { ICON_STOP } else { ICON_MIC };
                let mic_tint = if recording {
                    egui::Color32::from_rgb(220, 90, 90)
                } else {
                    egui::Color32::from_gray(220)
                };
                let mic_btn = ui.add_enabled(
                    mic_enabled || recording,
                    egui::Button::image(
                        egui::Image::new(mic_icon)
                            .fit_to_exact_size(egui::vec2(ICON_PX, ICON_PX))
                            .tint(mic_tint),
                    ),
                );
                let mic_btn = if recording {
                    mic_btn.on_hover_text("Stop recording")
                } else {
                    mic_btn.on_hover_text("Hold to record (auto-stops on silence)")
                };
                if mic_btn.clicked() {
                    if recording {
                        if let Some(rec) = mic.0.as_ref() {
                            rec.request_stop();
                        }
                    } else if mic.0.is_none() {
                        mic.0 = Some(MicRecorder::start());
                        state.mic = MicUiState::Recording {
                            speech_detected: false,
                        };
                    }
                }

                let send_btn = ui.add_enabled(
                    can_send,
                    egui::Button::image(icon_image(ICON_SEND)),
                );
                let send_clicked = send_btn.on_hover_text("Send").clicked();
                if (send_clicked || (enter_pressed && can_send)) && !state.draft.trim().is_empty()
                {
                    submit_chat(&mut state, &bridge);
                }
            });

            match state.mic {
                MicUiState::Recording {
                    speech_detected: false,
                } => {
                    ui.label(
                        egui::RichText::new("listening — speak now (silence will auto-stop)")
                            .small()
                            .weak(),
                    );
                }
                MicUiState::Recording {
                    speech_detected: true,
                } => {
                    ui.label(
                        egui::RichText::new("recording…")
                            .small()
                            .color(egui::Color32::from_rgb(220, 90, 90)),
                    );
                }
                MicUiState::Uploading => {
                    ui.label(egui::RichText::new("uploading voice…").small().weak());
                }
                MicUiState::Idle => {
                    if !session_ready {
                        ui.label(
                            egui::RichText::new(
                                "Waiting for the connection to come up — open Settings if it stalls.",
                            )
                            .small()
                            .weak(),
                        );
                    }
                }
            }
        });

    egui::CentralPanel::default().show(root, |ui| {
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .stick_to_bottom(true)
            .show(ui, |ui| {
                for entry in &state.history {
                    render_entry(ui, entry);
                    ui.add_space(6.0);
                }
                if state.turn == TurnState::Sending {
                    ui.label(egui::RichText::new("…thinking").italics().weak());
                }
            });
    });
}

fn render_entry(ui: &mut egui::Ui, entry: &ChatEntry) {
    let (prefix, color) = match entry.role {
        ChatRole::User => ("You", egui::Color32::from_rgb(120, 170, 230)),
        ChatRole::Assistant => ("Saphina", egui::Color32::from_rgb(180, 220, 180)),
        ChatRole::System => ("system", egui::Color32::GRAY),
    };
    ui.horizontal_wrapped(|ui| {
        ui.label(egui::RichText::new(prefix).strong().color(color));
        ui.label(&entry.text);
    });
}

fn submit_chat(state: &mut AppState, bridge: &RpcBridge) {
    let Session::Ready { session_id, .. } = &state.session else {
        return;
    };
    let (Some(url), Some(token)) = (
        state.config.server.url.clone(),
        state.config.server.token.clone(),
    ) else {
        return;
    };
    let content = std::mem::take(&mut state.draft);
    state.history.push(ChatEntry {
        role: ChatRole::User,
        text: content.clone(),
    });
    state.turn = TurnState::Sending;
    bridge.submit_chat(
        Endpoint { base: url, token },
        session_id.clone(),
        content,
        state.config.tts.enabled,
    );
}
