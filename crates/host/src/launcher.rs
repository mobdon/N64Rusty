//! The player's start screen: the ROMs played before as tiles in an 8-bit
//! style, drawn at 320x240 and scaled up in whole pixels. W/S/A/D or the
//! mouse move, Enter, Space or a click plays, Escape closes.

use std::num::NonZeroU32;
use std::path::PathBuf;
use std::rc::Rc;

use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, KeyEvent, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::{Theme, Window, WindowId};

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
const CARTS: [u32; 4] = FOUR;

/// Tiles shown: the ROMs, then "open another".
const ROWS: usize = 5;
const MAX_GAMES: usize = 2 * ROWS - 1;

/// Shows the start screen; returns the ROM picked, or `None` when closed.
pub fn pick(roms: &[PathBuf]) -> Option<PathBuf> {
    let roms: Vec<PathBuf> = roms.iter().take(MAX_GAMES).cloned().collect();
    let names = roms
        .iter()
        .map(|p| {
            p.file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    let mut app = Launcher {
        names,
        roms,
        selected: 0,
        window: None,
        surface: None,
        cursor: None,
        picked: None,
    };
    let event_loop = EventLoop::new().ok()?;
    event_loop.run_app(&mut app).ok()?;
    app.picked
}

struct Launcher {
    names: Vec<String>,
    roms: Vec<PathBuf>,
    selected: usize,
    window: Option<Rc<Window>>,
    surface: Option<softbuffer::Surface<Rc<Window>, Rc<Window>>>,
    cursor: Option<(f64, f64)>,
    picked: Option<PathBuf>,
}

impl Launcher {
    fn tiles(&self) -> usize {
        self.names.len() + 1
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

    fn choose(&mut self, event_loop: &ActiveEventLoop) {
        self.picked = match self.roms.get(self.selected) {
            Some(rom) => Some(rom.clone()),
            None => open_dialog(),
        };
        if self.picked.is_some() {
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
        let canvas = draw(&self.names, self.selected);
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

impl ApplicationHandler for Launcher {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title("N64Rusty")
            .with_theme(Some(Theme::Dark))
            .with_window_icon(icon())
            .with_inner_size(LogicalSize::new(W as f64 * 3.0, H as f64 * 3.0));
        let Ok(window) = event_loop.create_window(attrs) else {
            event_loop.exit();
            return;
        };
        let window = Rc::new(window);
        let surface = softbuffer::Context::new(window.clone())
            .and_then(|c| softbuffer::Surface::new(&c, window.clone()));
        match surface {
            Ok(s) => self.surface = Some(s),
            Err(e) => {
                eprintln!("error: the start screen cannot draw: {e}");
                event_loop.exit();
            }
        }
        self.window = Some(window);
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
        let before = self.selected;
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::RedrawRequested => self.redraw(),
            WindowEvent::Resized(_) => self.redraw(),
            WindowEvent::KeyboardInput {
                event:
                    KeyEvent {
                        physical_key: PhysicalKey::Code(key),
                        state: ElementState::Pressed,
                        ..
                    },
                ..
            } => match key {
                KeyCode::Escape => event_loop.exit(),
                KeyCode::Enter | KeyCode::NumpadEnter | KeyCode::Space => self.choose(event_loop),
                _ => self.selected = step(self.selected, self.tiles(), key),
            },
            WindowEvent::CursorMoved { position, .. } => {
                let (_, _, k, ox, oy) = self.fit();
                let x = (position.x as isize - ox as isize) / k as isize;
                let y = (position.y as isize - oy as isize) / k as isize;
                self.cursor = Some((x as f64, y as f64));
                if let Some(i) = tile_at(x, y).filter(|i| *i < self.tiles()) {
                    self.selected = i;
                }
            }
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Left,
                ..
            } => {
                if let Some((x, y)) = self.cursor {
                    if tile_at(x as isize, y as isize) == Some(self.selected) {
                        self.choose(event_loop);
                    }
                }
            }
            _ => {}
        }
        if self.selected != before {
            if let Some(w) = &self.window {
                w.request_redraw();
            }
        }
    }
}

/// Moves the selection in the two-column grid with W/S/A/D.
fn step(at: usize, tiles: usize, key: KeyCode) -> usize {
    let to = match key {
        KeyCode::KeyW => at.checked_sub(2),
        KeyCode::KeyS => Some(at + 2),
        KeyCode::KeyA => (at % 2 == 1).then(|| at - 1),
        KeyCode::KeyD => (at % 2 == 0).then_some(at + 1),
        _ => None,
    };
    to.filter(|t| *t < tiles).unwrap_or(at)
}

/// Tile `i` sits in column `i % 2`, row `i / 2`.
fn tile_rect(i: usize) -> (usize, usize, usize, usize) {
    (12 + (i % 2) * 152, 58 + (i / 2) * 32, 144, 28)
}

fn tile_at(x: isize, y: isize) -> Option<usize> {
    (0..2 * ROWS).find(|i| {
        let (tx, ty, tw, th) = tile_rect(*i);
        (tx as isize..(tx + tw) as isize).contains(&x)
            && (ty as isize..(ty + th) as isize).contains(&y)
    })
}

/// The start screen as 320x240 `0xRRGGBB` pixels.
pub fn draw(names: &[String], selected: usize) -> Vec<u32> {
    let mut c = vec![BG; W * H];
    // A starfield of single pixels, the same every time.
    for i in 0..60u32 {
        let h = i.wrapping_mul(2_654_435_761);
        let (x, y) = ((h % W as u32) as usize, ((h >> 12) % H as u32) as usize);
        c[y * W + x] = if i % 3 == 0 { EDGE } else { PANEL };
    }
    let title = "N64RUSTY";
    let tx = (W - title.len() * 16) / 2;
    // Each letter in the next of the four colors, over a black shadow.
    for (i, ch) in title.chars().enumerate() {
        let s = ch.to_string();
        text(&mut c, tx + i * 16 + 2, 14, &s, 0x000000, 2);
        text(&mut c, tx + i * 16, 12, &s, FOUR[i % 4], 2);
    }
    let sub = "SELECT A GAME";
    text(&mut c, (W - sub.len() * 8) / 2, 38, sub, SKY, 1);
    let names = &names[..names.len().min(MAX_GAMES)];
    let tiles = names.len() + 1;
    for i in 0..tiles {
        let (x, y, w, h) = tile_rect(i);
        let on = i == selected;
        // Chunky shadow, border and fill.
        rect(&mut c, x + 3, y + 3, w, h, 0x000000);
        rect(&mut c, x, y, w, h, if on { GOLD } else { EDGE });
        rect(
            &mut c,
            x + 2,
            y + 2,
            w - 4,
            h - 4,
            if on { 0x24345c } else { PANEL },
        );
        let (label, cart) = match names.get(i) {
            Some(n) => (n.to_uppercase(), CARTS[name_hash(n) % CARTS.len()]),
            None => ("OPEN ROM...".to_string(), EDGE),
        };
        // A little cartridge: body, label, contacts.
        rect(&mut c, x + 6, y + 6, 12, 16, cart);
        rect(&mut c, x + 8, y + 8, 8, 6, TEXT);
        rect(&mut c, x + 8, y + 18, 8, 2, 0x000000);
        let ink = if on { GOLD } else { TEXT };
        match lines(&label, 14) {
            (one, None) => text(&mut c, x + 22, y + 10, &one, ink, 1),
            (one, Some(two)) => {
                text(&mut c, x + 22, y + 5, &one, ink, 1);
                text(&mut c, x + 22, y + 15, &two, ink, 1);
            }
        }
    }
    let help = "WASD/MOUSE MOVE  ENTER PLAY";
    text(&mut c, (W - help.len() * 8) / 2, 226, help, EDGE, 1);
    c
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

fn name_hash(s: &str) -> usize {
    s.bytes()
        .fold(5381usize, |h, b| h.wrapping_mul(33) ^ b as usize)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wasd_moves_in_the_grid_and_stops_at_edges() {
        assert_eq!(step(0, 5, KeyCode::KeyD), 1);
        assert_eq!(step(1, 5, KeyCode::KeyD), 1);
        assert_eq!(step(1, 5, KeyCode::KeyS), 3);
        assert_eq!(step(3, 5, KeyCode::KeyS), 3);
        assert_eq!(step(4, 5, KeyCode::KeyW), 2);
        assert_eq!(step(0, 5, KeyCode::ArrowDown), 0);
    }

    #[test]
    fn long_names_wrap_at_a_space() {
        assert_eq!(lines("SHORT", 14), ("SHORT".into(), None));
        assert_eq!(
            lines("ALPHA BRAVO CHARLIE DELTA", 14),
            ("ALPHA BRAVO".into(), Some("CHARLIE DELTA".into()))
        );
        assert_eq!(
            lines("ABCDEFGHIJKLMNOPQRSTUVWXYZ0123", 14).1.unwrap().len(),
            14
        );
    }

    #[test]
    fn tiles_are_found_under_the_pointer() {
        let (x, y, ..) = tile_rect(3);
        assert_eq!(tile_at(x as isize + 1, y as isize + 1), Some(3));
        assert_eq!(tile_at(0, 0), None);
    }

    #[test]
    fn screen_draws_every_tile() {
        let names: Vec<String> = (0..20).map(|i| format!("game {i}")).collect();
        let c = draw(&names, 9);
        assert_eq!(c.len(), W * H);
        let (x, y, ..) = tile_rect(9);
        assert_eq!(c[y * W + x], GOLD);
        if let Ok(out) = std::env::var("N64RUSTY_LAUNCHER_PPM") {
            let mut ppm = format!("P6 {W} {H} 255\n").into_bytes();
            for p in &c {
                ppm.extend([(p >> 16) as u8, (p >> 8) as u8, *p as u8]);
            }
            std::fs::write(out, ppm).unwrap();
        }
    }
}
