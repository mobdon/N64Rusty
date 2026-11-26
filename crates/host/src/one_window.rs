//! One window for the whole program: the start screen and the game share
//! one event loop (run again for each) and the window itself, so picking a
//! game plays it in the window the start screen was in.
//!
//! A game cannot yet be unloaded in the same program, so going from a game
//! to the start screen or another game starts the program again; the new
//! window then opens exactly where the old one was (see [`hand_over`]) and
//! the old one closes once the new one is up.

use std::cell::RefCell;
use std::sync::Arc;

use winit::application::ApplicationHandler;
use winit::dpi::{PhysicalPosition, PhysicalSize};
use winit::event_loop::EventLoop;
use winit::platform::run_on_demand::EventLoopExtRunOnDemand;
use winit::window::{Fullscreen, Window, WindowAttributes};

thread_local! {
    static LOOP: RefCell<Option<EventLoop<()>>> = const { RefCell::new(None) };
    static KEPT: RefCell<Option<Arc<Window>>> = const { RefCell::new(None) };
}

/// Where the program started again gets its window from the old one.
const PLACE: &str = "N64RUSTY_WINDOW_PLACE";

/// Runs `app` on the program's one event loop until it exits.
pub(crate) fn run<A: ApplicationHandler>(app: &mut A) -> Result<(), String> {
    LOOP.with_borrow_mut(|l| {
        if l.is_none() {
            *l = Some(EventLoop::new().map_err(|e| e.to_string())?);
        }
        let l = l.as_mut().expect("just set");
        l.run_app_on_demand(app).map_err(|e| e.to_string())
    })
}

/// Keeps `window` open for the next screen, instead of closing it.
pub(crate) fn keep(window: Arc<Window>) {
    KEPT.set(Some(window));
}

/// The window the last screen kept, if any.
pub(crate) fn take() -> Option<Arc<Window>> {
    KEPT.take()
}

/// `attrs` placed where the program's previous window was, when it was
/// started again from one (see [`hand_over`]).
pub(crate) fn placed(attrs: WindowAttributes) -> WindowAttributes {
    let Some(p) = std::env::var(PLACE).ok().and_then(|s| parse(&s)) else {
        return attrs;
    };
    let (x, y, w, h, full, max) = p;
    attrs
        .with_position(PhysicalPosition::new(x, y))
        .with_inner_size(PhysicalSize::new(w, h))
        .with_maximized(max)
        .with_fullscreen(full.then_some(Fullscreen::Borderless(None)))
}

type Place = (i32, i32, u32, u32, bool, bool);

fn parse(s: &str) -> Option<Place> {
    let v: Vec<i64> = s
        .split_whitespace()
        .map(str::parse)
        .collect::<Result<_, _>>()
        .ok()?;
    let [x, y, w, h, full, max] = v[..] else {
        return None;
    };
    Some((
        x.try_into().ok()?,
        y.try_into().ok()?,
        w.try_into().ok()?,
        h.try_into().ok()?,
        full != 0,
        max != 0,
    ))
}

fn format(p: Place) -> String {
    let (x, y, w, h, full, max) = p;
    format!("{x} {y} {w} {h} {} {}", full as u8, max as u8)
}

/// Starts the program again with `args`, its window where `window` is.
/// Returns once the new program's window is up (or after a few seconds),
/// so the caller can close its own without a gap.
pub(crate) fn hand_over(
    window: &Window,
    args: impl IntoIterator<Item = std::ffi::OsString>,
) -> std::io::Result<()> {
    let pos = window.outer_position().unwrap_or_default();
    let size = window.inner_size();
    let place = format((
        pos.x,
        pos.y,
        size.width,
        size.height,
        window.fullscreen().is_some(),
        window.is_maximized(),
    ));
    let child = std::process::Command::new(std::env::current_exe()?)
        .args(args)
        .env(PLACE, place)
        .spawn()?;
    wait_for_window(&child);
    Ok(())
}

#[cfg(windows)]
fn wait_for_window(child: &std::process::Child) {
    use std::os::windows::io::AsRawHandle;
    // Returns when the new program first waits for input: its window is up.
    // SAFETY: the handle is the child's, open for as long as `child` lives.
    unsafe {
        windows_sys::Win32::System::Threading::WaitForInputIdle(child.as_raw_handle() as _, 10_000);
    }
}

#[cfg(not(windows))]
fn wait_for_window(_: &std::process::Child) {}

#[test]
fn place_round_trips() {
    let p = (-8, 30, 1280, 960, false, true);
    assert_eq!(parse(&format(p)), Some(p));
    assert_eq!(parse("1 2 3"), None);
    assert_eq!(parse("1 2 -3 4 0 0"), None);
}
