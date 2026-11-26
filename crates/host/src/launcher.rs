//! The player's own screens in an 8-bit style, drawn at 320x240 and scaled
//! up in whole pixels: Games (the ROMs played before as tiles), Mods (turn
//! mods and their options on or off), Settings (the window choices) and a
//! message screen for errors. W/S/A/D or the mouse move, Q/E or a click
//! changes page, Enter, Space or a click chooses, Escape closes.

use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, KeyEvent, MouseButton, WindowEvent};
use winit::event_loop::ActiveEventLoop;
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::{Theme, Window, WindowId};

use crate::config::RunConfig;
use crate::menu::{flip_mod_option, mod_options, mod_options_file, mods, toggled, REFRESH};

pub const W: usize = 320;
pub const H: usize = 240;

/// The palette: the console's four bright colors on its dark grey.
pub const BG: u32 = 0x1e1e22;
pub const PANEL: u32 = 0x3a3a40;
pub const EDGE: u32 = 0x6a6a72;
pub const TEXT: u32 = 0xf0f0f0;
pub const GOLD: u32 = 0xffc20e;
pub const RED: u32 = 0xe4002b;
pub const GREEN: u32 = 0x00a651;
pub const SKY: u32 = 0x0a72d8;
const FOUR: [u32; 4] = [RED, GOLD, GREEN, SKY];
const PICKED: u32 = 0x24345c;

/// Game tiles: two columns of five, the last for "open another".
const ROWS: usize = 5;
const MAX_GAMES: usize = 2 * ROWS - 1;
/// List rows on the Mods and Settings pages.
const LIST_ROWS: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Page {
    Games,
    Mods,
    Settings,
}

const PAGES: [(Page, &str); 3] = [
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
enum Act {
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

#[derive(Clone, Debug)]
struct Item {
    label: String,
    value: Option<(String, u32)>,
    act: Act,
}

/// The screens' state, apart from the window.
pub struct Ui {
    page: Page,
    sel: usize,
    games: Vec<PathBuf>,
    settings: RunConfig,
    /// A message shown over everything: title and text.
    message: Option<(String, String)>,
}

impl Ui {
    fn items(&self) -> Vec<Item> {
        match self.page {
            Page::Games => {
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
            Page::Mods => {
                let mut v = Vec::new();
                for (path, on) in mods() {
                    v.push(Item {
                        label: stem(&path).trim_end_matches(".dll").to_string(),
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
            Page::Settings => {
                let w = &self.settings.window;
                let refresh = REFRESH
                    .iter()
                    .find(|(_, r)| *r == w.refresh)
                    .map_or("MATCH DISPLAY", |(l, _)| l);
                vec![
                    Item {
                        label: "PICTURE".into(),
                        value: Some((
                            if w.widescreen {
                                "WIDE 16:9"
                            } else {
                                "NORMAL 4:3"
                            }
                            .into(),
                            GOLD,
                        )),
                        act: Act::Wide,
                    },
                    Item {
                        label: "REFRESH".into(),
                        value: Some((refresh.to_uppercase(), GOLD)),
                        act: Act::Refresh,
                    },
                    Item {
                        label: "TEXTURES".into(),
                        value: Some((
                            if w.console_filter { "CONSOLE" } else { "CRISP" }.into(),
                            GOLD,
                        )),
                        act: Act::Textures,
                    },
                    Item {
                        label: "WINDOW SIZE".into(),
                        value: Some((format!("{}X", w.scale), GOLD)),
                        act: Act::Size,
                    },
                ]
            }
        }
    }

    fn switch(&mut self, page: Page) {
        if self.page != page {
            self.page = page;
            self.sel = 0;
        }
    }

    /// Does the item's action; returns a ROM to play.
    fn choose(&mut self) -> Option<PathBuf> {
        let item = self.items().into_iter().nth(self.sel)?;
        let w = &mut self.settings.window;
        match item.act {
            Act::Play(rom) => return Some(rom),
            Act::Open => return open_dialog(),
            Act::Mod(path) => {
                if let Err(e) = std::fs::rename(&path, toggled(&path)) {
                    self.message = Some(("MOD NOT CHANGED".into(), e.to_string()));
                }
                return None;
            }
            Act::ModOption(file, name) => {
                let text = std::fs::read_to_string(&file).unwrap_or_default();
                if let Err(e) = std::fs::write(&file, flip_mod_option(&text, &name)) {
                    self.message = Some(("OPTION NOT CHANGED".into(), e.to_string()));
                }
                return None;
            }
            Act::ModsFolder => {
                open_folder(&crate::menu::mods_dir()?);
                return None;
            }
            Act::Nothing => return None,
            Act::Wide => w.widescreen = !w.widescreen,
            Act::Textures => w.console_filter = !w.console_filter,
            Act::Size => w.scale = if w.scale >= 4 { 2 } else { w.scale + 1 },
            Act::Refresh => {
                let at = REFRESH.iter().position(|(_, r)| *r == w.refresh);
                w.refresh = REFRESH[at.map_or(0, |i| (i + 1) % REFRESH.len())].1;
            }
        }
        if let Err(e) = save_everywhere(&self.settings) {
            self.message = Some(("SETTINGS NOT SAVED".into(), e));
        }
        None
    }

    /// Moves the selection with W/S/A/D; Q/E change page.
    fn key(&mut self, key: KeyCode) {
        let at = PAGES.iter().position(|(p, _)| *p == self.page).unwrap_or(0);
        match key {
            KeyCode::KeyQ => self.switch(PAGES[(at + PAGES.len() - 1) % PAGES.len()].0),
            KeyCode::KeyE | KeyCode::Tab => self.switch(PAGES[(at + 1) % PAGES.len()].0),
            _ => {
                let n = self.items().len();
                self.sel = match self.page {
                    Page::Games => step(self.sel, n, key, 2),
                    _ => step(self.sel, n, key, 1),
                };
            }
        }
    }

    /// The tab or item under a canvas point.
    fn hit(&self, x: isize, y: isize) -> Option<Hit> {
        let inside = |(rx, ry, rw, rh): (usize, usize, usize, usize)| {
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

    /// The screen as 320x240 `0xRRGGBB` pixels.
    pub fn draw(&self) -> Vec<u32> {
        let mut c = vec![BG; W * H];
        stars(&mut c);
        let title = "N64RUSTY";
        let tx = (W - title.len() * 16) / 2;
        for (i, ch) in title.chars().enumerate() {
            let s = ch.to_string();
            text(&mut c, tx + i * 16 + 2, 8, &s, 0x000000, 2);
            text(&mut c, tx + i * 16, 6, &s, FOUR[i % 4], 2);
        }
        if let Some((head, body)) = &self.message {
            panel(&mut c, 20, 50, 280, 160, RED);
            text(&mut c, (W - head.len() * 8) / 2, 62, head, GOLD, 1);
            for (n, line) in wrap(&body.to_uppercase(), 32).iter().take(10).enumerate() {
                text(&mut c, 32, 82 + n * 10, line, TEXT, 1);
            }
            let ok = "ENTER = OK";
            text(&mut c, (W - ok.len() * 8) / 2, 196, ok, EDGE, 1);
            return c;
        }
        for (i, (page, name)) in PAGES.iter().enumerate() {
            let (x, y, w, h) = tab_rect(i);
            let on = *page == self.page;
            rect(&mut c, x + 2, y + 2, w, h, 0x000000);
            rect(&mut c, x, y, w, h, if on { FOUR[i + 1] } else { EDGE });
            rect(
                &mut c,
                x + 2,
                y + 2,
                w - 4,
                h - 4,
                if on { PICKED } else { PANEL },
            );
            let ink = if on { GOLD } else { TEXT };
            text(&mut c, x + (w - name.len() * 8) / 2, y + 4, name, ink, 1);
        }
        for (i, item) in self.items().iter().enumerate() {
            let (x, y, w, h) = item_rect(self.page, i);
            let on = i == self.sel;
            panel(&mut c, x, y, w, h, if on { GOLD } else { EDGE });
            if on {
                rect(&mut c, x + 2, y + 2, w - 4, h - 4, PICKED);
            }
            let ink = if on { GOLD } else { TEXT };
            let label = item.label.to_uppercase();
            if self.page == Page::Games {
                let cart = match item.act {
                    Act::Play(_) => FOUR[name_hash(&item.label) % 4],
                    _ => EDGE,
                };
                rect(&mut c, x + 6, y + 6, 12, 16, cart);
                rect(&mut c, x + 8, y + 8, 8, 6, TEXT);
                rect(&mut c, x + 8, y + 18, 8, 2, 0x000000);
                match lines(&label, 14) {
                    (one, None) => text(&mut c, x + 22, y + 10, &one, ink, 1),
                    (one, Some(two)) => {
                        text(&mut c, x + 22, y + 5, &one, ink, 1);
                        text(&mut c, x + 22, y + 15, &two, ink, 1);
                    }
                }
            } else {
                let room = match &item.value {
                    Some((v, _)) => 34 - v.len().min(14),
                    None => 34,
                };
                let shown: String = label.chars().take(room).collect();
                text(&mut c, x + 8, y + 6, &shown, ink, 1);
                if let Some((v, color)) = &item.value {
                    let v: String = v.chars().take(14).collect();
                    text(&mut c, x + w - 8 - v.len() * 8, y + 6, &v, *color, 1);
                }
            }
        }
        let help = match self.page {
            Page::Games => "WASD MOVE  Q/E PAGE  ENTER PLAY",
            _ => "WASD MOVE  Q/E PAGE  ENTER CHANGE",
        };
        text(&mut c, (W - help.len() * 8) / 2, 228, help, EDGE, 1);
        c
    }
}

enum Hit {
    Tab(Page),
    Item(usize),
}

/// Shows the screens on `page`; returns the ROM picked, or `None` when
/// closed (or when no window could open).
pub fn pick(roms: &[PathBuf], page: Page) -> Option<PathBuf> {
    let ui = Ui {
        page,
        sel: 0,
        games: roms.to_vec(),
        settings: load_settings(),
        message: None,
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
    app.surface = None;
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
    app.surface = None;
    if let Some(w) = app.window.take() {
        crate::one_window::keep(w);
    }
    Some(picked)
}

struct Screens {
    ui: Ui,
    /// Closing the message closes the program (an error screen).
    message_only: bool,
    window: Option<Arc<Window>>,
    surface: Option<softbuffer::Surface<Arc<Window>, Arc<Window>>>,
    cursor: Option<(isize, isize)>,
    picked: Option<PathBuf>,
    /// A window opened and could be drawn in.
    shown: bool,
    /// The player closed the window.
    closed: bool,
}

impl Screens {
    fn new(ui: Ui, message_only: bool) -> Screens {
        Screens {
            ui,
            message_only,
            window: None,
            surface: None,
            cursor: None,
            picked: None,
            shown: false,
            closed: false,
        }
    }

    /// Canvas scale and offset for the window's size.
    fn fit(&self) -> (usize, usize, usize, usize, usize) {
        let size = self
            .window
            .as_ref()
            .map(|w| w.inner_size())
            .unwrap_or_default();
        let (ww, wh) = (size.width.max(1) as usize, size.height.max(1) as usize);
        let k = (ww / W).min(wh / H).max(1);
        (
            ww,
            wh,
            k,
            ww.saturating_sub(W * k) / 2,
            wh.saturating_sub(H * k) / 2,
        )
    }

    fn activate(&mut self, event_loop: &ActiveEventLoop) {
        if self.ui.message.take().is_some() {
            if self.message_only {
                event_loop.exit();
            }
            return;
        }
        if let Some(rom) = self.ui.choose() {
            self.picked = Some(rom);
            event_loop.exit();
        }
    }

    fn redraw(&mut self) {
        let (ww, wh, k, ox, oy) = self.fit();
        let Some(surface) = self.surface.as_mut() else {
            return;
        };
        let (Some(w), Some(h)) = (NonZeroU32::new(ww as u32), NonZeroU32::new(wh as u32)) else {
            return;
        };
        if surface.resize(w, h).is_err() {
            return;
        }
        let Ok(mut buf) = surface.buffer_mut() else {
            return;
        };
        let canvas = self.ui.draw();
        buf.fill(BG);
        for y in 0..(H * k).min(wh) {
            let row = &canvas[(y / k) * W..(y / k + 1) * W];
            let out = &mut buf[(oy + y) * ww..(oy + y + 1) * ww];
            for x in 0..(W * k).min(ww) {
                out[ox + x] = row[x / k];
            }
        }
        let _ = buf.present();
    }
}

impl ApplicationHandler for Screens {
    // Each run of the shared event loop starts here; `resumed` may not come
    // again on a later run.
    fn new_events(&mut self, event_loop: &ActiveEventLoop, cause: winit::event::StartCause) {
        if cause == winit::event::StartCause::Init {
            self.resumed(event_loop);
        }
    }

    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let window = match crate::one_window::take() {
            Some(w) => w,
            None => {
                let attrs = Window::default_attributes()
                    .with_title("N64Rusty")
                    .with_theme(Some(Theme::Dark))
                    .with_window_icon(icon())
                    .with_inner_size(LogicalSize::new(W as f64 * 3.0, H as f64 * 3.0));
                match event_loop.create_window(attrs) {
                    Ok(w) => Arc::new(w),
                    Err(_) => {
                        event_loop.exit();
                        return;
                    }
                }
            }
        };
        window.set_title("N64Rusty");
        let surface = softbuffer::Context::new(window.clone())
            .and_then(|c| softbuffer::Surface::new(&c, window.clone()));
        match surface {
            Ok(s) => {
                self.surface = Some(s);
                self.shown = true;
            }
            Err(e) => {
                eprintln!("error: the start screen cannot draw: {e}");
                event_loop.exit();
            }
        }
        window.request_redraw();
        self.window = Some(window);
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
        let mut changed = true;
        match event {
            WindowEvent::CloseRequested => {
                self.closed = true;
                event_loop.exit();
            }
            WindowEvent::RedrawRequested | WindowEvent::Resized(_) => {
                self.redraw();
                changed = false;
            }
            WindowEvent::KeyboardInput {
                event:
                    KeyEvent {
                        physical_key: PhysicalKey::Code(key),
                        state: ElementState::Pressed,
                        ..
                    },
                ..
            } => match key {
                KeyCode::Escape if self.ui.message.is_some() => self.activate(event_loop),
                KeyCode::Escape => event_loop.exit(),
                KeyCode::Enter | KeyCode::NumpadEnter | KeyCode::Space => self.activate(event_loop),
                _ if self.ui.message.is_none() => self.ui.key(key),
                _ => changed = false,
            },
            WindowEvent::CursorMoved { position, .. } => {
                let (_, _, k, ox, oy) = self.fit();
                let x = (position.x as isize - ox as isize) / k as isize;
                let y = (position.y as isize - oy as isize) / k as isize;
                self.cursor = Some((x, y));
                match self.ui.hit(x, y) {
                    Some(Hit::Item(i)) if self.ui.message.is_none() && i != self.ui.sel => {
                        self.ui.sel = i
                    }
                    _ => changed = false,
                }
            }
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Left,
                ..
            } => match self.cursor.and_then(|(x, y)| self.ui.hit(x, y)) {
                _ if self.ui.message.is_some() => self.activate(event_loop),
                Some(Hit::Tab(page)) => self.ui.switch(page),
                Some(Hit::Item(i)) if i == self.ui.sel => self.activate(event_loop),
                _ => changed = false,
            },
            _ => changed = false,
        }
        if changed {
            if let Some(w) = &self.window {
                w.request_redraw();
            }
        }
    }
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

fn tab_rect(i: usize) -> (usize, usize, usize, usize) {
    (12 + i * 100, 30, 92, 16)
}

/// Item `i` on a page: a tile in two columns, or a list row.
fn item_rect(page: Page, i: usize) -> (usize, usize, usize, usize) {
    match page {
        Page::Games => (12 + (i % 2) * 152, 56 + (i / 2) * 33, 144, 28),
        _ => (12, 56 + i * 21, 296, 19),
    }
}

/// Settings next to the program, shared by every game.
fn settings_path() -> Option<PathBuf> {
    Some(
        std::env::current_exe()
            .ok()?
            .parent()?
            .join("settings.toml"),
    )
}

fn load_settings() -> RunConfig {
    let Some(path) = settings_path() else {
        return RunConfig::default();
    };
    let mut cfg = RunConfig::load(&path).unwrap_or_default();
    cfg.path = Some(path);
    cfg
}

/// Saves the window choices to `settings.toml` and to each game's own
/// `run.toml` (whose other keys, such as HUD areas, stay).
fn save_everywhere(cfg: &RunConfig) -> Result<(), String> {
    cfg.save_window()?;
    let Some(games) = cfg
        .path
        .as_deref()
        .and_then(Path::parent)
        .map(|d| d.join("games"))
    else {
        return Ok(());
    };
    for game in std::fs::read_dir(games).into_iter().flatten().flatten() {
        let path = game.path().join("run.toml");
        if let Ok(mut own) = RunConfig::load(&path) {
            own.window.widescreen = cfg.window.widescreen;
            own.window.console_filter = cfg.window.console_filter;
            own.window.scale = cfg.window.scale;
            own.window.refresh = cfg.window.refresh;
            own.save_window()?;
        }
    }
    Ok(())
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

/// Splits a label into up to two lines of `n` characters, at a space
/// when one fits.
fn lines(s: &str, n: usize) -> (String, Option<String>) {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= n {
        return (s.to_string(), None);
    }
    let cut = chars[..=n]
        .iter()
        .rposition(|c| *c == ' ')
        .filter(|i| *i > 0)
        .unwrap_or(n);
    let one: String = chars[..cut].iter().collect();
    let two: String = chars[cut..]
        .iter()
        .collect::<String>()
        .trim_start()
        .chars()
        .take(n)
        .collect();
    (one, Some(two))
}

/// Wraps text into lines of at most `n` characters, at spaces where it can.
fn wrap(s: &str, n: usize) -> Vec<String> {
    let mut out = Vec::new();
    for para in s.lines() {
        let mut rest = para.trim().to_string();
        while !rest.is_empty() {
            let (one, two) = lines(&rest, n);
            out.push(one);
            rest = two.map_or(String::new(), |_| {
                rest.chars()
                    .skip(out.last().map_or(0, |l| l.chars().count()))
                    .collect::<String>()
                    .trim_start()
                    .to_string()
            });
        }
    }
    out
}

fn name_hash(s: &str) -> usize {
    s.bytes()
        .fold(5381usize, |h, b| h.wrapping_mul(33) ^ b as usize)
}

/// A few dim single pixels, the same every time.
fn stars(c: &mut [u32]) {
    for i in 0..60u32 {
        let h = i.wrapping_mul(2_654_435_761);
        let (x, y) = ((h % W as u32) as usize, ((h >> 12) % H as u32) as usize);
        c[y * W + x] = if i % 3 == 0 { EDGE } else { PANEL };
    }
}

/// A chunky box: shadow, `edge`-colored border, panel fill.
fn panel(c: &mut [u32], x: usize, y: usize, w: usize, h: usize, edge: u32) {
    rect(c, x + 3, y + 3, w, h, 0x000000);
    rect(c, x, y, w, h, edge);
    rect(c, x + 2, y + 2, w - 4, h - 4, PANEL);
}

fn rect(c: &mut [u32], x: usize, y: usize, w: usize, h: usize, color: u32) {
    for yy in y..(y + h).min(H) {
        for xx in x..(x + w).min(W) {
            c[yy * W + xx] = color;
        }
    }
}

/// Draws ASCII text with the 8x8 pixel font, each font pixel `k` wide.
fn text(c: &mut [u32], x: usize, y: usize, s: &str, color: u32, k: usize) {
    for (n, ch) in s.chars().enumerate() {
        let glyph = font8x8::legacy::BASIC_LEGACY
            .get(ch as usize)
            .copied()
            .unwrap_or(font8x8::legacy::BASIC_LEGACY[b'?' as usize]);
        for (row, bits) in glyph.iter().enumerate() {
            for col in 0..8 {
                if bits >> col & 1 == 1 {
                    rect(c, x + (n * 8 + col) * k, y + row * k, k, k, color);
                }
            }
        }
    }
}

/// The window icon: a block in the four colors with a white "R".
pub fn icon() -> Option<winit::window::Icon> {
    const S: usize = 32;
    let mut px = vec![0u8; S * S * 4];
    let glyph = font8x8::legacy::BASIC_LEGACY[b'R' as usize];
    for y in 0..S {
        for x in 0..S {
            let (gx, gy) = (x.wrapping_sub(4) / 3, y.wrapping_sub(4) / 3);
            let ink = gx < 8 && gy < 8 && glyph[gy] >> gx & 1 == 1;
            let quarter = FOUR[(y / 16) * 2 + x / 16];
            let color = if ink { TEXT } else { quarter };
            px[(y * S + x) * 4..][..4].copy_from_slice(&[
                (color >> 16) as u8,
                (color >> 8) as u8,
                color as u8,
                255,
            ]);
        }
    }
    winit::window::Icon::from_rgba(px, S as u32, S as u32).ok()
}

#[cfg(windows)]
fn open_dialog() -> Option<PathBuf> {
    rfd::FileDialog::new()
        .set_title("Pick your N64 ROM")
        .add_filter("N64 ROM", &["z64", "n64", "v64"])
        .pick_file()
}

#[cfg(not(windows))]
fn open_dialog() -> Option<PathBuf> {
    None
}

fn open_folder(dir: &Path) {
    let _ = std::fs::create_dir_all(dir);
    #[cfg(windows)]
    let _ = std::process::Command::new("explorer").arg(dir).spawn();
    #[cfg(not(windows))]
    let _ = dir;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ui(page: Page, games: usize) -> Ui {
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
        let mut u = ui(Page::Games, 3);
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
        let mut u = ui(Page::Settings, 0);
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
    fn long_names_wrap_at_a_space() {
        assert_eq!(lines("SHORT", 14), ("SHORT".into(), None));
        assert_eq!(
            lines("ALPHA BRAVO CHARLIE DELTA", 14),
            ("ALPHA BRAVO".into(), Some("CHARLIE DELTA".into()))
        );
        assert_eq!(
            wrap("ONE TWO THREE FOUR FIVE", 9),
            vec!["ONE TWO", "THREE", "FOUR FIVE"]
        );
    }

    #[test]
    fn tiles_and_tabs_are_found_under_the_pointer() {
        let u = ui(Page::Games, 5);
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

    #[test]
    fn every_screen_draws() {
        let mut shots = Vec::new();
        let mut games = ui(Page::Games, 20);
        games.sel = 3;
        shots.push(("games", games.draw()));
        shots.push(("settings", ui(Page::Settings, 0).draw()));
        shots.push(("mods", ui(Page::Mods, 0).draw()));
        let mut err = ui(Page::Games, 0);
        err.message = Some((
            "COULD NOT START".into(),
            "error: no system routines found in this ROM, so it cannot run yet".into(),
        ));
        shots.push(("error", err.draw()));
        for (name, c) in shots {
            assert_eq!(c.len(), W * H);
            if let Ok(dir) = std::env::var("N64RUSTY_LAUNCHER_PPM") {
                let mut ppm = format!("P6 {W} {H} 255\n").into_bytes();
                for p in &c {
                    ppm.extend([(p >> 16) as u8, (p >> 8) as u8, *p as u8]);
                }
                std::fs::write(Path::new(&dir).join(format!("{name}.ppm")), ppm).unwrap();
            }
        }
    }
}
