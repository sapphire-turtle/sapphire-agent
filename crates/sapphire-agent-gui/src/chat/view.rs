//! Chat screen: scrolling history + input row.

use bevy::prelude::*;
use bevy_egui::{EguiContexts, egui};

use super::{ChatBridge, ChatEntry, ChatRole, ChatState, Session, TurnState};

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

/// The `Ui` a screen lays its panels out in: the whole window, on the
/// background layer.
///
/// egui 0.36+ shows panels inside a `Ui` rather than straight on the
/// `Context`, and bevy_egui hands systems only the context, so each frame
/// starts by making the root one — what egui's own integrations do.
fn root_ui(ctx: &egui::Context) -> egui::Ui {
    egui::Ui::new(
        ctx.clone(),
        egui::Id::unique("root"),
        egui::UiBuilder::new()
            .layer_id(egui::LayerId::background())
            .max_rect(ctx.content_rect()),
    )
}

pub(crate) fn ui(
    mut contexts: EguiContexts,
    mut state: ResMut<ChatState>,
    bridge: Res<ChatBridge>,
) {
    let Ok(ctx) = contexts.ctx_mut() else {
        return;
    };
    let root = &mut root_ui(ctx);
    egui::Panel::top("chat_topbar").show(root, |ui| {
        ui.horizontal(|ui| {
            ui.heading("sapphire-agent-desktop");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let failed = matches!(state.session, Session::Failed { .. });
                if failed
                    && ui
                        .button("Reconnect")
                        .on_hover_text("Connect to the agent running on this host again")
                        .clicked()
                {
                    state.session = Session::Connecting;
                    bridge.submit_connect();
                }
                let session_label = match &state.session {
                    Session::Ready { display_id } => format!("session: {display_id}"),
                    Session::Connecting => "connecting…".to_string(),
                    Session::Failed { .. } => "not connected".to_string(),
                };
                ui.label(egui::RichText::new(session_label).small().weak());
            });
        });
        if let Session::Failed { message } = &state.session {
            ui.colored_label(egui::Color32::from_rgb(200, 80, 80), message);
        }
    });

    egui::Panel::bottom("chat_input")
        .min_size(60.0)
        .show(root, |ui| {
            let session_ready = matches!(state.session, Session::Ready { .. });
            let can_send = session_ready && state.turn == TurnState::Idle;
            ui.horizontal(|ui| {
                let resp = ui.add_sized(
                    [ui.available_width() - 50.0, 28.0],
                    egui::TextEdit::singleline(&mut state.draft).hint_text("Type a message…"),
                );
                let enter_pressed =
                    resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));

                let send_btn = ui.add_enabled(can_send, egui::Button::image(icon_image(ICON_SEND)));
                let send_clicked = send_btn.on_hover_text("Send").clicked();
                // `take_submission` holds the rest of the rule: a ready,
                // idle session and a draft that isn't blank.
                if (send_clicked || enter_pressed)
                    && let Some(text) = state.take_submission()
                {
                    bridge.submit_chat(text);
                }
            });
            if let Some(status) = state.last_status.as_deref() {
                ui.label(egui::RichText::new(status).small().weak());
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
