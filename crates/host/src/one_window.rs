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
    /// The game ended by itself with this error (a crash): tell the player
    /// and go back to the start screen.
    Crashed(String),
}

/// Set by the game's own thread when it ends by itself, so the window
/// leaves the game instead of the whole program exiting.
static OVER: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// Ends the game in the window with `error` (see [`Next::Crashed`]).
pub fn game_over(error: String) {
    *OVER.lock().unwrap_or_else(|e| e.into_inner()) = Some(error);
}

/// The error a game ended with, if it did.
pub(crate) fn take_over() -> Option<String> {
    OVER.lock().unwrap_or_else(|e| e.into_inner()).take()
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
            #[cfg(target_os = "android")]
            let new = {
                use winit::platform::android::EventLoopBuilderExtAndroid;
                EventLoop::builder()
                    .with_android_app(crate::android::app())
                    .build()
            };
            #[cfg(not(target_os = "android"))]
            let new = EventLoop::new();
            *l = Some(new.map_err(|e| e.to_string())?);
        }
        let l = l.as_mut().expect("just set");
        l.run_app_on_demand(app).map_err(|e| e.to_string())
    })
}

/// Keeps `window` open for the next screen, instead of closing it.
pub(crate) fn keep(window: Arc<Window>) {
    KEPT.set(Some(window));
}

/// Whether a screen can open its window as its event loop starts. On
/// Android the app's window only exists once winit says `resumed`, which
/// comes on the first run only; later runs reuse the kept window.
pub(crate) fn ready() -> bool {
    !cfg!(target_os = "android") || kept()
}

/// Whether the last screen kept its window open for the next one.
pub fn kept() -> bool {
    KEPT.with_borrow(Option::is_some)
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
