//! The companion overlay: the speech bubble, input box, history and menu
//! drawn over the avatar, and the gestures that open them (spec §5).
//!
//! The overlay is transparent: no panel covers the window, so whatever
//! the active renderer draws shows beneath, and the bubble, the input box,
//! the history and the menu are each an egui area of their own. Only the
//! settings, while open, fill the window.
//!
//! - [`gesture`]: what the pointer means (pure).
//! - [`bubble`]: what the bubble says and for how long (pure).
//! - [`prefs`]: the companion's preferences on this device.
//! - `view`: the egui of it all.
//!
//! Each frame, [`pointer_input`] turns the mouse and keyboard into
//! gestures on [`Overlay`] and writes [`PointerWanted`]; the view then
//! draws what [`Overlay`] holds. [`CompanionRequest`]s go out to whoever
//! acts on them: the desktop presenter moves the window on `StartDrag`,
//! and the GUI exits on `Quit`.

pub mod bubble;
pub mod character_page;
pub mod gesture;
pub mod prefs;
pub(crate) mod view;

use std::time::Duration;

use bevy::input::mouse::MouseWheel;
use bevy::prelude::*;
use bevy::window::PrimaryWindow;
use bevy_egui::{EguiContexts, EguiPrimaryContextPass, egui};

use crate::avatar::AvatarBounds;
use crate::chat::{ChatEntry, ChatRole, ChatState};
use crate::settings::GuiSettings;
use crate::{GuiRuntime, GuiWaker};
pub use bubble::{Bubble, BubbleContent, bubble_duration};
pub use gesture::{Gesture, GestureTracker, PointerSample, Press, wants_pointer};
pub use prefs::{CompanionPrefs, WindowPlacement};

/// Which parts of the overlay are open. The settings are not among them:
/// [`GuiSettings::open`] holds those.
#[derive(Resource, Default, Clone, Copy, Debug, PartialEq, Eq)]
pub struct Overlay {
    pub input: bool,
    pub history: bool,
    pub menu: bool,
}

impl Overlay {
    /// What `gesture` does to the overlay. Opening the input or the
    /// history puts the menu away; a drag leaves the overlay be.
    pub fn apply(&mut self, gesture: Gesture) {
        match gesture {
            Gesture::OpenInput => {
                self.input = true;
                self.menu = false;
            }
            Gesture::OpenHistory => {
                self.history = true;
                self.menu = false;
            }
            Gesture::OpenMenu => self.menu = true,
            Gesture::StartDrag => {}
            Gesture::CloseAll => *self = Self::default(),
        }
    }
}

/// What the overlay asks of the app around it.
#[derive(Message, Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompanionRequest {
    /// Move the window with the pointer, from a drag on the avatar. The
    /// desktop presenter handles it.
    StartDrag,
    /// Quit the app, from the menu. The GUI handles it.
    Quit,
}

/// Where in `PostUpdate` the overlay runs: bevy_egui's pass. A system that
/// acts on this frame's [`PointerWanted`] or [`CompanionRequest`]s goes
/// `.after` it.
pub const OVERLAY_PASS: bevy_egui::EguiPostUpdateSet = bevy_egui::EguiPostUpdateSet::EndPass;

/// Whether the window should take the pointer where it is now: on the
/// avatar or on the overlay's egui. Written every frame (changed only when
/// it flips) for the desktop presenter, which lets the pointer through to
/// the desktop elsewhere.
#[derive(Resource, Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct PointerWanted(pub bool);

/// Where the pointer is, when the app knows it better than the window
/// does: a click-through window gets no cursor events, so the desktop
/// presenter writes the global cursor here.
#[derive(Resource, Clone, Copy, Default, Debug, PartialEq)]
pub enum CursorOverride {
    /// No override: the primary window's own cursor position is used.
    #[default]
    None,
    /// Outside the window, whatever the window last saw.
    Outside,
    /// At this point, in logical window pixels.
    At(Vec2),
}

impl CursorOverride {
    /// The pointer in logical window pixels, given what the window itself
    /// reports (`window`); `None` when it is not over the window.
    pub fn resolve(self, window: impl FnOnce() -> Option<Vec2>) -> Option<Vec2> {
        match self {
            Self::None => window(),
            Self::Outside => None,
            Self::At(pos) => Some(pos),
        }
    }
}

/// What the latest pointer input meant for the view.
#[derive(Resource, Default)]
pub(crate) struct OverlayInput {
    /// Where the menu opens: the right-click, or the menu button.
    pub(crate) menu_at: egui::Pos2,
    /// Something opened this frame. The click that opened it must not
    /// also count as a click outside it, which closes it.
    pub(crate) opened_now: bool,
}

/// The reply the bubble shows, and how much of the history it has seen.
#[derive(Resource, Default)]
pub struct Speech {
    pub bubble: Option<Bubble>,
    seen: usize,
}

impl Speech {
    /// Picks up what the history gained since the last call at `now`: the
    /// newest reply (the agent's or an error, never the user's own line)
    /// goes in the bubble. Returns how long it shows, so the caller can
    /// wake the GUI to take it down; `None` when there is nothing new.
    pub fn follow(&mut self, history: &[ChatEntry], now: Duration) -> Option<Duration> {
        let new = history.get(self.seen..).unwrap_or_default();
        self.seen = history.len();
        let reply = new.iter().rev().find(|e| e.role != ChatRole::User)?;
        let shown_for = bubble_duration(&reply.text);
        self.bubble = Some(Bubble {
            text: reply.text.clone(),
            until: now + shown_for,
        });
        Some(shown_for)
    }
}

/// The companion overlay. [`crate::AgentGuiPlugin`] adds it; it loads
/// [`CompanionPrefs`] in `finish`.
pub struct CompanionPlugin;

impl Plugin for CompanionPlugin {
    fn build(&self, app: &mut App) {
        app.add_message::<CompanionRequest>()
            .init_resource::<Overlay>()
            .init_resource::<OverlayInput>()
            .init_resource::<PointerWanted>()
            .init_resource::<CursorOverride>()
            .init_resource::<Speech>()
            .init_resource::<character_page::RendererPanels>()
            .add_systems(
                Update,
                (
                    follow_chat.after(crate::chat::drain_bridge_events),
                    save_prefs,
                ),
            )
            .add_systems(
                EguiPrimaryContextPass,
                // After the settings apply their look, and the overlay's
                // input before the overlay it opens.
                (pointer_input, view::ui)
                    .chain()
                    .after(crate::settings::follow),
            )
            // Same frame as the menu's click: an idle app may not draw
            // another before the user clicks again.
            .add_systems(Last, quit_on_request);
    }

    fn finish(&self, app: &mut App) {
        let path = CompanionPrefs::default_path();
        let prefs = match &path {
            Some(path) => CompanionPrefs::load(path),
            None => {
                tracing::warn!("no config directory; the companion's preferences are not kept");
                CompanionPrefs::default()
            }
        };
        app.insert_resource(PrefsFile {
            path,
            saved: prefs.clone(),
        })
        .insert_resource(prefs);
    }
}

/// Where the prefs are kept and what was last written there.
#[derive(Resource)]
struct PrefsFile {
    path: Option<std::path::PathBuf>,
    saved: CompanionPrefs,
}

/// Writes the prefs whenever they differ from what was last saved.
fn save_prefs(prefs: Res<CompanionPrefs>, mut file: ResMut<PrefsFile>) {
    if !prefs.is_changed() || *prefs == file.saved {
        return;
    }
    if let Some(path) = &file.path
        && let Err(e) = prefs.save(path)
    {
        tracing::warn!("cannot save the companion's preferences: {e:#}");
    }
    // Saved or not, this is the value tried: a failing disk is not retried
    // every frame.
    file.saved = prefs.clone();
}

/// Puts a new reply in the bubble and wakes the GUI when it is due to go:
/// an idle window draws no frame on its own.
fn follow_chat(
    chat: Res<ChatState>,
    mut speech: ResMut<Speech>,
    time: Res<Time<Real>>,
    runtime: Res<GuiRuntime>,
    waker: Res<GuiWaker>,
) {
    if !chat.is_changed() {
        return;
    }
    if let Some(shown_for) = speech.follow(&chat.history, time.elapsed()) {
        waker.wake_in(&runtime, shown_for);
    }
}

/// The mouse and keyboard as the overlay's gestures.
#[derive(bevy::ecs::system::SystemParam)]
pub(crate) struct Pointer<'w, 's> {
    window: Query<'w, 's, &'static Window, With<PrimaryWindow>>,
    cursor_override: Res<'w, CursorOverride>,
    buttons: Res<'w, ButtonInput<MouseButton>>,
    keys: Res<'w, ButtonInput<KeyCode>>,
    wheel: MessageReader<'w, 's, MouseWheel>,
}

impl Pointer<'_, '_> {
    fn cursor(&self) -> Option<Vec2> {
        self.cursor_override
            .resolve(|| self.window.single().ok()?.cursor_position())
    }
}

/// Turns this frame's pointer and keys into gestures on [`Overlay`] and
/// [`CompanionRequest`]s, and writes [`PointerWanted`]. Runs in the egui
/// pass, before the view, so it can ask egui what lies under the pointer.
// A bevy system: each argument is a resource it reads or writes.
#[allow(clippy::too_many_arguments)]
pub(crate) fn pointer_input(
    mut contexts: EguiContexts,
    mut pointer: Pointer,
    bounds: Res<AvatarBounds>,
    mut tracker: Local<GestureTracker>,
    mut overlay: ResMut<Overlay>,
    mut input: ResMut<OverlayInput>,
    mut wanted: ResMut<PointerWanted>,
    mut requests: MessageWriter<CompanionRequest>,
    settings: Option<NonSend<GuiSettings>>,
) {
    let Ok(ctx) = contexts.ctx_mut() else {
        return;
    };
    let cursor = pointer.cursor();
    // The overlay's areas are above `Order::Background`, where the root
    // `Ui` and the placeholder are. Asked by position rather than through
    // egui's own pointer so `CursorOverride` counts too.
    let over_ui = ctx.egui_is_using_pointer()
        || cursor.is_some_and(|c| {
            ctx.layer_id_at(egui::pos2(c.x, c.y))
                .is_some_and(|layer| layer.order != egui::Order::Background)
        });
    wanted.set_if_neq(PointerWanted(wants_pointer(cursor, bounds.0, over_ui)));

    let mut gestures = Vec::new();
    if let Some(pos) = cursor {
        let at = PointerSample {
            pos,
            over_avatar: bounds.0.is_some_and(|b| b.contains(pos)),
            over_ui,
        };
        for (button, press) in [
            (MouseButton::Left, Press::Primary),
            (MouseButton::Right, Press::Secondary),
        ] {
            if pointer.buttons.just_pressed(button) {
                tracker.press(press, at);
            }
        }
        gestures.extend(tracker.moved(at));
        for (button, press) in [
            (MouseButton::Left, Press::Primary),
            (MouseButton::Right, Press::Secondary),
        ] {
            if pointer.buttons.just_released(button) {
                gestures.extend(tracker.release(press, at));
            }
        }
        let scrolled: f32 = pointer.wheel.read().map(|w| w.y).sum();
        gestures.extend(tracker.scroll(at, scrolled));
    } else {
        pointer.wheel.clear();
    }

    let keys = &pointer.keys;
    if keys.just_pressed(KeyCode::Escape) {
        gestures.push(tracker.escape());
    }
    let settings_open = settings.is_some_and(|s| s.open);
    let ctrl = keys.any_pressed([KeyCode::ControlLeft, KeyCode::ControlRight]);
    if keys.any_just_pressed([KeyCode::Enter, KeyCode::NumpadEnter])
        && !settings_open
        && !ctx.egui_wants_keyboard_input()
        && (ctrl || *overlay == Overlay::default())
    {
        gestures.push(Gesture::OpenInput);
    }

    let mut next = *overlay;
    input.opened_now = false;
    for gesture in gestures {
        match gesture {
            Gesture::StartDrag => {
                requests.write(CompanionRequest::StartDrag);
            }
            Gesture::OpenMenu => {
                if let Some(c) = cursor {
                    input.menu_at = egui::pos2(c.x, c.y);
                }
                input.opened_now = true;
            }
            Gesture::OpenInput | Gesture::OpenHistory => input.opened_now = true,
            Gesture::CloseAll => {}
        }
        next.apply(gesture);
    }
    overlay.set_if_neq(next);
}

/// Exits the app when the menu asks to quit.
fn quit_on_request(
    mut requests: MessageReader<CompanionRequest>,
    mut exit: MessageWriter<AppExit>,
) {
    if requests.read().any(|r| *r == CompanionRequest::Quit) {
        exit.write(AppExit::Success);
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::chat::{BridgeEvent, ChatState, Session};

    #[test]
    fn a_cursor_outside_never_falls_back_to_the_window() {
        let stale = || Some(Vec2::new(5.0, 5.0));
        assert_eq!(CursorOverride::None.resolve(stale), stale());
        assert_eq!(CursorOverride::Outside.resolve(stale), None);
        let at = Vec2::new(1.0, 2.0);
        assert_eq!(CursorOverride::At(at).resolve(stale), Some(at));
    }

    #[test]
    fn gestures_open_and_close_the_overlay() {
        let mut o = Overlay::default();
        o.apply(Gesture::OpenMenu);
        assert_eq!(
            o,
            Overlay {
                menu: true,
                ..Default::default()
            }
        );
        // Opening the input or the history puts the menu away.
        o.apply(Gesture::OpenInput);
        assert_eq!(
            o,
            Overlay {
                input: true,
                ..Default::default()
            }
        );
        o.apply(Gesture::OpenHistory);
        assert_eq!(
            o,
            Overlay {
                input: true,
                history: true,
                menu: false
            }
        );
        // A drag moves the window and leaves the overlay be.
        o.apply(Gesture::StartDrag);
        assert!(o.input && o.history);
        o.apply(Gesture::CloseAll);
        assert_eq!(o, Overlay::default());
    }

    /// The input box stays open by construction: `Speech::follow` has no
    /// way to the `Overlay`, so only the draft is worth checking.
    #[test]
    fn a_reply_while_typing_keeps_the_draft() {
        let mut chat = ChatState::new();
        chat.session = Session::Ready {
            display_id: "a".into(),
        };
        chat.draft = "first".into();
        chat.take_submission().unwrap();
        let mut speech = Speech::default();
        let now = Duration::from_secs(100);
        assert_eq!(
            speech.follow(&chat.history, now),
            None,
            "the user's own line"
        );

        // The user goes on typing while the agent answers.
        chat.draft = "abc".into();
        chat.apply(BridgeEvent::ChatText {
            text: "hello".into(),
        });
        let shown_for = speech.follow(&chat.history, now);

        assert_eq!(chat.draft, "abc");
        assert_eq!(shown_for, Some(bubble_duration("hello")));
        let bubble = speech.bubble.as_ref().unwrap();
        assert_eq!(bubble.text, "hello");
        assert_eq!(bubble.until, now + bubble_duration("hello"));
        // Seen once: the next frame shows nothing new.
        assert_eq!(speech.follow(&chat.history, now), None);
    }
}
