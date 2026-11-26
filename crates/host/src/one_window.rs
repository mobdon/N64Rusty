//! One window for the whole program: the start screen and the games share
//! one event loop (run again for each) and the window itself, so picking a
//! game, going back to the start screen or another game all happen in the
//! same window, which screen capture keeps seeing.

use std::cell::RefCell;
use std::path::PathBuf;
use std::sync::Arc;

use winit::application::ApplicationHandler;
use winit::event_loop::EventLoop;
use winit::platform::run_on_demand::EventLoopExtRunOnDemand;
use winit::window::Window;

/// Where a game asked the program to go next, from its menu.
#[derive(Clone, Debug, PartialEq)]
pub enum Next {
    /// Play this ROM.
    Rom(PathBuf),
    /// Show this page of the start screen: `games`, `mods` or `settings`.
    Screen(&'static str),
    /// Play the same game again (mods install at start).
    Again,
}

thread_local! {
    static LOOP: RefCell<Option<EventLoop<()>>> = const { RefCell::new(None) };
    static KEPT: RefCell<Option<Arc<Window>>> = const { RefCell::new(None) };
    static NEXT: RefCell<Option<Next>> = const { RefCell::new(None) };
}

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

/// Keeps `window` and asks the program to go to `next` once the game's
/// event loop has exited.
pub(crate) fn switch(window: Arc<Window>, next: Next) {
    keep(window);
    NEXT.set(Some(next));
}

/// Whether the game that just ended asked to go somewhere next.
pub fn switching() -> bool {
    NEXT.with_borrow(Option::is_some)
}

/// Where the game that just ended asked to go, if anywhere.
pub fn take_next() -> Option<Next> {
    NEXT.take()
}
