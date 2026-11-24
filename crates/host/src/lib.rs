//! Host side of a recompiled N64 game: the parts that talk to the player's
//! machine rather than to the guest.
//!
//! - [`pad`]: controller state for the four ports.
//! - [`keymap`]: keyboard bindings (WASD moves the stick; arrow keys are never used).
//! - [`script`]: input scripts anchored on frame numbers, recorded and replayed.
//! - [`config`]: one config file describes one run.
//! - [`debug`]: a localhost port that lets a test step the game frame by frame.
//! - [`session`]: ties these together; [`session::run_headless`] needs no window.
//! - `window` (feature `window`): a winit window with frame pacing, ready for a
//!   wgpu surface. Never opened by tests.

pub mod config;
pub mod debug;
#[cfg(feature = "gamepad")]
pub mod gamepad;
pub mod keymap;
pub mod pacer;
pub mod pad;
pub mod script;
pub mod session;
#[cfg(feature = "window")]
pub mod window;

pub use config::RunConfig;
pub use pad::{Pad, Pads};
pub use session::{run_headless, run_session, Session};
