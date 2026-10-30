//! The pointer anywhere on the desktop, polled off bevy's thread.
//!
//! A click-through window gets no cursor events, so a thread asks the
//! display server where the pointer is every [`POLL`] and keeps the answer
//! in [`GlobalPointer`]. Polling costs no frame: the thread wakes bevy (the
//! GUI's [`Waker`]) only when the answer matters to the window, that is
//! when the pointer enters, moves inside or leaves the window, or the
//! primary button goes down or up. With the pointer elsewhere the app
//! draws nothing.
//!
//! - Linux: Xlib through `x11-dl`, loaded at run time, so a desktop without
//!   libX11 still starts (and goes without click-through).
//! - Windows and macOS: `device_query`'s mouse query. On macOS that reads
//!   the pointer through `CGEventCreate` / `CGEventSourceButtonState`,
//!   which need no Accessibility permission (only its keyboard query
//!   does), so the state is made without the permission check. Untested
//!   on a Mac.
//! - Elsewhere, or when the backend cannot start: unavailable.

use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant};

use bevy::math::{IVec2, UVec2, Vec2};
use bevy::prelude::Resource;
use sapphire_agent_gui::Waker;

/// How often the thread asks where the pointer is.
pub const POLL: Duration = Duration::from_millis(50);

/// The window on the desktop, in physical pixels, and its scale factor.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WindowRect {
    pub origin: IVec2,
    pub size: UVec2,
    pub scale: f32,
}

impl WindowRect {
    /// `global` (physical desktop pixels) in the window's logical pixels;
    /// `None` outside the window.
    pub fn local(&self, global: Vec2) -> Option<Vec2> {
        let physical = global - self.origin.as_vec2();
        let inside = physical.cmpge(Vec2::ZERO).all() && physical.cmplt(self.size.as_vec2()).all();
        inside.then(|| physical / self.scale)
    }
}

/// What the thread last saw.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Sample {
    /// The pointer in the window's logical pixels; `None` outside it.
    pub at: Option<Vec2>,
    /// The primary button is down, wherever the pointer is.
    pub primary_down: bool,
}

/// Whether going from `before` to `now` is worth a frame: the pointer
/// entered, moved inside or left the window, or the primary button went
/// down or up. Moving about elsewhere is not.
pub fn needs_wake(before: &Sample, now: &Sample) -> bool {
    before != now
}

/// Whether the pointer can be followed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    /// The thread is still opening its backend.
    Starting,
    Polling,
    Unavailable,
}

#[derive(Debug)]
struct Shared {
    status: Status,
    window: Option<WindowRect>,
    sample: Sample,
    /// When `sample` was taken.
    sampled_at: Option<Instant>,
}

/// The global pointer, as the polling thread last saw it.
#[derive(Resource, Clone)]
pub struct GlobalPointer(Arc<Mutex<Shared>>);

impl GlobalPointer {
    /// Starts the polling thread, which wakes bevy with `waker`. It stops
    /// once this (and every clone) is dropped.
    pub fn start(waker: Waker) -> Self {
        let pointer = Self::with_status(Status::Starting);
        let shared = Arc::downgrade(&pointer.0);
        let spawned = std::thread::Builder::new()
            .name("companion-pointer".into())
            .spawn(move || poll(shared, waker));
        if let Err(e) = spawned {
            tracing::warn!("cannot start the pointer thread: {e}");
            pointer.lock().status = Status::Unavailable;
        }
        pointer
    }

    /// A pointer that cannot be followed (Wayland).
    pub fn unavailable() -> Self {
        Self::with_status(Status::Unavailable)
    }

    fn with_status(status: Status) -> Self {
        Self(Arc::new(Mutex::new(Shared {
            status,
            window: None,
            sample: Sample::default(),
            sampled_at: None,
        })))
    }

    fn lock(&self) -> MutexGuard<'_, Shared> {
        lock(&self.0)
    }

    pub fn status(&self) -> Status {
        self.lock().status
    }

    /// Where the window is now; `None` while that is not known.
    pub fn set_window(&self, window: Option<WindowRect>) {
        self.lock().window = window;
    }

    pub fn sample(&self) -> Sample {
        self.lock().sample
    }

    /// The primary button is up in a sample taken after `since`: a press
    /// made at `since` or before has been released, wherever that was.
    /// `false` while the pointer is not followed, or no sample that recent
    /// has come in yet.
    pub fn released_since(&self, since: Instant) -> bool {
        let shared = self.lock();
        shared.status == Status::Polling
            && shared.sampled_at.is_some_and(|at| at > since)
            && !shared.sample.primary_down
    }
}

/// A poisoned lock still holds a usable sample.
fn lock(shared: &Mutex<Shared>) -> MutexGuard<'_, Shared> {
    shared.lock().unwrap_or_else(|e| e.into_inner())
}

/// The thread: polls until [`GlobalPointer`] is gone.
fn poll(shared: Weak<Mutex<Shared>>, waker: Waker) {
    let backend = backend::Backend::open();
    let Some(this) = shared.upgrade() else {
        return;
    };
    let Some(backend) = backend else {
        tracing::warn!(
            "cannot read the global pointer: the companion window takes every click \
             (no click-through)"
        );
        lock(&this).status = Status::Unavailable;
        drop(this);
        waker();
        return;
    };
    lock(&this).status = Status::Polling;
    drop(this);
    waker();

    loop {
        std::thread::sleep(POLL);
        let Some(this) = shared.upgrade() else {
            return;
        };
        // Stamped before asking, so a sample never claims to be newer
        // than what it saw.
        let asked_at = Instant::now();
        let Some((global, primary_down)) = backend.query() else {
            continue;
        };
        let wake = {
            let mut s = lock(&this);
            let at = s
                .window
                .and_then(|w| w.local(backend::to_physical(global, w.scale)));
            let now = Sample { at, primary_down };
            let wake = needs_wake(&s.sample, &now);
            s.sample = now;
            s.sampled_at = Some(asked_at);
            wake
        };
        if wake {
            waker();
        }
    }
}

#[cfg(target_os = "linux")]
mod backend {
    use std::os::raw::{c_int, c_uint, c_ulong};
    use std::ptr;

    use bevy::math::Vec2;
    use x11_dl::xlib;

    /// A connection of its own to the X server, through a libX11 loaded at
    /// run time.
    pub struct Backend {
        xlib: xlib::Xlib,
        display: *mut xlib::Display,
        root: c_ulong,
    }

    impl Backend {
        pub fn open() -> Option<Self> {
            let xlib = xlib::Xlib::open().ok()?;
            // SAFETY: a null name opens `$DISPLAY`; the result is checked.
            let display = unsafe { (xlib.XOpenDisplay)(ptr::null()) };
            if display.is_null() {
                return None;
            }
            // SAFETY: `display` is open.
            let root = unsafe { (xlib.XDefaultRootWindow)(display) };
            Some(Self {
                xlib,
                display,
                root,
            })
        }

        /// The pointer in physical desktop pixels, and whether the primary
        /// button is down.
        pub fn query(&self) -> Option<(Vec2, bool)> {
            let (mut root, mut child): (c_ulong, c_ulong) = (0, 0);
            let (mut x, mut y, mut wx, mut wy): (c_int, c_int, c_int, c_int) = (0, 0, 0, 0);
            let mut mask: c_uint = 0;
            // SAFETY: `display` is open, used by this thread only, and every
            // out-pointer is a live local.
            let same_screen = unsafe {
                (self.xlib.XQueryPointer)(
                    self.display,
                    self.root,
                    &mut root,
                    &mut child,
                    &mut x,
                    &mut y,
                    &mut wx,
                    &mut wy,
                    &mut mask,
                )
            };
            (same_screen != 0)
                .then(|| (Vec2::new(x as f32, y as f32), mask & xlib::Button1Mask != 0))
        }
    }

    impl Drop for Backend {
        fn drop(&mut self) {
            // SAFETY: opened in `open`, closed once.
            unsafe { (self.xlib.XCloseDisplay)(self.display) };
        }
    }

    /// X reports physical pixels already.
    pub fn to_physical(global: Vec2, _scale: f32) -> Vec2 {
        global
    }
}

#[cfg(any(target_os = "windows", target_os = "macos"))]
mod backend {
    use bevy::math::Vec2;
    use device_query::{DeviceQuery, DeviceState};

    pub struct Backend(DeviceState);

    impl Backend {
        /// Always opens. Not `DeviceState::checked_new`: on macOS that
        /// requires (and prompts for) the Accessibility permission, which
        /// only the keyboard query needs; without it the window would never
        /// let a click through. `DeviceState` is a unit struct on both
        /// platforms, holding nothing to set up.
        pub fn open() -> Option<Self> {
            Some(Self(DeviceState))
        }

        /// The pointer in the backend's desktop units, and whether the
        /// primary button (1-based index 1) is down.
        pub fn query(&self) -> Option<(Vec2, bool)> {
            let mouse = self.0.get_mouse();
            let primary = mouse.button_pressed.get(1).copied().unwrap_or(false);
            Some((
                Vec2::new(mouse.coords.0 as f32, mouse.coords.1 as f32),
                primary,
            ))
        }
    }

    /// Windows reports physical pixels.
    #[cfg(target_os = "windows")]
    pub fn to_physical(global: Vec2, _scale: f32) -> Vec2 {
        global
    }

    /// macOS reports points (logical); the window's position is physical.
    /// Scaled by the window's own factor, which holds while the pointer is
    /// on the window's monitor, the only place it matters. Untested.
    #[cfg(target_os = "macos")]
    pub fn to_physical(global: Vec2, scale: f32) -> Vec2 {
        global * scale
    }
}

#[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
mod backend {
    use bevy::math::Vec2;

    pub enum Backend {}

    impl Backend {
        pub fn open() -> Option<Self> {
            None
        }

        pub fn query(&self) -> Option<(Vec2, bool)> {
            match *self {}
        }
    }

    pub fn to_physical(global: Vec2, _scale: f32) -> Vec2 {
        global
    }
}

#[cfg(test)]
mod tests {
    use bevy::math::{IVec2, UVec2, Vec2};

    use super::*;

    const RECT: WindowRect = WindowRect {
        origin: IVec2::new(100, 200),
        size: UVec2::new(840, 1280),
        scale: 2.0,
    };

    #[test]
    fn the_global_pointer_in_window_logical_pixels() {
        assert_eq!(RECT.local(Vec2::new(100.0, 200.0)), Some(Vec2::ZERO));
        assert_eq!(
            RECT.local(Vec2::new(500.0, 600.0)),
            Some(Vec2::new(200.0, 200.0))
        );
        // Outside, on any side.
        assert_eq!(RECT.local(Vec2::new(99.0, 300.0)), None);
        assert_eq!(RECT.local(Vec2::new(940.0, 300.0)), None);
        assert_eq!(RECT.local(Vec2::new(300.0, 1480.0)), None);
    }

    #[test]
    fn bevy_wakes_only_when_the_pointer_matters() {
        let outside = Sample {
            at: None,
            primary_down: false,
        };
        let inside = |x: f32| Sample {
            at: Some(Vec2::new(x, 10.0)),
            primary_down: false,
        };
        // Moving about elsewhere on the desktop: nothing to do.
        assert!(!needs_wake(&outside, &outside));
        // Entering, moving inside, leaving.
        assert!(needs_wake(&outside, &inside(1.0)));
        assert!(needs_wake(&inside(1.0), &inside(2.0)));
        assert!(!needs_wake(&inside(2.0), &inside(2.0)));
        assert!(needs_wake(&inside(2.0), &outside));
        // The primary button, wherever the pointer is.
        let pressed = Sample {
            primary_down: true,
            ..outside
        };
        assert!(needs_wake(&outside, &pressed));
        assert!(needs_wake(&pressed, &outside));
    }
}
