//! What the screens ask of the system: a file dialog for a ROM, and the
//! file manager for a folder.

use std::path::{Path, PathBuf};

/// Asks for a ROM file; `None` when cancelled or where there is no dialog.
#[cfg(windows)]
pub(super) fn open_rom() -> Option<PathBuf> {
    rfd::FileDialog::new()
        .set_title("Pick your N64 ROM")
        .add_filter("N64 ROM", &["z64", "n64", "v64"])
        .pick_file()
}

/// No file dialog on this system.
#[cfg(not(windows))]
pub(super) fn open_rom() -> Option<PathBuf> {
    None
}

/// Makes `dir` if needed and opens it in the file manager (Windows only;
/// elsewhere it is only made).
pub(crate) fn open_folder(dir: &Path) {
    if let Err(e) = std::fs::create_dir_all(dir) {
        eprintln!("warning: making {}: {e}", dir.display());
    }
    #[cfg(windows)]
    if let Err(e) = std::process::Command::new("explorer").arg(dir).spawn() {
        eprintln!("warning: opening {}: {e}", dir.display());
    }
}
