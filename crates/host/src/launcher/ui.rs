//! The screens' state and navigation: pages, their items, the selection,
//! and what choosing an item does. No window and no pixels here.

use std::path::{Path, PathBuf};

use winit::keyboard::KeyCode;

use super::layout::{item_rect, tab_rect, Rect, TILE_COLS};
use super::palette::{GOLD, GREEN, RED};
use super::{dialogs, settings};
use crate::config::RunConfig;
use crate::menu::{
    mod_name, mod_options, mod_options_file, mods, toggle_mod_option, toggled, REFRESH, SCALES,
};

/// Game tiles: two columns of five, the last for "open another".
const ROWS: usize = 5;
const MAX_GAMES: usize = TILE_COLS * ROWS - 1;
/// List rows on the Mods and Settings pages.
const LIST_ROWS: usize = 8;

/// A page of the start screen, chosen by its tab.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Page {
    /// ROMs played before, as tiles.
    Games,
    /// Mods and their options, on or off.
    Mods,
    /// The window choices.
    Settings,
}

/// The pages in tab order, with their tab names.
pub(super) const PAGES: [(Page, &str); 3] = [
    (Page::Games, "GAMES"),
    (Page::Mods, "MODS"),
    (Page::Settings, "SETTINGS"),
];

impl Page {
    /// `games`, `mods` or `settings`, as `--screen` takes it.
    pub fn parse(s: &str) -> Option<Page> {
        PAGES
            .iter()
            .find(|(_, n)| n.eq_ignore_ascii_case(s))
            .map(|(p, _)| *p)
    }
}

/// What choosing an item does.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum Act {
    Play(PathBuf),
    Open,
    Mod(PathBuf),
    ModOption(PathBuf, String),
    ModsFolder,
    Wide,
    Refresh,
    Textures,
    Size,
    Nothing,
}

/// One tile or row: its label, a value shown on the right with its color,
/// and what choosing it does.
#[derive(Clone, Debug)]
pub(super) struct Item {
    pub(super) label: String,
    pub(super) value: Option<(String, u32)>,
    pub(super) act: Act,
}

/// What is under the pointer.
pub(super) enum Hit {
    Tab(Page),
    Item(usize),
}

/// The screens' state, apart from the window.
pub struct Ui {
    pub(super) page: Page,
    pub(super) sel: usize,
    pub(super) games: Vec<PathBuf>,
    pub(super) settings: RunConfig,
    /// A message shown over everything: title and text.
    pub(super) message: Option<(String, String)>,
}

impl Ui {
    /// The current page's items, in order.
    pub(super) fn items(&self) -> Vec<Item> {
        match self.page {
            Page::Games => self.game_items(),
            Page::Mods => mod_items(mods()),
            Page::Settings => self.setting_items(),
        }
    }

    fn game_items(&self) -> Vec<Item> {
        let mut v: Vec<Item> = self
            .games
            .iter()
            .take(MAX_GAMES)
            .map(|p| Item {
                label: stem(p),
                value: None,
                act: Act::Play(p.clone()),
            })
            .collect();
        v.push(Item {
            label: "OPEN ROM...".into(),
            value: None,
            act: Act::Open,
        });
        v
    }

    fn setting_items(&self) -> Vec<Item> {
        let w = &self.settings.window;
        let refresh = REFRESH
            .iter()
            .find(|(_, r)| *r == w.refresh)
            .map_or("MATCH DISPLAY", |(l, _)| l);
        let setting = |label: &str, value: String, act| Item {
            label: label.into(),
            value: Some((value, GOLD)),
            act,
        };
        let picture = if w.widescreen {
            "WIDE 16:9"
        } else {
            "NORMAL 4:3"
        };
        let textures = if w.console_filter { "CONSOLE" } else { "CRISP" };
        vec![
            setting("PICTURE", picture.into(), Act::Wide),
            setting("REFRESH", refresh.to_uppercase(), Act::Refresh),
            setting("TEXTURES", textures.into(), Act::Textures),
            setting("WINDOW SIZE", format!("{}X", w.scale), Act::Size),
        ]
    }

    /// Goes to `page`, selecting its first item.
    pub(super) fn switch(&mut self, page: Page) {
        if self.page != page {
            self.page = page;
            self.sel = 0;
        }
    }

    /// Does the item's action; returns a ROM to play.
    pub(super) fn choose(&mut self) -> Option<PathBuf> {
        let item = self.items().into_iter().nth(self.sel)?;
        let w = &mut self.settings.window;
        match item.act {
            Act::Play(rom) => return Some(rom),
            Act::Open => return dialogs::open_rom(),
            Act::Mod(path) => {
                if let Err(e) = std::fs::rename(&path, toggled(&path)) {
                    self.message = Some(("MOD NOT CHANGED".into(), e.to_string()));
                }
                return None;
            }
            Act::ModOption(file, name) => {
                if let Err(e) = toggle_mod_option(&file, &name) {
                    self.message = Some(("OPTION NOT CHANGED".into(), e.to_string()));
                }
                return None;
            }
            Act::ModsFolder => {
                dialogs::open_folder(&crate::menu::mods_dir()?);
                return None;
            }
            Act::Nothing => return None,
            Act::Wide => w.widescreen = !w.widescreen,
            Act::Textures => w.console_filter = !w.console_filter,
            Act::Size => {
                let (smallest, largest) = (SCALES[0], SCALES[SCALES.len() - 1]);
                w.scale = if w.scale >= largest {
                    smallest
                } else {
                    w.scale + 1
                };
            }
            Act::Refresh => {
                let at = REFRESH.iter().position(|(_, r)| *r == w.refresh);
                w.refresh = REFRESH[at.map_or(0, |i| (i + 1) % REFRESH.len())].1;
            }
        }
        if let Err(e) = settings::save_everywhere(&self.settings) {
            self.message = Some(("SETTINGS NOT SAVED".into(), e));
        }
        None
    }

    /// Moves the selection with W/S/A/D; Q/E change page.
    pub(super) fn key(&mut self, key: KeyCode) {
        let at = PAGES.iter().position(|(p, _)| *p == self.page).unwrap_or(0);
        match key {
            KeyCode::KeyQ => self.switch(PAGES[(at + PAGES.len() - 1) % PAGES.len()].0),
            KeyCode::KeyE | KeyCode::Tab => self.switch(PAGES[(at + 1) % PAGES.len()].0),
            _ => {
                let n = self.items().len();
                self.sel = match self.page {
                    Page::Games => step(self.sel, n, key, TILE_COLS),
                    _ => step(self.sel, n, key, 1),
                };
            }
        }
    }

    /// The tab or item under a canvas point.
    pub(super) fn hit(&self, x: isize, y: isize) -> Option<Hit> {
        let inside = |(rx, ry, rw, rh): Rect| {
            (rx as isize..(rx + rw) as isize).contains(&x)
                && (ry as isize..(ry + rh) as isize).contains(&y)
        };
        if let Some(i) = (0..PAGES.len()).find(|i| inside(tab_rect(*i))) {
            return Some(Hit::Tab(PAGES[i].0));
        }
        (0..self.items().len())
            .find(|i| inside(item_rect(self.page, *i)))
            .map(Hit::Item)
    }
}

/// The Mods page for the mod files `found` (as [`mods`] lists them): each
/// mod, then its options, then "open mods folder" last.
fn mod_items(found: Vec<(PathBuf, bool)>) -> Vec<Item> {
    let mut v = Vec::new();
    for (path, on) in found {
        v.push(Item {
            // The same name the menu bar shows, for every library extension.
            label: mod_name(&path),
            value: Some(on_off(on)),
            act: Act::Mod(path.clone()),
        });
        let file = mod_options_file(&path);
        let text = std::fs::read_to_string(&file).unwrap_or_default();
        for (name, on) in mod_options(&text) {
            v.push(Item {
                label: format!("  {name}"),
                value: Some(on_off(on)),
                act: Act::ModOption(file.clone(), name),
            });
        }
    }
    if v.is_empty() {
        v.push(Item {
            label: "NO MODS YET".into(),
            value: None,
            act: Act::Nothing,
        });
    }
    v.truncate(LIST_ROWS - 1);
    v.push(Item {
        label: "OPEN MODS FOLDER".into(),
        value: None,
        act: Act::ModsFolder,
    });
    v
}

/// Moves the selection in a grid `cols` wide with W/S/A/D.
fn step(at: usize, items: usize, key: KeyCode, cols: usize) -> usize {
    let to = match key {
        KeyCode::KeyW => at.checked_sub(cols),
        KeyCode::KeyS => Some(at + cols),
        KeyCode::KeyA if cols > 1 => (at % cols > 0).then(|| at - 1),
        KeyCode::KeyD if cols > 1 => (at % cols < cols - 1).then_some(at + 1),
        _ => None,
    };
    to.filter(|t| *t < items).unwrap_or(at)
}

fn stem(p: &Path) -> String {
    p.file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned()
}

fn on_off(on: bool) -> (String, u32) {
    if on {
        ("ON".into(), GREEN)
    } else {
        ("OFF".into(), RED)
    }
}

/// A screen on `page` with `games` made-up ROMs, saving nowhere.
#[cfg(test)]
pub(super) fn test_ui(page: Page, games: usize) -> Ui {
    Ui {
        page,
        sel: 0,
        games: (0..games)
            .map(|i| PathBuf::from(format!("game {i}.z64")))
            .collect(),
        settings: RunConfig::default(),
        message: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wasd_moves_in_the_grid_and_stops_at_edges() {
        assert_eq!(step(0, 5, KeyCode::KeyD, 2), 1);
        assert_eq!(step(1, 5, KeyCode::KeyD, 2), 1);
        assert_eq!(step(1, 5, KeyCode::KeyS, 2), 3);
        assert_eq!(step(3, 5, KeyCode::KeyS, 2), 3);
        assert_eq!(step(4, 5, KeyCode::KeyW, 2), 2);
        assert_eq!(step(0, 5, KeyCode::ArrowDown, 2), 0);
        assert_eq!(step(0, 5, KeyCode::KeyS, 1), 1);
        assert_eq!(step(0, 5, KeyCode::KeyD, 1), 0);
    }

    #[test]
    fn q_and_e_change_page() {
        let mut u = test_ui(Page::Games, 3);
        u.sel = 2;
        u.key(KeyCode::KeyE);
        assert_eq!((u.page, u.sel), (Page::Mods, 0));
        u.key(KeyCode::KeyQ);
        u.key(KeyCode::KeyQ);
        assert_eq!(u.page, Page::Settings);
        assert_eq!(Page::parse("mods"), Some(Page::Mods));
    }

    #[test]
    fn settings_cycle() {
        let mut u = test_ui(Page::Settings, 0);
        u.settings.path = None; // nothing to save
        let wide = u.settings.window.widescreen;
        u.choose();
        assert_eq!(u.settings.window.widescreen, !wide);
        u.sel = 1;
        for (_, r) in REFRESH.iter().cycle().skip(1).take(REFRESH.len()) {
            u.choose();
            assert_eq!(u.settings.window.refresh, *r);
        }
        u.sel = 3;
        u.settings.window.scale = 4;
        u.choose();
        assert_eq!(u.settings.window.scale, 2);
    }

    #[test]
    fn mods_show_their_name_without_any_library_extension() {
        let found = vec![
            (PathBuf::from("mods/a.dll.off"), false),
            (PathBuf::from("mods/b.so.off"), false),
            (PathBuf::from("mods/c.dylib.off"), false),
            (PathBuf::from("mods/d.so"), true),
        ];
        let labels: Vec<String> = mod_items(found).into_iter().map(|i| i.label).collect();
        assert_eq!(labels, ["a", "b", "c", "d", "OPEN MODS FOLDER"]);
    }

    #[test]
    fn no_mods_still_offer_the_folder() {
        let acts: Vec<Act> = mod_items(Vec::new()).into_iter().map(|i| i.act).collect();
        assert_eq!(acts, [Act::Nothing, Act::ModsFolder]);
    }

    #[test]
    fn tiles_and_tabs_are_found_under_the_pointer() {
        let u = test_ui(Page::Games, 5);
        let (x, y, ..) = item_rect(Page::Games, 3);
        assert!(matches!(
            u.hit(x as isize + 1, y as isize + 1),
            Some(Hit::Item(3))
        ));
        let (x, y, ..) = tab_rect(2);
        assert!(matches!(
            u.hit(x as isize + 1, y as isize + 1),
            Some(Hit::Tab(Page::Settings))
        ));
        assert!(u.hit(0, 0).is_none());
    }
}
