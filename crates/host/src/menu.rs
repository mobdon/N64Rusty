//! The window's menu bar (Windows only): open a ROM, refresh rate, picture
//! shape, texture look, window size and full screen.

use winit::window::Window;

use crate::config::WindowConfig;

/// What a menu choice asks for.
#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    /// Start the program again on another ROM file.
    OpenRom(std::path::PathBuf),
    /// `Some(0.0)` is the display's rate.
    Refresh(Option<f64>),
    Widescreen(bool),
    ConsoleFilter(bool),
    /// Window size as a multiple of 320x240, leaving full screen.
    Scale(u32),
    Fullscreen,
}

/// Refresh choices: label and setting.
pub const REFRESH: [(&str, Option<f64>); 4] = [
    ("Match display", Some(0.0)),
    ("60 Hz", Some(60.0)),
    ("120 Hz", Some(120.0)),
    ("165 Hz", Some(165.0)),
];

/// ROMs played before, newest first: `recent.txt` next to the program,
/// one path per line (the player keeps it). Only files still there.
pub fn recent_roms() -> Vec<std::path::PathBuf> {
    let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
    else {
        return Vec::new();
    };
    // last-rom.txt: the one ROM older players remembered.
    let read = |f| std::fs::read_to_string(dir.join(f)).unwrap_or_default();
    let (list, old) = (read("recent.txt"), read("last-rom.txt"));
    list.lines()
        .chain(old.lines().filter(|l| !list.lines().any(|r| r == *l)))
        .map(std::path::PathBuf::from)
        .filter(|p| p.is_file())
        .collect()
}

/// Window size choices, as multiples of 320x240.
pub const SCALES: [u32; 3] = [2, 3, 4];

#[cfg(windows)]
pub use imp::Menu;

#[cfg(not(windows))]
pub struct Menu;

#[cfg(not(windows))]
impl Menu {
    pub fn new(_: &Window, _: &WindowConfig) -> Option<Menu> {
        None
    }
    pub fn poll(&self) -> Option<Action> {
        None
    }
    pub fn show(&self, _: &WindowConfig) {}
}

#[cfg(windows)]
mod imp {
    use super::*;
    use muda::{CheckMenuItem, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};

    pub struct Menu {
        _bar: muda::Menu,
        open: MenuItem,
        games: Vec<(MenuItem, std::path::PathBuf)>,
        refresh: Vec<CheckMenuItem>,
        wide: CheckMenuItem,
        normal: CheckMenuItem,
        crisp: CheckMenuItem,
        console: CheckMenuItem,
        scales: Vec<MenuItem>,
        full: MenuItem,
    }

    fn check(label: &str) -> CheckMenuItem {
        CheckMenuItem::new(label, true, false, None)
    }

    impl Menu {
        pub fn new(window: &Window, cfg: &WindowConfig) -> Option<Menu> {
            let RawWindowHandle::Win32(h) = window.window_handle().ok()?.as_raw() else {
                return None;
            };
            let m = Menu {
                _bar: muda::Menu::new(),
                open: MenuItem::new("Open ROM...", true, None),
                games: recent_roms()
                    .into_iter()
                    .map(|p| {
                        // Windows reads '&' as a shortcut marker.
                        let name = p
                            .file_stem()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .replace('&', "&&");
                        (MenuItem::new(name, true, None), p)
                    })
                    .collect(),
                refresh: REFRESH.iter().map(|(l, _)| check(l)).collect(),
                wide: check("Widescreen"),
                normal: check("Normal (4:3)"),
                crisp: check("Crisp textures"),
                console: check("Console textures"),
                scales: SCALES
                    .iter()
                    .map(|s| MenuItem::new(format!("{}x{}", 320 * s, 240 * s), true, None))
                    .collect(),
                full: MenuItem::new("Full screen (F11)", true, None),
            };
            let sep0 = PredefinedMenuItem::separator();
            let mut game: Vec<&dyn muda::IsMenuItem> = vec![&m.open];
            if !m.games.is_empty() {
                game.push(&sep0);
            }
            game.extend(m.games.iter().map(|(i, _)| i as &dyn muda::IsMenuItem));
            let game = Submenu::with_items("Game", true, &game).ok()?;
            let refresh: Vec<&dyn muda::IsMenuItem> = m
                .refresh
                .iter()
                .map(|i| i as &dyn muda::IsMenuItem)
                .collect();
            let sep = PredefinedMenuItem::separator();
            let sep2 = PredefinedMenuItem::separator();
            let mut view: Vec<&dyn muda::IsMenuItem> =
                vec![&m.wide, &m.normal, &sep, &m.crisp, &m.console, &sep2];
            view.extend(m.scales.iter().map(|i| i as &dyn muda::IsMenuItem));
            view.push(&m.full);
            let refresh = Submenu::with_items("Refresh", true, &refresh).ok()?;
            let view = Submenu::with_items("View", true, &view).ok()?;
            m._bar.append_items(&[&game, &refresh, &view]).ok()?;
            // SAFETY: the window outlives the menu, which the app drops first.
            unsafe { m._bar.init_for_hwnd(h.hwnd.get()) }.ok()?;
            m.show(cfg);
            Some(m)
        }

        /// The next menu choice, if one was made.
        pub fn poll(&self) -> Option<Action> {
            let id = MenuEvent::receiver().try_recv().ok()?.id;
            if id == *self.open.id() {
                let rom = rfd::FileDialog::new()
                    .set_title("Pick your N64 ROM")
                    .add_filter("N64 ROM", &["z64", "n64", "v64"])
                    .pick_file()?;
                return Some(Action::OpenRom(rom));
            }
            if let Some((_, rom)) = self.games.iter().find(|(i, _)| *i.id() == id) {
                return Some(Action::OpenRom(rom.clone()));
            }
            if let Some(k) = self.refresh.iter().position(|i| *i.id() == id) {
                return Some(Action::Refresh(REFRESH[k].1));
            }
            if let Some(k) = self.scales.iter().position(|i| *i.id() == id) {
                return Some(Action::Scale(SCALES[k]));
            }
            Some(match () {
                _ if id == *self.wide.id() => Action::Widescreen(true),
                _ if id == *self.normal.id() => Action::Widescreen(false),
                _ if id == *self.crisp.id() => Action::ConsoleFilter(false),
                _ if id == *self.console.id() => Action::ConsoleFilter(true),
                _ if id == *self.full.id() => Action::Fullscreen,
                _ => return None,
            })
        }

        /// Ticks the choices `cfg` makes.
        pub fn show(&self, cfg: &WindowConfig) {
            for (item, (_, r)) in self.refresh.iter().zip(REFRESH) {
                item.set_checked(cfg.refresh == r);
            }
            self.wide.set_checked(cfg.widescreen);
            self.normal.set_checked(!cfg.widescreen);
            self.crisp.set_checked(!cfg.console_filter);
            self.console.set_checked(cfg.console_filter);
        }
    }
}
