//! The overlay's egui: the bubble above the character, the input box at
//! its lower half, the history over it, the menu at the click and, with
//! [`CompanionPrefs::show_buttons`], the gesture buttons beside it. Or the
//! settings, filling the window, while they are open.
//!
//! Each part is an egui area of its own above `Order::Background`, so the
//! renderer's avatar shows through everywhere else.

use bevy::ecs::system::SystemParam;
use bevy::prelude::*;
use bevy_egui::{EguiContexts, egui};
use sapphire_agent_core::ClientCaps;

use super::bubble::{self, Absence, BubbleContent};
use super::{CompanionPrefs, CompanionRequest, Gesture, Overlay, OverlayInput, Speech};
use crate::avatar::{
    ActiveRenderer, AvatarAssets, AvatarBounds, AvatarState, DeclaredAvatar, RendererError,
};
use crate::chat::{ChatBridge, ChatEntry, ChatRole, ChatState, Session, TurnState};
use crate::settings::{GuiSettings, Layout, Screen};

const ICON_SEND: egui::ImageSource<'_> = egui::include_image!("../../assets/icons/send.svg");

/// Pixel size we hand to the SVG loader. resvg rasterises once at this
/// resolution and caches the texture, so picking a larger value than
/// the on-screen footprint avoids visible aliasing.
const ICON_PX: f32 = 18.0;

/// Room between the character and the parts around it, in points.
const GAP: f32 = 8.0;
/// The bubble's and the input box's widest.
const BUBBLE_WIDTH: f32 = 300.0;
const INPUT_WIDTH: f32 = 280.0;
/// The history's smallest size, in points.
const HISTORY_MIN: egui::Vec2 = egui::vec2(320.0, 200.0);
/// How much of the theme's window colour shows through the history.
const HISTORY_OPACITY: f32 = 0.85;

fn icon_image<'a>(ui: &egui::Ui, src: egui::ImageSource<'a>) -> egui::Image<'a> {
    egui::Image::new(src)
        .fit_to_exact_size(egui::vec2(ICON_PX, ICON_PX))
        .tint(ui.visuals().text_color())
}

/// The avatar as the overlay sees it: where it is, who draws it and what
/// to declare with a turn. `state` stays a `Mut` so the frame marks it
/// changed only when a turn actually starts: renderers react to a change.
#[derive(SystemParam)]
pub(crate) struct Avatar<'w> {
    declared: Res<'w, DeclaredAvatar>,
    state: ResMut<'w, AvatarState>,
    bounds: Res<'w, AvatarBounds>,
    active: Res<'w, ActiveRenderer>,
    error: Res<'w, RendererError>,
    assets: Res<'w, AvatarAssets>,
}

/// The overlay's own state, beyond what [`Overlay`] holds.
#[derive(SystemParam)]
pub(crate) struct Parts<'w> {
    overlay: ResMut<'w, Overlay>,
    input: ResMut<'w, OverlayInput>,
    speech: Res<'w, Speech>,
    prefs: Res<'w, CompanionPrefs>,
    time: Res<'w, Time<Real>>,
}

/// Draws the overlay, or the settings while they are open.
// A bevy system: each argument is a resource it reads or writes.
#[allow(clippy::too_many_arguments)]
pub(crate) fn ui(
    mut contexts: EguiContexts,
    mut chat: ResMut<ChatState>,
    bridge: Res<ChatBridge>,
    mut avatar: Avatar,
    mut parts: Parts,
    mut requests: MessageWriter<CompanionRequest>,
    mut settings: Option<NonSendMut<GuiSettings>>,
    mut input_was_open: Local<bool>,
) {
    let Ok(ctx) = contexts.ctx_mut() else {
        return;
    };
    let mut settings = settings.as_deref_mut();
    let layout = Layout::of(settings.as_ref().map(|s| (s.open, s.settings.health())));
    if layout.screen == Screen::Settings
        && let Some(settings) = settings
    {
        settings_screen(ctx, settings);
        return;
    }

    let screen = ctx.content_rect();
    let body = avatar.bounds.0.map(|b| {
        egui::Rect::from_min_max(egui::pos2(b.min.x, b.min.y), egui::pos2(b.max.x, b.max.y))
    });
    let mut next = *parts.overlay;
    let failed = match &chat.session {
        Session::Failed { message } => Some(message.clone()),
        _ => None,
    };

    // The bubble, with the services' banner above it on a problem.
    let content = bubble::content(
        chat.turn == TurnState::Sending,
        parts.speech.bubble.as_ref(),
        parts.time.elapsed(),
    );
    let absence = bubble::absence(
        *avatar.active,
        avatar.error.0.as_deref(),
        avatar.assets.loaded.is_some(),
        avatar.assets.error.as_deref(),
    )
    .filter(|_| chat.turn == TurnState::Idle && content.is_none());
    if content.is_some() || absence.is_some() || failed.is_some() || layout.banner {
        let (at, pivot) = match body {
            Some(body) => (
                egui::pos2(body.center().x, body.top() - GAP),
                egui::Align2::CENTER_BOTTOM,
            ),
            None => (
                egui::pos2(screen.center().x, screen.top() + GAP),
                egui::Align2::CENTER_TOP,
            ),
        };
        egui::Area::new(egui::Id::unique("companion_bubble"))
            .order(egui::Order::Middle)
            .pivot(pivot)
            .fixed_pos(at)
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.set_max_width(BUBBLE_WIDTH);
                    if layout.banner
                        && let Some(settings) = settings.as_mut()
                    {
                        settings.settings.status_ui(ui);
                        ui.separator();
                    }
                    if let Some(message) = &failed {
                        ui.colored_label(ui.visuals().error_fg_color, "Not connected to the agent");
                        ui.label(egui::RichText::new(message).small().weak());
                    }
                    match &content {
                        Some(BubbleContent::Thinking) => {
                            ui.label(egui::RichText::new("…thinking").italics().weak());
                            if let Some(status) = chat.last_status.as_deref() {
                                ui.label(egui::RichText::new(status).small().weak());
                            }
                        }
                        Some(BubbleContent::Reply(text)) => {
                            ui.label(text);
                        }
                        None => {}
                    }
                    if let Some(absence) = &absence {
                        let (headline, reason) = match absence {
                            Absence::NotSetUp => ("No character is set up yet", None),
                            Absence::NotFetched(e) => {
                                ("Cannot fetch the character from the agent", Some(e))
                            }
                            Absence::CannotDraw(e) => ("Cannot draw the character", Some(e)),
                        };
                        ui.label(headline);
                        if let Some(reason) = reason {
                            ui.label(
                                egui::RichText::new(reason)
                                    .small()
                                    .color(ui.visuals().error_fg_color),
                            );
                        }
                        if let Some(settings) = settings.as_mut()
                            && ui.button("Character settings").clicked()
                        {
                            // Settings can't be told to open on a page (only About), so
                            // the Character page is one click away.
                            settings.open = true;
                            ui.ctx().request_repaint();
                        }
                    }
                });
            });
    }

    // The input box, at the character's lower half.
    if next.input {
        let at = match body {
            Some(body) => egui::pos2(body.center().x, body.top() + body.height() * 0.6),
            None => screen.center(),
        };
        let can_send =
            matches!(chat.session, Session::Ready { .. }) && chat.turn == TurnState::Idle;
        egui::Area::new(egui::Id::unique("companion_input"))
            .order(egui::Order::Middle)
            .pivot(egui::Align2::CENTER_TOP)
            .fixed_pos(at)
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.horizontal(|ui| {
                        let edit = ui.add(
                            egui::TextEdit::singleline(&mut chat.draft)
                                .desired_width(INPUT_WIDTH)
                                .hint_text("Type a message…"),
                        );
                        if !*input_was_open {
                            edit.request_focus();
                        }
                        let enter_pressed =
                            edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                        let send = ui
                            .add_enabled(can_send, egui::Button::image(icon_image(ui, ICON_SEND)))
                            .on_hover_text("Send");
                        // `take_submission` holds the rest of the rule: a
                        // ready, idle session and a draft that isn't blank.
                        if (send.clicked() || enter_pressed)
                            && let Some(text) = chat.take_submission()
                        {
                            avatar.state.turn_started();
                            let caps = ClientCaps {
                                avatar: avatar.declared.0.clone(),
                            };
                            bridge.submit_chat(text, caps);
                            // Keep typing: the input stays open for the next line.
                            edit.request_focus();
                        }
                    });
                });
            });
    }
    *input_was_open = next.input;

    // The history, over the character.
    if next.history {
        let size = body
            .map_or(HISTORY_MIN, |b| egui::vec2(b.width(), b.height() * 0.7))
            .max(HISTORY_MIN)
            .min(screen.size() - egui::vec2(2.0 * GAP, 2.0 * GAP));
        let center = body.map_or(screen.center(), |b| b.center());
        let rect = egui::Rect::from_center_size(center, size);
        let area = egui::Area::new(egui::Id::unique("companion_history"))
            .order(egui::Order::Middle)
            .fixed_pos(rect.min)
            .show(ctx, |ui| {
                let fill = ui.visuals().window_fill().gamma_multiply(HISTORY_OPACITY);
                egui::Frame::window(ui.style()).fill(fill).show(ui, |ui| {
                    ui.set_width(size.x - 2.0 * GAP);
                    ui.set_height(size.y - 2.0 * GAP);
                    egui::ScrollArea::vertical()
                        .auto_shrink([false, false])
                        .stick_to_bottom(true)
                        .show(ui, |ui| {
                            for entry in &chat.history {
                                render_entry(ui, entry);
                                ui.add_space(6.0);
                            }
                            if chat.turn == TurnState::Sending {
                                ui.label(egui::RichText::new("…thinking").italics().weak());
                            }
                        });
                });
            });
        if area.response.clicked_elsewhere() && !parts.input.opened_now {
            next.history = false;
        }
    }

    // The menu, at the click.
    if next.menu {
        let area = egui::Area::new(egui::Id::unique("companion_menu"))
            .order(egui::Order::Foreground)
            .fixed_pos(parts.input.menu_at)
            .show(ctx, |ui| {
                egui::Frame::menu(ui.style()).show(ui, |ui| {
                    ui.set_min_width(120.0);
                    if let Some(settings) = settings.as_mut()
                        && ui.button("Settings").clicked()
                    {
                        settings.open = true;
                        next.menu = false;
                    }
                    if failed.is_some()
                        && ui
                            .button("Reconnect")
                            .on_hover_text("Connect to the agent running on this host again")
                            .clicked()
                    {
                        chat.session = Session::Connecting;
                        bridge.submit_connect();
                        next.menu = false;
                    }
                    if ui.button("Quit").clicked() {
                        requests.write(CompanionRequest::Quit);
                        next.menu = false;
                    }
                });
            });
        if area.response.clicked_elsewhere() && !parts.input.opened_now {
            next.menu = false;
        }
    }

    // The gesture buttons, beside the character. Drawn after the history
    // and the menu so a click here does not count as one outside them.
    if parts.prefs.show_buttons {
        let at = match body {
            Some(body) => egui::pos2(body.right() + GAP, body.center().y),
            None => egui::pos2(screen.right() - GAP, screen.center().y),
        };
        let pivot = if body.is_some() {
            egui::Align2::LEFT_CENTER
        } else {
            egui::Align2::RIGHT_CENTER
        };
        egui::Area::new(egui::Id::unique("companion_buttons"))
            .order(egui::Order::Middle)
            .pivot(pivot)
            .fixed_pos(at)
            .show(ctx, |ui| {
                if ui.button("Talk").clicked() {
                    next.apply(Gesture::OpenInput);
                }
                if ui.button("History").clicked() {
                    next.apply(Gesture::OpenHistory);
                }
                let menu = ui.button("Menu");
                if menu.clicked() {
                    parts.input.menu_at = menu.rect.right_top();
                    next.apply(Gesture::OpenMenu);
                }
            });
    }

    if parts.overlay.set_if_neq(next) {
        // An idle app draws the parts that just opened on the next frame
        // only if one is asked for.
        ctx.request_repaint();
    }
}

/// The settings, filling the window over everything else, with a way back.
fn settings_screen(ctx: &egui::Context, settings: &mut GuiSettings) {
    let screen = ctx.content_rect();
    egui::Area::new(egui::Id::unique("companion_settings"))
        .order(egui::Order::Middle)
        .fixed_pos(screen.min)
        .movable(false)
        .show(ctx, |ui| {
            // The area's own size is last frame's; the panels follow the
            // window as it is now.
            ui.scope_builder(egui::UiBuilder::new().max_rect(screen), |ui| {
                egui::Panel::top("companion_settings_bar").show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.heading("Settings");
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.button("Close").clicked() {
                                settings.open = false;
                                // This frame is already drawn; the next
                                // one shows the companion.
                                ui.ctx().request_repaint();
                            }
                        });
                    });
                });
                egui::CentralPanel::default().show(ui, |ui| settings.settings.ui(ui));
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
