//! The desktop companion's window (spec §4): one transparent, borderless
//! window that stays on top and out of the taskbar, holding the avatar and
//! the overlay the GUI draws over it.
//!
//! The window takes the pointer only where the GUI wants it ([`PointerWanted`]:
//! on the avatar or on the overlay's egui), while a drag is under way, or
//! while something of the overlay or the settings is open, so a click
//! outside them reaches egui and closes them. Elsewhere `hit_test` is off
//! and clicks reach whatever is beneath. A window that lets the pointer
//! through gets no cursor events, so the global cursor is polled
//! ([`device_query`]) and handed to the GUI as [`CursorOverride`]; that
//! needs frames, which [`update_mode`] provides.
//!
//! Where the global cursor cannot be read (Wayland, or macOS without the
//! accessibility permission) the window keeps taking the pointer: no
//! click-through, but nothing lost either.
//!
//! The window goes where [`CompanionPrefs::window`] says, or to the
//! primary monitor's bottom-right corner, and its place is saved once it
//! has stopped moving.

use std::time::Duration;

use bevy::prelude::*;
use bevy::window::{
    CompositeAlphaMode, CursorOptions, Monitor, PrimaryMonitor, PrimaryWindow, RawHandleWrapper,
    WindowLevel,
};
use bevy::winit::{UpdateMode, WinitSettings};
use device_query::{DeviceQuery, DeviceState};
use raw_window_handle::RawDisplayHandle;
use sapphire_agent_gui::GuiSettings;
use sapphire_agent_gui::companion::{
    CompanionPrefs, CompanionRequest, CursorOverride, Overlay, PointerWanted, WindowPlacement,
};

/// The window's size, in logical pixels, until the user has placed it.
pub const DEFAULT_SIZE: UVec2 = UVec2::new(420, 640);

/// The gap, in physical pixels, between a newly placed window and the
/// primary monitor's bottom-right corner.
pub const MARGIN: i32 = 32;

/// How often the window wakes to look at the global cursor.
const POLL: Duration = Duration::from_millis(50);

/// How long the window has to stay put before its place is saved.
const SETTLE: Duration = Duration::from_millis(500);

/// How many frames to wait for winit to report the monitors before the
/// window is placed without them.
const MONITOR_WAIT_FRAMES: u32 = 10;

/// A monitor's area on the desktop, in physical pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MonitorRect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl MonitorRect {
    fn contains(&self, x: i64, y: i64) -> bool {
        let (left, top) = (i64::from(self.x), i64::from(self.y));
        (left..left + i64::from(self.width)).contains(&x)
            && (top..top + i64::from(self.height)).contains(&y)
    }
}

/// Where the window goes: where it was saved if that is still on a
/// monitor (pulled wholly onto it and no larger than it), else at the
/// bottom-right of the primary monitor, `monitors[0]`, [`MARGIN`] in from
/// its corner. `default_size` (physical pixels) is the size when nothing
/// was saved. Without monitors to check against, the saved or default
/// placement stands.
pub fn place_on_screen(
    saved: Option<WindowPlacement>,
    monitors: &[MonitorRect],
    default_size: UVec2,
) -> WindowPlacement {
    let wanted = saved.unwrap_or(WindowPlacement {
        x: 0,
        y: 0,
        width: default_size.x,
        height: default_size.y,
    });
    let Some(primary) = monitors.first() else {
        return wanted;
    };
    let center = |p: &WindowPlacement| {
        (
            i64::from(p.x) + i64::from(p.width / 2),
            i64::from(p.y) + i64::from(p.height / 2),
        )
    };
    let home = saved.and_then(|s| {
        let (cx, cy) = center(&s);
        monitors.iter().find(|m| m.contains(cx, cy))
    });
    match home {
        Some(monitor) => fit(wanted, monitor),
        None => {
            let width = wanted.width.min(primary.width);
            let height = wanted.height.min(primary.height);
            let corner = |start: i32, extent: u32, size: u32| {
                (i64::from(start) + i64::from(extent) - i64::from(size) - i64::from(MARGIN))
                    .max(i64::from(start))
            };
            WindowPlacement {
                x: corner(primary.x, primary.width, width) as i32,
                y: corner(primary.y, primary.height, height) as i32,
                width,
                height,
            }
        }
    }
}

/// `p` shrunk to `monitor`'s size and moved wholly onto it.
fn fit(p: WindowPlacement, monitor: &MonitorRect) -> WindowPlacement {
    let width = p.width.min(monitor.width);
    let height = p.height.min(monitor.height);
    let pull = |pos: i32, start: i32, extent: u32, size: u32| {
        let start = i64::from(start);
        i64::from(pos).clamp(start, start + i64::from(extent) - i64::from(size)) as i32
    };
    WindowPlacement {
        x: pull(p.x, monitor.x, monitor.width, width),
        y: pull(p.y, monitor.y, monitor.height, height),
        width,
        height,
    }
}

/// The companion's window, for `WindowPlugin::primary_window`.
pub fn window() -> Window {
    Window {
        title: "sapphire-agent-desktop".to_string(),
        resolution: (DEFAULT_SIZE.x, DEFAULT_SIZE.y).into(),
        transparent: true,
        decorations: false,
        // Raised to `AlwaysOnTop` once placed: see `place_window`.
        window_level: WindowLevel::Normal,
        skip_taskbar: true,
        composite_alpha_mode: composite_alpha_mode(),
        ..default()
    }
}

/// macOS composites a transparent window only post-multiplied; elsewhere
/// the surface picks (and post-multiplied is often not on offer, which
/// would fail the surface).
fn composite_alpha_mode() -> CompositeAlphaMode {
    if cfg!(target_os = "macos") {
        CompositeAlphaMode::PostMultiplied
    } else {
        CompositeAlphaMode::Auto
    }
}

/// How often bevy updates, for [`sapphire_agent_gui::AgentGuiPlugin`]:
/// every [`POLL`] at the least, focused or not, so the global cursor is
/// read while the window lets the pointer through and gets no events.
pub fn update_mode() -> WinitSettings {
    WinitSettings {
        focused_mode: UpdateMode::reactive_low_power(POLL),
        unfocused_mode: UpdateMode::reactive_low_power(POLL),
    }
}

/// The window's behaviour on the desktop: placement, click-through, drag
/// and the saving of its place. Add it with the GUI, with the primary
/// window made by [`window`].
pub struct CompanionWindowPlugin;

impl Plugin for CompanionWindowPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(ClearColor(Color::NONE))
            .insert_non_send(GlobalPointer::Undecided)
            .init_resource::<Placed>()
            .init_resource::<Dragging>()
            .add_systems(
                PreUpdate,
                follow_global_cursor.after(bevy::input::InputSystems),
            )
            .add_systems(Update, (place_window, save_placement).chain())
            .add_systems(PostUpdate, (start_drag, take_pointer).chain());
    }
}

/// The global cursor, where it can be read.
enum GlobalPointer {
    /// The window has no handle yet to tell the display server by.
    Undecided,
    /// The pointer cannot be read: the window always takes it.
    Unavailable,
    Polling {
        device: DeviceState,
        /// The primary button is down now, wherever the pointer is.
        primary_down: bool,
    },
}

impl GlobalPointer {
    fn decide(display: RawDisplayHandle) -> Self {
        if matches!(display, RawDisplayHandle::Wayland(_)) {
            tracing::info!(
                "Wayland does not tell a window where the pointer is outside it: \
                 the companion window takes every click (no click-through)"
            );
            return Self::Unavailable;
        }
        match DeviceState::checked_new() {
            Some(device) => Self::Polling {
                device,
                primary_down: false,
            },
            None => {
                tracing::warn!(
                    "cannot read the global pointer: the companion window takes every click \
                     (no click-through)"
                );
                Self::Unavailable
            }
        }
    }

    /// The primary button is down: from the global pointer when there is
    /// one, which a press the window never saw released cannot fool.
    fn primary_down(&self, buttons: &ButtonInput<MouseButton>) -> bool {
        match self {
            Self::Polling { primary_down, .. } => *primary_down,
            _ => buttons.pressed(MouseButton::Left),
        }
    }
}

/// The window has been put in its place.
#[derive(Resource, Default)]
struct Placed(bool);

/// A drag the window manager is carrying out, until the button comes up.
#[derive(Resource, Default)]
struct Dragging(bool);

/// Reads the global cursor and hands it to the GUI in the window's logical
/// pixels, `None` while it is outside the window.
///
/// Also lets go of a primary press the window will never see released:
/// the window manager keeps the release that ends a drag of the window,
/// and the press would otherwise stay down, swallowing the next one.
fn follow_global_cursor(
    mut pointer: NonSendMut<GlobalPointer>,
    windows: Query<(&Window, Option<&RawHandleWrapper>), With<PrimaryWindow>>,
    mut cursor: ResMut<CursorOverride>,
    mut buttons: ResMut<ButtonInput<MouseButton>>,
) {
    let Ok((window, handle)) = windows.single() else {
        return;
    };
    if matches!(*pointer, GlobalPointer::Undecided) {
        let Some(handle) = handle else {
            return;
        };
        *pointer = GlobalPointer::decide(handle.get_display_handle());
    }
    let GlobalPointer::Polling {
        device,
        primary_down,
    } = &mut *pointer
    else {
        return;
    };
    let mouse = device.get_mouse();
    // 1-based: index 1 is the primary button.
    *primary_down = mouse.button_pressed.get(1).copied().unwrap_or(false);
    if !*primary_down && buttons.pressed(MouseButton::Left) {
        buttons.release(MouseButton::Left);
    }
    let WindowPosition::At(origin) = window.position else {
        cursor.set_if_neq(CursorOverride(None));
        return;
    };
    let local =
        (IVec2::new(mouse.coords.0, mouse.coords.1) - origin).as_vec2() / window.scale_factor();
    let inside = Rect::new(0.0, 0.0, window.width(), window.height()).contains(local);
    cursor.set_if_neq(CursorOverride(inside.then_some(local)));
}

/// Puts the window where it was saved (or at the primary monitor's corner)
/// once winit has reported the monitors.
fn place_window(
    mut placed: ResMut<Placed>,
    mut frames: Local<u32>,
    mut windows: Query<&mut Window, With<PrimaryWindow>>,
    monitors: Query<(&Monitor, Has<PrimaryMonitor>)>,
    prefs: Res<CompanionPrefs>,
) {
    if placed.0 {
        return;
    }
    *frames += 1;
    if monitors.is_empty() && *frames < MONITOR_WAIT_FRAMES {
        return;
    }
    let Ok(mut window) = windows.single_mut() else {
        return;
    };
    let mut found: Vec<_> = monitors.iter().collect();
    // The primary monitor first, as `place_on_screen` wants it.
    found.sort_by_key(|(_, primary)| !primary);
    let scale = found.first().map_or(1.0, |(m, _)| m.scale_factor) as f32;
    let rects: Vec<_> = found
        .iter()
        .map(|(m, _)| MonitorRect {
            x: m.physical_position.x,
            y: m.physical_position.y,
            width: m.physical_width,
            height: m.physical_height,
        })
        .collect();
    let default_size = (DEFAULT_SIZE.as_vec2() * scale).round().as_uvec2();
    let at = place_on_screen(prefs.window, &rects, default_size);
    window.position = WindowPosition::At(IVec2::new(at.x, at.y));
    window
        .resolution
        .set_physical_resolution(at.width, at.height);
    // On X11 winit asks the window manager to keep the window above while
    // it maps it, before the window manager has taken it on, and that
    // request is lost; asked now, it holds.
    window.window_level = WindowLevel::AlwaysOnTop;
    placed.0 = true;
}

/// Saves where the window is once it has stayed put for [`SETTLE`].
fn save_placement(
    placed: Res<Placed>,
    windows: Query<&Window, With<PrimaryWindow>>,
    mut prefs: ResMut<CompanionPrefs>,
    time: Res<Time<Real>>,
    mut last: Local<Option<(WindowPlacement, Duration)>>,
) {
    if !placed.0 {
        return;
    }
    let Ok(window) = windows.single() else {
        return;
    };
    let WindowPosition::At(at) = window.position else {
        return;
    };
    let now = WindowPlacement {
        x: at.x,
        y: at.y,
        width: window.resolution.physical_width(),
        height: window.resolution.physical_height(),
    };
    let elapsed = time.elapsed();
    match *last {
        Some((seen, since)) if seen == now => {
            if elapsed.saturating_sub(since) >= SETTLE && prefs.window != Some(now) {
                prefs.window = Some(now);
            }
        }
        _ => *last = Some((now, elapsed)),
    }
}

/// Moves the window with the pointer when the GUI asks, and only while
/// the primary button is really down: a press the window never saw come
/// up (released outside it) must not drag it when the pointer comes back.
fn start_drag(
    mut requests: MessageReader<CompanionRequest>,
    pointer: NonSend<GlobalPointer>,
    buttons: Res<ButtonInput<MouseButton>>,
    mut dragging: ResMut<Dragging>,
    mut windows: Query<&mut Window, With<PrimaryWindow>>,
) {
    let primary_down = pointer.primary_down(&buttons);
    if dragging.0 && !primary_down {
        dragging.0 = false;
    }
    if !requests.read().any(|r| *r == CompanionRequest::StartDrag) {
        return;
    }
    if !primary_down {
        tracing::debug!("a drag from a press already released: ignored");
        return;
    }
    if let Ok(mut window) = windows.single_mut() {
        tracing::debug!("moving the window with the pointer");
        window.start_drag_move();
        dragging.0 = true;
    }
}

/// Lets the pointer through the window except where, or while, the
/// companion needs it.
fn take_pointer(
    pointer: NonSend<GlobalPointer>,
    wanted: Res<PointerWanted>,
    dragging: Res<Dragging>,
    overlay: Res<Overlay>,
    settings: Option<NonSend<GuiSettings>>,
    mut cursor_options: Query<&mut CursorOptions, With<PrimaryWindow>>,
) {
    let polling = matches!(*pointer, GlobalPointer::Polling { .. });
    let take = !polling
        || wanted.0
        || dragging.0
        || *overlay != Overlay::default()
        || settings.is_some_and(|s| s.open);
    if let Ok(mut options) = cursor_options.single_mut()
        && options.hit_test != take
    {
        options.hit_test = take;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEFAULT: UVec2 = UVec2::new(420, 640);

    fn monitors() -> [MonitorRect; 2] {
        [
            MonitorRect {
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
            },
            MonitorRect {
                x: 1920,
                y: 0,
                width: 2560,
                height: 1440,
            },
        ]
    }

    #[test]
    fn a_window_saved_on_the_second_monitor_stays_there() {
        let saved = WindowPlacement {
            x: 3000,
            y: 500,
            width: 420,
            height: 640,
        };
        assert_eq!(place_on_screen(Some(saved), &monitors(), DEFAULT), saved);
    }

    #[test]
    fn a_window_saved_on_a_gone_monitor_moves_to_the_primary_bottom_right() {
        let saved = WindowPlacement {
            x: 5000,
            y: 300,
            width: 400,
            height: 600,
        };
        assert_eq!(
            place_on_screen(Some(saved), &monitors(), DEFAULT),
            WindowPlacement {
                x: 1920 - 400 - 32,
                y: 1080 - 600 - 32,
                width: 400,
                height: 600,
            }
        );
    }

    #[test]
    fn nothing_saved_puts_the_default_size_at_the_primary_bottom_right() {
        let mut ms = monitors();
        ms[0].x = -1920;
        assert_eq!(
            place_on_screen(None, &ms, DEFAULT),
            WindowPlacement {
                x: -1920 + 1920 - 420 - 32,
                y: 1080 - 640 - 32,
                width: 420,
                height: 640,
            }
        );
    }

    #[test]
    fn a_window_larger_than_its_monitor_is_clamped_to_fit() {
        let small = [MonitorRect {
            x: 100,
            y: 50,
            width: 800,
            height: 600,
        }];
        let saved = WindowPlacement {
            x: 200,
            y: 100,
            width: 1000,
            height: 900,
        };
        assert_eq!(
            place_on_screen(Some(saved), &small, DEFAULT),
            WindowPlacement {
                x: 100,
                y: 50,
                width: 800,
                height: 600,
            }
        );
        // A window hanging off its monitor's edge is pulled back onto it.
        let hanging = WindowPlacement {
            x: 700,
            y: 400,
            width: 300,
            height: 200,
        };
        assert_eq!(
            place_on_screen(Some(hanging), &small, DEFAULT),
            WindowPlacement {
                x: 600,
                y: 400,
                width: 300,
                height: 200,
            }
        );
    }

    #[test]
    fn without_monitors_the_saved_or_default_placement_stands() {
        let saved = WindowPlacement {
            x: -50,
            y: 7000,
            width: 5000,
            height: 10,
        };
        assert_eq!(place_on_screen(Some(saved), &[], DEFAULT), saved);
        assert_eq!(
            place_on_screen(None, &[], DEFAULT),
            WindowPlacement {
                x: 0,
                y: 0,
                width: 420,
                height: 640,
            }
        );
    }
}
