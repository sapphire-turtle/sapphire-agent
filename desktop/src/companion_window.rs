//! The desktop companion's window (spec §4): one transparent, borderless
//! window that stays on top, holding the avatar and the overlay the GUI
//! draws over it.
//!
//! The window takes the pointer only where the GUI wants it ([`PointerWanted`]:
//! on the avatar or on the overlay's egui, the input box included), while a
//! drag is under way, or while the history, the menu or the settings are
//! open, so a click outside them reaches egui and closes them. Elsewhere
//! `hit_test` is off and clicks reach whatever is beneath. A window that
//! lets the pointer through gets no cursor events, so [`GlobalPointer`]
//! follows the pointer off bevy's thread and the window hands it to the GUI
//! as [`CursorOverride`].
//!
//! Where the global pointer cannot be read (Wayland, no libX11) the window
//! keeps taking the pointer: no click-through, but nothing lost either.
//!
//! The window goes where [`CompanionPrefs::window`] says, or to the
//! primary monitor's bottom-right corner, and its place is saved once it
//! has stopped moving.

use std::time::{Duration, Instant};

use bevy::prelude::*;
use bevy::window::{
    CompositeAlphaMode, CursorOptions, Monitor, PrimaryMonitor, PrimaryWindow, RawHandleWrapper,
    RequestRedraw, WindowLevel,
};
use raw_window_handle::RawDisplayHandle;
use sapphire_agent_gui::companion::{
    CompanionPrefs, CompanionRequest, CursorOverride, OVERLAY_PASS, Overlay, PointerWanted,
    WindowPlacement,
};
use sapphire_agent_gui::{GuiRuntime, GuiSettings, GuiWaker};

use crate::global_pointer::{GlobalPointer, Status, WindowRect};

/// The window's size, in logical pixels, until the user has placed it.
pub const DEFAULT_SIZE: UVec2 = UVec2::new(420, 640);

/// The gap, in logical pixels, between a newly placed window and the
/// primary monitor's bottom-right corner.
pub const MARGIN: u32 = 32;

/// How long the window has to stay put before its place is saved.
const SETTLE: Duration = Duration::from_millis(500);

/// How long the window is held where it was placed while the window
/// manager takes it on.
const PLACEMENT_SETTLE: Duration = Duration::from_secs(2);

/// How many frames to wait for winit to report the monitors before the
/// window is placed without them.
const MONITOR_WAIT_FRAMES: u32 = 10;

/// A monitor's area on the desktop, in physical pixels, and its scale
/// factor.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MonitorRect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub scale: f64,
}

impl MonitorRect {
    fn contains(&self, x: i64, y: i64) -> bool {
        let (left, top) = (i64::from(self.x), i64::from(self.y));
        (left..left + i64::from(self.width)).contains(&x)
            && (top..top + i64::from(self.height)).contains(&y)
    }

    /// `logical` pixels on this monitor, in physical ones.
    fn physical(&self, logical: u32) -> u32 {
        (f64::from(logical) * self.scale).round() as u32
    }
}

/// Where the window goes, in physical pixels: where it was saved if that
/// is still on a monitor (pulled wholly onto it and no larger than it),
/// else at the bottom-right of the primary monitor, `monitors[0]`,
/// [`MARGIN`] in from its corner. `default_size` (logical pixels, scaled by
/// the primary monitor's factor) is the size when nothing was saved.
/// Without monitors to check against, the saved or default placement
/// stands.
pub fn place_on_screen(
    saved: Option<WindowPlacement>,
    monitors: &[MonitorRect],
    default_size: UVec2,
) -> WindowPlacement {
    let Some(primary) = monitors.first() else {
        return saved.unwrap_or(WindowPlacement {
            x: 0,
            y: 0,
            width: default_size.x,
            height: default_size.y,
        });
    };
    let wanted = saved.unwrap_or(WindowPlacement {
        x: 0,
        y: 0,
        width: primary.physical(default_size.x),
        height: primary.physical(default_size.y),
    });
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
            let margin = i64::from(primary.physical(MARGIN));
            let corner = |start: i32, extent: u32, size: u32| {
                (i64::from(start) + i64::from(extent) - i64::from(size) - margin)
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
        // Honoured on Windows only.
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

/// The window's behaviour on the desktop: placement, click-through, drag
/// and the saving of its place. Add it with the GUI, with the primary
/// window made by [`window`].
pub struct CompanionWindowPlugin;

impl Plugin for CompanionWindowPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(ClearColor(Color::NONE))
            .init_resource::<Placement>()
            .init_resource::<Dragging>()
            .init_resource::<Press>()
            .init_resource::<Unflushed>()
            .add_systems(First, flush_window_requests)
            .add_systems(
                PreUpdate,
                (start_global_pointer, follow_global_pointer, settle_press)
                    .chain()
                    .after(bevy::input::InputSystems),
            )
            .add_systems(Update, (place_window, save_placement).chain())
            .add_systems(
                PostUpdate,
                (start_drag, take_pointer).chain().after(OVERLAY_PASS),
            );
    }
}

/// Where the window stands in being placed.
#[derive(Resource, Default)]
enum Placement {
    /// Waiting for the monitors.
    #[default]
    Pending,
    /// Asked to go to `target`. A window manager that maps the window
    /// after that may put it where it likes instead, so the request is
    /// repeated until `until`, unless the user has moved it meanwhile.
    Settling {
        target: IVec2,
        until: Instant,
    },
    Done,
}

/// A drag the window manager is carrying out, since when. Wall-clock
/// instants, as the global pointer's samples are: bevy's frame time can lag
/// behind after the app has idled.
#[derive(Resource, Default)]
struct Dragging(Option<Instant>);

/// The primary press as the window saw it.
#[derive(Resource, Default)]
struct Press {
    /// When the window saw it go down, while it is down.
    since: Option<Instant>,
    /// A drag took it: let it go next frame, as the window will not see it
    /// come up (the window manager keeps that release).
    taken: bool,
}

/// Window requests (hit-test, drag) made last frame that may still sit in
/// the display connection's buffer.
#[derive(Resource, Default)]
struct Unflushed(bool);

/// For a system that changes the window in a way winit sends as a request
/// to the display server: makes sure it gets there.
#[derive(bevy::ecs::system::SystemParam)]
struct WindowRequests<'w> {
    unflushed: ResMut<'w, Unflushed>,
    redraw: MessageWriter<'w, RequestRedraw>,
}

impl WindowRequests<'_> {
    /// A request was made this frame: flush it at the start of the next,
    /// which this asks for.
    fn made(&mut self) {
        self.unflushed.0 = true;
        self.redraw.write(RequestRedraw);
    }
}

/// Wakes the GUI once a deadline passes. The app idles between frames, so
/// a system waiting for time to pass (the window to settle) would
/// otherwise wait for an unrelated frame: up to a minute with the window
/// unfocused.
#[derive(bevy::ecs::system::SystemParam)]
struct Alarm<'w> {
    runtime: Option<Res<'w, GuiRuntime>>,
    waker: Option<Res<'w, GuiWaker>>,
}

impl Alarm<'_> {
    /// A frame comes no sooner than `after` from now.
    fn wake_in(&self, after: Duration) {
        if let (Some(runtime), Some(waker)) = (&self.runtime, &self.waker) {
            waker.wake_in(runtime, after);
        }
    }
}

/// Sends last frame's window requests on to the display server.
///
/// winit hands X11 its hit-test and drag requests without flushing the
/// connection, and an app that then idles leaves them in the buffer: the
/// window keeps letting clicks through (or keeps taking them) until some
/// later frame happens to flush. The frame that made them asks for this
/// one ([`RequestRedraw`]), whose `First` flushes.
fn flush_window_requests(
    mut unflushed: ResMut<Unflushed>,
    windows: Query<&RawHandleWrapper, With<PrimaryWindow>>,
    #[cfg(target_os = "linux")] mut xlib: Local<Option<Option<x11_dl::xlib::Xlib>>>,
) {
    if !std::mem::take(&mut unflushed.0) {
        return;
    }
    let Ok(handle) = windows.single() else {
        return;
    };
    #[cfg(target_os = "linux")]
    if let RawDisplayHandle::Xlib(display) = handle.get_display_handle()
        && let Some(display) = display.display
        && let Some(xlib) = xlib.get_or_insert_with(|| x11_dl::xlib::Xlib::open().ok())
    {
        // SAFETY: the window's own, open display; winit has made Xlib
        // thread-safe (`XInitThreads`) before opening it.
        unsafe { (xlib.XFlush)(display.as_ptr().cast()) };
    }
    #[cfg(not(target_os = "linux"))]
    let _ = handle;
}

/// Starts following the global pointer once the window has a handle to
/// tell the display server by. Not on Wayland, which does not tell a
/// window where the pointer is outside it.
fn start_global_pointer(
    mut commands: Commands,
    pointer: Option<Res<GlobalPointer>>,
    windows: Query<&RawHandleWrapper, With<PrimaryWindow>>,
    waker: Option<Res<GuiWaker>>,
) {
    if pointer.is_some() {
        return;
    }
    let (Ok(handle), Some(waker)) = (windows.single(), waker) else {
        return;
    };
    let pointer = if matches!(handle.get_display_handle(), RawDisplayHandle::Wayland(_)) {
        tracing::info!(
            "Wayland does not tell a window where the pointer is outside it: \
             the companion window takes every click (no click-through)"
        );
        GlobalPointer::unavailable()
    } else {
        GlobalPointer::start(waker.0.clone())
    };
    commands.insert_resource(pointer);
}

/// Tells the poller where the window is and hands the GUI where the
/// pointer is: at a point in the window, or outside it.
fn follow_global_pointer(
    pointer: Option<Res<GlobalPointer>>,
    windows: Query<&Window, With<PrimaryWindow>>,
    mut cursor: ResMut<CursorOverride>,
) {
    let (Some(pointer), Ok(window)) = (pointer, windows.single()) else {
        return;
    };
    let rect = match window.position {
        WindowPosition::At(origin) => Some(WindowRect {
            origin,
            size: window.physical_size(),
            scale: window.scale_factor(),
        }),
        _ => None,
    };
    pointer.set_window(rect);
    if pointer.status() != Status::Polling {
        return;
    }
    let next = match pointer.sample().at {
        Some(at) => CursorOverride::At(at),
        None => CursorOverride::Outside,
    };
    cursor.set_if_neq(next);
}

/// Keeps the primary press honest before the overlay reads it: a press a
/// drag took is let go, and so is one the global pointer has seen come up
/// though the window never did.
fn settle_press(
    mut press: ResMut<Press>,
    mut buttons: ResMut<ButtonInput<MouseButton>>,
    pointer: Option<Res<GlobalPointer>>,
) {
    if buttons.just_pressed(MouseButton::Left) {
        press.since = Some(Instant::now());
    }
    let stale = pointer
        .zip(press.since)
        .is_some_and(|(p, since)| p.released_since(since));
    let taken = std::mem::take(&mut press.taken);
    if (taken || stale) && buttons.pressed(MouseButton::Left) {
        tracing::debug!(taken, stale, "letting go of the primary press");
        buttons.release(MouseButton::Left);
    }
    if !buttons.pressed(MouseButton::Left) {
        press.since = None;
    }
}

/// Puts the window where it was saved (or at the primary monitor's corner)
/// once winit has reported the monitors, and keeps it there while the
/// window manager settles.
// A bevy system: each argument is a resource it reads or writes.
#[allow(clippy::too_many_arguments)]
fn place_window(
    mut placement: ResMut<Placement>,
    mut frames: Local<u32>,
    mut windows: Query<&mut Window, With<PrimaryWindow>>,
    monitors: Query<(&Monitor, Has<PrimaryMonitor>)>,
    prefs: Res<CompanionPrefs>,
    dragging: Res<Dragging>,
    mut requests_made: WindowRequests,
    alarm: Alarm,
) {
    match *placement {
        Placement::Pending => {}
        Placement::Settling { target, until } => {
            if dragging.0.is_some() || Instant::now() >= until {
                *placement = Placement::Done;
            } else if let Ok(mut window) = windows.single_mut()
                && window.position != WindowPosition::At(target)
            {
                window.position = WindowPosition::At(target);
                requests_made.made();
            }
            return;
        }
        Placement::Done => return,
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
    let rects: Vec<_> = found
        .iter()
        .map(|(m, _)| MonitorRect {
            x: m.physical_position.x,
            y: m.physical_position.y,
            width: m.physical_width,
            height: m.physical_height,
            scale: m.scale_factor,
        })
        .collect();
    let at = place_on_screen(prefs.window, &rects, DEFAULT_SIZE);
    window.position = WindowPosition::At(IVec2::new(at.x, at.y));
    window
        .resolution
        .set_physical_resolution(at.width, at.height);
    // On X11 winit asks the window manager to keep the window above while
    // it maps it, before the window manager has taken it on, and that
    // request is lost; asked now, it holds.
    window.window_level = WindowLevel::AlwaysOnTop;
    requests_made.made();
    *placement = Placement::Settling {
        target: IVec2::new(at.x, at.y),
        until: Instant::now() + PLACEMENT_SETTLE,
    };
    // To let go of the window when the settling is over.
    alarm.wake_in(PLACEMENT_SETTLE);
}

/// Saves where the window is once it has stayed put for [`SETTLE`].
fn save_placement(
    placement: Res<Placement>,
    windows: Query<&Window, With<PrimaryWindow>>,
    mut prefs: ResMut<CompanionPrefs>,
    time: Res<Time<Real>>,
    mut last: Local<Option<(WindowPlacement, Duration)>>,
    alarm: Alarm,
) {
    if !matches!(*placement, Placement::Done) {
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
        _ => {
            *last = Some((now, elapsed));
            // To save it if it stays put, frame or no frame meanwhile.
            alarm.wake_in(SETTLE);
        }
    }
}

/// Moves the window with the pointer when the GUI asks, while the primary
/// press is down (`settle_press` has let go of any the window will not
/// see come up), and takes that press: the window manager keeps its
/// release.
fn start_drag(
    mut requests: MessageReader<CompanionRequest>,
    buttons: Res<ButtonInput<MouseButton>>,
    mut press: ResMut<Press>,
    mut dragging: ResMut<Dragging>,
    pointer: Option<Res<GlobalPointer>>,
    mut windows: Query<&mut Window, With<PrimaryWindow>>,
    mut requests_made: WindowRequests,
) {
    // Over once the global pointer has seen the button come up; without
    // it, there is no click-through to keep off.
    if let Some(since) = dragging.0 {
        let followed = pointer
            .as_ref()
            .is_some_and(|p| p.status() == Status::Polling);
        if !followed || pointer.as_ref().is_some_and(|p| p.released_since(since)) {
            dragging.0 = None;
        }
    }
    if !requests.read().any(|r| *r == CompanionRequest::StartDrag) {
        return;
    }
    if !buttons.pressed(MouseButton::Left) {
        tracing::debug!("a drag from a press already released: ignored");
        return;
    }
    if let Ok(mut window) = windows.single_mut() {
        tracing::debug!("moving the window with the pointer");
        window.start_drag_move();
        press.taken = true;
        dragging.0 = Some(Instant::now());
        requests_made.made();
    }
}

/// Lets the pointer through the window except where, or while, the
/// companion needs it. The input box is not among the parts that hold the
/// pointer while open: it stays open until Esc or a send, and clicks
/// elsewhere go to the desktop meanwhile.
fn take_pointer(
    pointer: Option<Res<GlobalPointer>>,
    wanted: Res<PointerWanted>,
    dragging: Res<Dragging>,
    overlay: Res<Overlay>,
    settings: Option<NonSend<GuiSettings>>,
    mut cursor_options: Query<&mut CursorOptions, With<PrimaryWindow>>,
    mut requests_made: WindowRequests,
) {
    let polling = pointer.is_some_and(|p| p.status() == Status::Polling);
    let take = !polling
        || wanted.0
        || dragging.0.is_some()
        || overlay.history
        || overlay.menu
        || settings.is_some_and(|s| s.open);
    if let Ok(mut options) = cursor_options.single_mut()
        && options.hit_test != take
    {
        options.hit_test = take;
        requests_made.made();
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
                scale: 1.0,
            },
            MonitorRect {
                x: 1920,
                y: 0,
                width: 2560,
                height: 1440,
                scale: 1.0,
            },
        ]
    }

    #[test]
    fn a_window_that_stops_moving_wakes_the_app_to_save_its_place() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let woken = Arc::new(AtomicUsize::new(0));
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_time()
            .build()
            .unwrap();
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .insert_resource(Placement::Done)
            .init_resource::<CompanionPrefs>()
            .insert_resource(GuiRuntime(Arc::new(runtime)))
            .insert_resource(GuiWaker(Arc::new({
                let woken = Arc::clone(&woken);
                move || {
                    woken.fetch_add(1, Ordering::SeqCst);
                }
            })))
            .add_systems(Update, save_placement);
        app.world_mut().spawn((
            Window {
                position: WindowPosition::At(IVec2::new(10, 20)),
                ..default()
            },
            PrimaryWindow,
        ));

        // The window is seen where it now is; nothing is saved yet.
        app.update();
        assert_eq!(app.world().resource::<CompanionPrefs>().window, None);
        assert_eq!(woken.load(Ordering::SeqCst), 0);

        // With no other frame coming, the app is woken once it has settled,
        // and that frame saves it.
        std::thread::sleep(SETTLE + Duration::from_millis(300));
        assert_eq!(woken.load(Ordering::SeqCst), 1);
        app.update();
        let saved = app.world().resource::<CompanionPrefs>().window.unwrap();
        assert_eq!((saved.x, saved.y), (10, 20));
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
            scale: 1.0,
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
    fn the_default_size_and_the_margin_follow_the_primary_monitors_scale() {
        let hidpi = [MonitorRect {
            x: 0,
            y: 0,
            width: 3840,
            height: 2160,
            scale: 2.0,
        }];
        assert_eq!(
            place_on_screen(None, &hidpi, DEFAULT),
            WindowPlacement {
                x: 3840 - 840 - 64,
                y: 2160 - 1280 - 64,
                width: 840,
                height: 1280,
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
