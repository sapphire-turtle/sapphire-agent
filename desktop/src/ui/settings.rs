//! First-run / re-config Settings panel.
//!
//! Asks for the endpoint URL + bearer token, plus the TTS opt-in. On
//! Save: persists `DesktopConfig` to disk, kicks off `initialize`
//! against the new endpoint, and switches to the Chat screen.

use bevy::prelude::*;
use bevy_egui::{EguiContexts, egui};

use crate::AppState;
use crate::bridge::{Endpoint, RpcBridge};
use crate::config::DesktopConfig;
use crate::state::{Screen, Session};

pub fn ui(mut contexts: EguiContexts, mut state: ResMut<AppState>, bridge: NonSend<RpcBridge>) {
    let Ok(ctx) = contexts.ctx_mut() else {
        return;
    };
    egui::CentralPanel::default().show(&mut super::root_ui(ctx), |ui| {
        ui.heading("sapphire-agent-desktop · Connection");
        ui.add_space(8.0);

        let url = state.config.server.url.get_or_insert_default();
        ui.label("Endpoint URL");
        ui.add(egui::TextEdit::singleline(url).hint_text("https://agent.example.com"));

        ui.add_space(4.0);
        let token = state.config.server.token.get_or_insert_default();
        ui.label("API key (bearer token)");
        ui.add(egui::TextEdit::singleline(token).password(true).hint_text(
            "minted by `sapphire-agent device add`, listed in [room_profile.*].devices",
        ));

        ui.add_space(8.0);
        ui.checkbox(&mut state.config.tts.enabled, "Speak replies (TTS)");
        ui.label(
            egui::RichText::new("Requires a voice_pipeline on the server's room_profile.")
                .small()
                .weak(),
        );

        ui.add_space(12.0);
        let complete = state.config.is_complete();
        ui.horizontal(|ui| {
            let save = ui
                .add_enabled(complete, egui::Button::new("Save & connect"))
                .clicked();
            if save {
                if let Some(path) = DesktopConfig::default_path() {
                    if let Err(e) = state.config.save(&path) {
                        state.last_status = Some(format!("save failed: {e:#}"));
                    } else {
                        state.last_status = Some(format!("saved to {}", path.display()));
                        kick_off_initialize(&mut state, &bridge);
                    }
                } else {
                    state.last_status =
                        Some("could not resolve a config directory for this platform".into());
                }
            }
            // Cancel only makes sense once we already have a live session
            // to fall back to.
            let can_back = matches!(state.session, Session::Ready { .. });
            if ui
                .add_enabled(can_back, egui::Button::new("Cancel"))
                .clicked()
            {
                state.screen = Screen::Chat;
            }
        });

        if let Some(msg) = state.last_status.as_deref() {
            ui.add_space(8.0);
            ui.label(egui::RichText::new(msg).small());
        }
        match &state.session {
            Session::Initializing => {
                ui.add_space(8.0);
                ui.label("Connecting…");
            }
            Session::Failed { message } => {
                ui.add_space(8.0);
                ui.colored_label(
                    egui::Color32::from_rgb(200, 80, 80),
                    format!("connection failed: {message}"),
                );
            }
            _ => {}
        }
    });
}

fn kick_off_initialize(state: &mut AppState, bridge: &RpcBridge) {
    let (Some(url), Some(token)) = (
        state.config.server.url.clone(),
        state.config.server.token.clone(),
    ) else {
        return;
    };
    state.session = Session::Initializing;
    bridge.submit_initialize(
        Endpoint { base: url, token },
        state.config.server.session.clone(),
        None,
    );
}
