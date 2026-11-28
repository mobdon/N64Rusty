//! The window's menu bar (Windows only): open a ROM, refresh rate, picture
//! shape, texture look, window size and full screen.

use winit::window::Window;

use crate::config::WindowConfig;

/// What a menu choice asks for.
#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    /// Start the program again on another ROM file.
    OpenRom(std::path::PathBuf),
    /// Start the program again on one page of its start screen: `games`,
    /// `mods` or `settings`.
    Screen(&'static str),
    /// Draw this often; `Some(0.0)` is the display's rate.
    Refresh(Option<f64>),
    /// Widescreen on (true) or the console's 4:3 (false).
    Widescreen(bool),
    /// Textures as the console filters them (true) or crisp (false).
    ConsoleFilter(bool),
    /// Window size as a multiple of 320x240, leaving full screen.
    Scale(u32),
    /// Full screen on or off.
    Fullscreen,
    /// Turn the mod file on or off and start the program again.
    ToggleMod(std::path::PathBuf),
    /// Flip one `name = on|off` line of a mod's options file and start the
    /// program again.
    ToggleModOption(std::path::PathBuf, String),
    /// Open the `mods` folder in the file manager.
    OpenModsFolder,
}

/// Set by a program with a start screen (the player): the Game menu then
/// offers "All games...", which starts the program again with no ROM.
pub static START_SCREEN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The `mods` folder next to the program.
pub fn mods_dir() -> Option<std::path::PathBuf> {
    Some(crate::home()?.join("mods"))
}

/// Mod files in [`mods_dir`] with whether each is on (a name ending in
/// `.off` is off), sorted by name.
pub fn mods() -> Vec<(std::path::PathBuf, bool)> {
    let Some(Ok(dir)) = mods_dir().map(std::fs::read_dir) else {
        return Vec::new();
    };
    let lib = |n: &str| [".dll", ".so", ".dylib"].iter().any(|x| n.ends_with(x));
    let mut v: Vec<_> = dir
        .flatten()
        .map(|e| e.path())
        .filter_map(|p| {
            let n = p.file_name()?.to_string_lossy().into_owned();
            match n.strip_suffix(".off") {
                Some(base) if lib(base) => Some((p, false)),
                _ if lib(&n) => Some((p, true)),
                _ => None,
            }
        })
        .collect();
    v.sort();
    v
}

/// A mod's name: its file name without `.off` and the library extension.
fn mod_name(path: &std::path::Path) -> String {
    let n = path.file_name().unwrap_or_default().to_string_lossy();
    let base = n.trim_end_matches(".off");
    base.rsplit_once('.').map_or(base, |(a, _)| a).to_string()
}

/// A mod's options file: `<mod name>.txt` beside it, which the mod writes.
pub fn mod_options_file(path: &std::path::Path) -> std::path::PathBuf {
    path.with_file_name(format!("{}.txt", mod_name(path)))
}

/// The `name = on|off` lines of an options file; other lines are ignored.
pub fn mod_options(text: &str) -> Vec<(String, bool)> {
    text.lines()
        .filter_map(|l| {
            let (k, v) = l.split_once('=')?;
            match v.trim() {
                "on" => Some((k.trim().to_string(), true)),
                "off" => Some((k.trim().to_string(), false)),
                _ => None,
            }
        })
        .collect()
}

/// `text` with option `name` flipped between on and off.
pub fn flip_mod_option(text: &str, name: &str) -> String {
    text.lines()
        .map(|l| match l.split_once('=') {
            Some((k, v)) if k.trim() == name && ["on", "off"].contains(&v.trim()) => {
                let to = if v.trim() == "on" { "off" } else { "on" };
                format!("{} = {to}", k.trim())
            }
            _ => l.to_string(),
        })
        .map(|l| l + "\n")
        .collect()
}

/// Flips option `name` in the options file at `file`, keeping its other
/// lines. A missing file counts as empty; any other read error leaves the
/// file untouched and is returned, so a bad read never wipes the options.
pub fn toggle_mod_option(file: &std::path::Path, name: &str) -> std::io::Result<()> {
    let text = match std::fs::read_to_string(file) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e),
    };
    std::fs::write(file, flip_mod_option(&text, name))
}

/// The file name that turns the mod at `path` on or off.
pub fn toggled(path: &std::path::Path) -> std::path::PathBuf {
    let n = path.file_name().unwrap_or_default().to_string_lossy();
    match n.strip_suffix(".off") {
        Some(base) => path.with_file_name(base),
        None => path.with_file_name(format!("{n}.off")),
    }
}

/// Refresh choices: label and setting. Android also offers its default,
/// one picture per game frame; on PC that setting could not be saved (a
/// missing `refresh` loads as the PC default), so it is not offered there.
pub const REFRESH: &[(&str, Option<f64>)] = if cfg!(target_os = "android") {
    &[
        ("Match display", Some(0.0)),
        ("60 Hz", Some(60.0)),
        ("120 Hz", Some(120.0)),
        ("165 Hz", Some(165.0)),
        ("Game rate", None),
    ]
} else {
    &[
        ("Match display", Some(0.0)),
        ("60 Hz", Some(60.0)),
        ("120 Hz", Some(120.0)),
        ("165 Hz", Some(165.0)),
    ]
};

/// ROMs played before, newest first: `recent.txt` next to the program,
/// one path per line (the player keeps it). Only files still there.
pub fn recent_roms() -> Vec<std::path::PathBuf> {
    let Some(dir) = crate::home() else {
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

/// No menu bar on this system.
#[cfg(not(windows))]
pub struct Menu;

#[cfg(not(windows))]
impl Menu {
    /// Never a menu here.
    pub fn new(_: &Window, _: &WindowConfig) -> Option<Menu> {
        None
    }
    /// Never a choice here.
    pub fn poll(&self) -> Option<Action> {
        None
    }
    /// Nothing to tick here.
    pub fn show(&self, _: &WindowConfig) {}
}

#[cfg(windows)]
mod imp {
    use super::*;
    use muda::{CheckMenuItem, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};

    /// The window's menu bar and its items.
    pub struct Menu {
        _bar: muda::Menu,
        open: MenuItem,
        list: MenuItem,
        mods_page: MenuItem,
        settings_page: MenuItem,
        games: Vec<(MenuItem, std::path::PathBuf)>,
        refresh: Vec<CheckMenuItem>,
        wide: CheckMenuItem,
        normal: CheckMenuItem,
        crisp: CheckMenuItem,
        console: CheckMenuItem,
        scales: Vec<MenuItem>,
        full: MenuItem,
        mods: Vec<(CheckMenuItem, std::path::PathBuf)>,
        /// Each mod's options: item, options file, option name.
        mod_options: Vec<(CheckMenuItem, std::path::PathBuf, String)>,
        mods_folder: MenuItem,
    }

    fn check(label: &str) -> CheckMenuItem {
        CheckMenuItem::new(label, true, false, None)
    }

    impl Menu {
        /// Adds the menu bar to `window`, ticked as `cfg` says; `None` when
        /// it is not a Win32 window or the bar cannot be made.
        pub fn new(window: &Window, cfg: &WindowConfig) -> Option<Menu> {
            let RawWindowHandle::Win32(h) = window.window_handle().ok()?.as_raw() else {
                return None;
            };
            let found = mods();
            let m = Menu {
                _bar: muda::Menu::new(),
                open: MenuItem::new("Open ROM...", true, None),
                list: MenuItem::new("All games...", true, None),
                mods_page: MenuItem::new("Mods screen...", true, None),
                settings_page: MenuItem::new("Settings screen...", true, None),
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
                    .map(|s| {
                        let (w, h) = (crate::launcher::W as u32, crate::launcher::H as u32);
                        MenuItem::new(format!("{}x{}", w * s, h * s), true, None)
                    })
                    .collect(),
                full: MenuItem::new("Full screen (F11)", true, None),
                mods: found
                    .iter()
                    .map(|(p, on)| {
                        let name = mod_name(p).replace('&', "&&");
                        (CheckMenuItem::new(name, true, *on, None), p.clone())
                    })
                    .collect(),
                mod_options: found
                    .into_iter()
                    .flat_map(|(p, on)| {
                        let file = mod_options_file(&p);
                        let text = std::fs::read_to_string(&file).unwrap_or_default();
                        mod_options(&text).into_iter().map(move |(k, v)| {
                            let label = format!("    {}", k.replace('&', "&&"));
                            (CheckMenuItem::new(label, on, v, None), file.clone(), k)
                        })
                    })
                    .collect(),
                mods_folder: MenuItem::new("Open mods folder", true, None),
            };
            let sep0 = PredefinedMenuItem::separator();
            let mut game: Vec<&dyn muda::IsMenuItem> = vec![&m.open];
            let screens = super::START_SCREEN.load(std::sync::atomic::Ordering::Relaxed);
            if screens {
                game.insert(0, &m.list);
            }
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
            let sep4 = PredefinedMenuItem::separator();
            if screens {
                view.extend([&sep4 as &dyn muda::IsMenuItem, &m.settings_page]);
            }
            let refresh = Submenu::with_items("Refresh", true, &refresh).ok()?;
            let view = Submenu::with_items("View", true, &view).ok()?;
            let sep3 = PredefinedMenuItem::separator();
            let mut modm: Vec<&dyn muda::IsMenuItem> = Vec::new();
            for (i, p) in &m.mods {
                modm.push(i);
                let file = mod_options_file(p);
                modm.extend(
                    m.mod_options
                        .iter()
                        .filter(|(_, f, _)| *f == file)
                        .map(|(o, _, _)| o as &dyn muda::IsMenuItem),
                );
            }
            if !m.mods.is_empty() {
                modm.push(&sep3);
            }
            modm.push(&m.mods_folder);
            if screens {
                modm.push(&m.mods_page);
            }
            let modm = Submenu::with_items("Mods", true, &modm).ok()?;
            m._bar.append_items(&[&game, &refresh, &view, &modm]).ok()?;
            // SAFETY: the window outlives the menu, which the app drops first.
            unsafe {
                m._bar
                    .init_for_hwnd_with_theme(h.hwnd.get(), muda::MenuTheme::Dark)
            }
            .ok()?;
            m.show(cfg);
            Some(m)
        }

        /// The next menu choice, if one was made.
        pub fn poll(&self) -> Option<Action> {
            let id = MenuEvent::receiver().try_recv().ok()?.id;
            for (item, page) in [
                (&self.list, "games"),
                (&self.mods_page, "mods"),
                (&self.settings_page, "settings"),
            ] {
                if id == *item.id() {
                    return Some(Action::Screen(page));
                }
            }
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
            if let Some((_, p)) = self.mods.iter().find(|(i, _)| *i.id() == id) {
                return Some(Action::ToggleMod(p.clone()));
            }
            if let Some((_, f, k)) = self.mod_options.iter().find(|(i, _, _)| *i.id() == id) {
                return Some(Action::ToggleModOption(f.clone(), k.clone()));
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
                _ if id == *self.mods_folder.id() => Action::OpenModsFolder,
                _ => return None,
            })
        }

        /// Ticks the choices `cfg` makes.
        pub fn show(&self, cfg: &WindowConfig) {
            for (item, (_, r)) in self.refresh.iter().zip(REFRESH) {
                item.set_checked(cfg.refresh == *r);
            }
            self.wide.set_checked(cfg.widescreen);
            self.normal.set_checked(!cfg.widescreen);
            self.crisp.set_checked(!cfg.console_filter);
            self.console.set_checked(cfg.console_filter);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{flip_mod_option, mod_options, mod_options_file, toggle_mod_option, toggled};
    use std::path::Path;

    #[test]
    fn toggling_an_option_keeps_other_lines() {
        let file = std::env::temp_dir().join(format!("opts-{}.txt", std::process::id()));
        std::fs::write(&file, "# comment\nbeam magic = on\nother = off\n").unwrap();
        toggle_mod_option(&file, "beam magic").unwrap();
        let back = std::fs::read_to_string(&file).unwrap();
        std::fs::remove_file(&file).unwrap();
        assert_eq!(back, "# comment\nbeam magic = off\nother = off\n");
    }

    #[test]
    fn toggling_an_option_creates_a_missing_file() {
        let file = std::env::temp_dir().join(format!("opts-missing-{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&file);
        toggle_mod_option(&file, "beam magic").unwrap();
        let created = file.is_file();
        std::fs::remove_file(&file).unwrap();
        assert!(created);
    }

    #[test]
    fn toggling_an_option_leaves_an_unreadable_file_alone() {
        let file = std::env::temp_dir().join(format!("opts-bad-utf8-{}.txt", std::process::id()));
        let bytes = b"beam magic = on\n\xff\n";
        std::fs::write(&file, bytes).unwrap();
        let result = toggle_mod_option(&file, "beam magic");
        let kept = std::fs::read(&file).unwrap();
        std::fs::remove_file(&file).unwrap();
        assert!(result.is_err());
        assert_eq!(kept, bytes);

        let dir = std::env::temp_dir().join(format!("opts-dir-{}.txt", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let result = toggle_mod_option(&dir, "beam magic");
        let still_dir = dir.is_dir();
        std::fs::remove_dir(&dir).unwrap();
        assert!(result.is_err());
        assert!(still_dir);
    }

    #[test]
    fn mod_options_read_and_flip() {
        let text = "# comment\nbeam magic = on\nother = off\n";
        assert_eq!(
            mod_options(text),
            vec![("beam magic".into(), true), ("other".into(), false)]
        );
        let flipped = flip_mod_option(text, "beam magic");
        assert_eq!(flipped, "# comment\nbeam magic = off\nother = off\n");
        assert_eq!(
            mod_options_file(Path::new("mods/strong-form.dll.off")),
            Path::new("mods/strong-form.txt")
        );
    }

    #[test]
    fn toggling_a_mod_adds_or_drops_off() {
        assert_eq!(
            toggled(Path::new("mods/a.dll")),
            Path::new("mods/a.dll.off")
        );
        assert_eq!(
            toggled(Path::new("mods/a.dll.off")),
            Path::new("mods/a.dll")
        );
    }
}
