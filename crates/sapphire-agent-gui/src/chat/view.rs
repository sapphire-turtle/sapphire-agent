//! Chat screen: scrolling history + input row, or the framework's
//! settings in their place.

use bevy::prelude::*;
use bevy_egui::{EguiContexts, egui};
use sapphire_agent_core::ClientCaps;

use super::{ChatBridge, ChatEntry, ChatRole, ChatState, Session, TurnState};
use crate::avatar::{AvatarState, DeclaredAvatar};
use crate::settings::{GuiSettings, Layout, Screen, Toggle};

const ICON_SEND: egui::ImageSource<'_> = egui::include_image!("../../assets/icons/send.svg");

/// Pixel size we hand to the SVG loader. resvg rasterises once at this
/// resolution and caches the texture, so picking a larger value than
/// the on-screen footprint avoids visible aliasing.
const ICON_PX: f32 = 18.0;

fn icon_image<'a>(ui: &egui::Ui, src: egui::ImageSource<'a>) -> egui::Image<'a> {
    egui::Image::new(src)
        .fit_to_exact_size(egui::vec2(ICON_PX, ICON_PX))
        .tint(ui.visuals().text_color())
}

/// The avatar as the chat sees it: what to declare with a turn, and the
/// state a new turn holds. `state` stays a `Mut` so the frame marks it
/// changed only when a turn actually starts.
struct Avatar<'a> {
    declared: &'a DeclaredAvatar,
    state: Mut<'a, AvatarState>,
}

pub(crate) fn ui(
    mut contexts: EguiContexts,
    mut state: ResMut<ChatState>,
    bridge: Res<ChatBridge>,
    declared: Res<DeclaredAvatar>,
    mut avatar: ResMut<AvatarState>,
    mut settings: Option<NonSendMut<GuiSettings>>,
) {
    let Ok(ctx) = contexts.ctx_mut() else {
        return;
    };
    let settings = settings.as_deref_mut();
    let avatar = Avatar {
        declared: &declared,
        state: avatar.reborrow(),
    };
    // The chat gets a full-window area of its own rather than the root
    // `Ui`: bevy_egui runs this pass inside `Context::run_ui`, which holds
    // the root `Ui` for the whole pass. `Order::Middle` keeps it above the
    // avatar's placeholder, which paints in `Order::Background`.
    let screen = ctx.content_rect();
    egui::Area::new(egui::Id::unique("chat"))
        .order(egui::Order::Middle)
        .fixed_pos(screen.min)
        .movable(false)
        .show(ctx, |ui| {
            // The area's own size is last frame's; the panels follow the
            // window as it is now.
            ui.scope_builder(egui::UiBuilder::new().max_rect(screen), |ui| {
                show(ui, &mut state, &bridge, avatar, settings);
            });
        });
}

fn show(
    root: &mut egui::Ui,
    state: &mut ChatState,
    bridge: &ChatBridge,
    mut avatar: Avatar<'_>,
    mut settings: Option<&mut GuiSettings>,
) {
    let layout = Layout::of(settings.as_ref().map(|s| (s.open, s.settings.health())));
    egui::Panel::top("chat_topbar").show(root, |ui| {
        ui.horizontal(|ui| {
            ui.heading("sapphire-agent-desktop");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if let (Some(toggle), Some(settings)) = (layout.toggle, settings.as_deref_mut()) {
                    let label = match toggle {
                        Toggle::OpenSettings => "Settings",
                        Toggle::BackToChat => "Back to chat",
                    };
                    if ui.button(label).clicked() {
                        settings.open = !settings.open;
                        // This frame's layout is already decided; the
                        // next one shows the other screen.
                        ui.ctx().request_repaint();
                    }
                }
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
            ui.colored_label(ui.visuals().error_fg_color, message);
        }
        if layout.banner
            && let Some(settings) = settings.as_deref_mut()
        {
            settings.settings.status_ui(ui);
        }
    });

    if layout.screen == Screen::Settings
        && let Some(settings) = settings
    {
        egui::CentralPanel::default().show(root, |ui| settings.settings.ui(ui));
        return;
    }

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

                let send_btn =
                    ui.add_enabled(can_send, egui::Button::image(icon_image(ui, ICON_SEND)));
                let send_clicked = send_btn.on_hover_text("Send").clicked();
                // `take_submission` holds the rest of the rule: a ready,
                // idle session and a draft that isn't blank.
                if (send_clicked || enter_pressed)
                    && let Some(text) = state.take_submission()
                {
                    avatar.state.turn_started();
                    let caps = ClientCaps {
                        avatar: avatar.declared.0.clone(),
                    };
                    bridge.submit_chat(text, caps);
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
    let visuals = ui.visuals().clone();
    let (prefix, color) = match entry.role {
        // From the active visuals, so light and dark both read: the link
        // colour, the strong text colour and the weak one stay apart.
        ChatRole::User => ("You", visuals.hyperlink_color),
        ChatRole::Assistant => ("Saphina", visuals.strong_text_color()),
        ChatRole::System => ("system", visuals.weak_text_color()),
    };
    ui.horizontal_wrapped(|ui| {
        ui.label(egui::RichText::new(prefix).strong().color(color));
        ui.label(&entry.text);
    });
}
