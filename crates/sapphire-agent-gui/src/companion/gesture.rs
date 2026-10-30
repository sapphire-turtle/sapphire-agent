//! What the pointer means: a click, a drag, a right-click or a scroll on
//! the companion turns into a [`Gesture`] (spec §5).
//!
//! Pure: the bevy side feeds [`GestureTracker`] samples of the pointer and
//! acts on what it returns.

use bevy::math::{Rect, Vec2};

/// How far, in logical pixels, a primary press on the avatar moves before
/// it is a drag rather than a click.
pub const DRAG_PX: f32 = 4.0;

/// The pointer at one moment, in logical window pixels (egui's points).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PointerSample {
    pub pos: Vec2,
    /// Inside [`crate::avatar::AvatarBounds`].
    pub over_avatar: bool,
    /// Over the overlay's own egui (bubble, input, history, menu…): those
    /// presses are egui's, not gestures.
    pub over_ui: bool,
}

/// A mouse button the gestures use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Press {
    Primary,
    Secondary,
}

/// What the user asked of the companion.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Gesture {
    OpenInput,
    OpenHistory,
    OpenMenu,
    /// Move the window with the pointer (the desktop presenter does it).
    StartDrag,
    CloseAll,
}

/// A press that started on the avatar and has not been released yet.
#[derive(Clone, Copy, Debug)]
struct Held {
    button: Press,
    from: Vec2,
    dragging: bool,
}

/// Turns pointer samples into gestures. Only presses that start on the
/// avatar, off the overlay's egui, count.
#[derive(Debug, Default)]
pub struct GestureTracker {
    held: Option<Held>,
}

impl GestureTracker {
    /// `button` went down at `at`.
    pub fn press(&mut self, button: Press, at: PointerSample) {
        self.held = (at.over_avatar && !at.over_ui).then_some(Held {
            button,
            from: at.pos,
            dragging: false,
        });
    }

    /// The pointer moved to `to`: a primary press held on the avatar that
    /// goes past [`DRAG_PX`] starts a drag, once.
    pub fn moved(&mut self, to: PointerSample) -> Option<Gesture> {
        let held = self.held.as_mut()?;
        if held.button != Press::Primary || held.dragging || held.from.distance(to.pos) <= DRAG_PX {
            return None;
        }
        held.dragging = true;
        Some(Gesture::StartDrag)
    }

    /// `button` came up at `at`: a press and release on the avatar that
    /// did not drag is a click.
    pub fn release(&mut self, button: Press, at: PointerSample) -> Option<Gesture> {
        let held = self.held.take_if(|held| held.button == button)?;
        if held.dragging || !at.over_avatar || at.over_ui {
            return None;
        }
        Some(match button {
            Press::Primary => Gesture::OpenInput,
            Press::Secondary => Gesture::OpenMenu,
        })
    }

    /// The wheel turned by `delta` at `at`.
    pub fn scroll(&mut self, at: PointerSample, delta: f32) -> Option<Gesture> {
        (at.over_avatar && !at.over_ui && delta != 0.0).then_some(Gesture::OpenHistory)
    }

    /// Esc was pressed.
    pub fn escape(&mut self) -> Gesture {
        self.held = None;
        Gesture::CloseAll
    }
}

/// Whether the companion's window should take the pointer at `cursor`
/// rather than let it through to what is beneath: on the avatar, or where
/// egui wants it. Without a cursor, nothing to take.
pub fn wants_pointer(cursor: Option<Vec2>, bounds: Option<Rect>, egui_wants: bool) -> bool {
    let Some(cursor) = cursor else {
        return false;
    };
    egui_wants || bounds.is_some_and(|b| b.contains(cursor))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn on(x: f32, y: f32) -> PointerSample {
        PointerSample {
            pos: Vec2::new(x, y),
            over_avatar: true,
            over_ui: false,
        }
    }

    fn off(x: f32, y: f32) -> PointerSample {
        PointerSample {
            over_avatar: false,
            ..on(x, y)
        }
    }

    fn over_ui(x: f32, y: f32) -> PointerSample {
        PointerSample {
            over_ui: true,
            ..on(x, y)
        }
    }

    #[test]
    fn a_click_on_the_avatar_opens_input() {
        let mut t = GestureTracker::default();
        t.press(Press::Primary, on(10.0, 10.0));
        assert_eq!(t.moved(on(11.0, 11.0)), None);
        assert_eq!(
            t.release(Press::Primary, on(11.0, 11.0)),
            Some(Gesture::OpenInput)
        );
    }

    #[test]
    fn a_click_off_the_avatar_does_nothing() {
        let mut t = GestureTracker::default();
        t.press(Press::Primary, off(10.0, 10.0));
        assert_eq!(t.moved(off(30.0, 10.0)), None);
        assert_eq!(t.release(Press::Primary, off(30.0, 10.0)), None);
        // Pressed off it and released on it is no click on it either.
        t.press(Press::Primary, off(10.0, 10.0));
        assert_eq!(t.release(Press::Primary, on(10.0, 10.0)), None);
        assert_eq!(t.scroll(off(10.0, 10.0), 3.0), None);
        t.press(Press::Secondary, off(10.0, 10.0));
        assert_eq!(t.release(Press::Secondary, off(10.0, 10.0)), None);
    }

    #[test]
    fn dragging_starts_once_past_four_pixels_and_suppresses_the_click() {
        let mut t = GestureTracker::default();
        t.press(Press::Primary, on(10.0, 10.0));
        // Exactly four pixels is still a click's wobble.
        assert_eq!(t.moved(on(14.0, 10.0)), None);
        assert_eq!(t.moved(on(15.0, 10.0)), Some(Gesture::StartDrag));
        // Once, however far it goes on.
        assert_eq!(t.moved(on(40.0, 10.0)), None);
        assert_eq!(t.moved(off(80.0, 10.0)), None);
        assert_eq!(t.release(Press::Primary, on(40.0, 10.0)), None);
        // The next press starts afresh.
        t.press(Press::Primary, on(40.0, 10.0));
        assert_eq!(
            t.release(Press::Primary, on(40.0, 10.0)),
            Some(Gesture::OpenInput)
        );
    }

    #[test]
    fn secondary_click_opens_the_menu() {
        let mut t = GestureTracker::default();
        t.press(Press::Secondary, on(10.0, 10.0));
        // The secondary button never drags.
        assert_eq!(t.moved(on(30.0, 10.0)), None);
        assert_eq!(
            t.release(Press::Secondary, on(30.0, 10.0)),
            Some(Gesture::OpenMenu)
        );
    }

    #[test]
    fn scroll_on_the_avatar_opens_history() {
        let mut t = GestureTracker::default();
        assert_eq!(t.scroll(on(10.0, 10.0), 0.0), None);
        assert_eq!(t.scroll(on(10.0, 10.0), -1.0), Some(Gesture::OpenHistory));
        assert_eq!(t.scroll(on(10.0, 10.0), 0.5), Some(Gesture::OpenHistory));
        assert_eq!(t.escape(), Gesture::CloseAll);
    }

    #[test]
    fn presses_over_egui_are_ignored() {
        let mut t = GestureTracker::default();
        t.press(Press::Primary, over_ui(10.0, 10.0));
        assert_eq!(t.moved(on(30.0, 10.0)), None);
        assert_eq!(t.release(Press::Primary, on(30.0, 10.0)), None);
        t.press(Press::Secondary, over_ui(10.0, 10.0));
        assert_eq!(t.release(Press::Secondary, on(10.0, 10.0)), None);
        assert_eq!(t.scroll(over_ui(10.0, 10.0), 1.0), None);
        // A press on the avatar released over egui is egui's too.
        t.press(Press::Primary, on(10.0, 10.0));
        assert_eq!(t.release(Press::Primary, over_ui(10.0, 10.0)), None);
    }

    #[test]
    fn wants_pointer_is_true_on_the_avatar_or_egui_only() {
        let bounds = Some(Rect::new(0.0, 0.0, 100.0, 200.0));
        let inside = Some(Vec2::new(50.0, 50.0));
        let outside = Some(Vec2::new(150.0, 50.0));
        assert!(wants_pointer(inside, bounds, false));
        assert!(!wants_pointer(outside, bounds, false));
        assert!(wants_pointer(outside, bounds, true));
        assert!(!wants_pointer(None, bounds, false));
        assert!(!wants_pointer(None, bounds, true));
        assert!(!wants_pointer(inside, None, false));
    }
}
