//! Host side of a recompiled game: the window, controller input from the
//! keyboard and gamepads, scripted input with record and replay, the
//! per-run config file and a debug port for stepping frames.
//!
//! Tests use [`run_headless`] or [`Session`] directly: no window, no focus,
//! no sound.

#[cfg(target_os = "android")]
pub mod android;
pub mod config;
pub mod debug;
pub mod launcher;
pub mod mapping;
pub mod menu;
pub mod one_window;
pub mod pad;
pub mod script;
pub mod session;
pub mod window;

pub use config::RunConfig;
pub use pad::{Pad, Pads};
pub use session::{run_headless, Guest, Session, Tick};
pub use window::run_windowed;

/// The player's own folder: ROMs, settings, saves and `mods` live here.
/// The program's folder on a computer; the app's files folder on Android.
pub fn home() -> Option<std::path::PathBuf> {
    #[cfg(target_os = "android")]
    return android::home();
    #[cfg(not(target_os = "android"))]
    std::env::current_exe()
        .ok()?
        .parent()
        .map(|d| d.to_path_buf())
}
