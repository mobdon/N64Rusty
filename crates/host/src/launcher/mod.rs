//! The player's own screens in an 8-bit style, drawn at 320x240 and scaled
//! up in whole pixels: Games (the ROMs played before as tiles), Mods (turn
//! mods and their options on or off), Settings (the window choices) and a
//! message screen for errors. W/S/A/D or the mouse move, Q/E or a click
//! changes page, Enter, Space or a click chooses, Escape closes.
//!
//! One file per job: `ui` is the state and navigation, `layout` where
//! things sit, `canvas` and `draw` the pixels, `screens` the window,
//! `settings` the files the choices are saved to, `icon` the window icon
//! and `dialogs` the system's file dialog and file manager.

mod canvas;
mod dialogs;
mod draw;
mod icon;
mod layout;
mod palette;
mod screens;
mod settings;
mod ui;

use std::path::PathBuf;

pub(crate) use dialogs::open_folder;
pub use icon::icon;
pub use layout::{H, W};
pub use palette::{BG, EDGE, GOLD, GREEN, PANEL, RED, SKY, TEXT};
pub use ui::{Page, Ui};

use crate::config::RunConfig;
use screens::Screens;

/// Shows the screens on `page`; returns the ROM picked, or `None` when
/// closed (or when no window could open). Settings that could not be read
/// are said first, over the page.
pub fn pick(roms: &[PathBuf], page: Page) -> Option<PathBuf> {
    let (settings, error) = settings::load_settings();
    let ui = Ui {
        page,
        sel: 0,
        games: roms.to_vec(),
        settings,
        message: error.map(|e| ("SETTINGS NOT READ".into(), e)),
    };
    run(ui)
}

/// Shows a message (an error) in the same style until it is closed.
/// Returns false when no window could open.
pub fn message(head: &str, body: &str) -> bool {
    let ui = Ui {
        page: Page::Games,
        sel: 0,
        games: Vec::new(),
        settings: RunConfig::default(),
        message: Some((head.to_uppercase(), body.to_string())),
    };
    let mut app = Screens::new(ui, true);
    let shown = crate::one_window::run(&mut app).is_ok() && app.shown;
    // Read and dismissed, not closed: the next screen uses this window.
    app.let_go();
    if let (false, Some(w)) = (app.closed, app.window.take()) {
        crate::one_window::keep(w);
    }
    shown
}

/// Picking a game keeps the window open for it (see [`crate::one_window`]).
fn run(ui: Ui) -> Option<PathBuf> {
    let mut app = Screens::new(ui, false);
    crate::one_window::run(&mut app).ok()?;
    let picked = app.picked.take()?;
    app.let_go();
    if let Some(w) = app.window.take() {
        crate::one_window::keep(w);
    }
    Some(picked)
}
