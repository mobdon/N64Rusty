//! Host side of a recompiled game: the window, controller input from the
//! keyboard and gamepads, scripted input with record and replay, the
//! per-run config file and a debug port for stepping frames.
//!
//! Tests use [`run_headless`] or [`Session`] directly: no window, no focus,
//! no sound.

pub mod config;
pub mod debug;
pub mod launcher;
pub mod mapping;
pub mod menu;
pub mod pad;
pub mod script;
pub mod session;
pub mod window;

pub use config::RunConfig;
pub use pad::{Pad, Pads};
pub use session::{run_headless, Guest, Session, Tick};
pub use window::run_windowed;
