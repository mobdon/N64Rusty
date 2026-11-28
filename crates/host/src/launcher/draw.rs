//! Paints a [`Ui`] onto the canvas: title, tabs, the page's items, the
//! help line, or the message panel over everything.

use super::canvas::{inner, lines, name_hash, panel, rect, stars, text, wrap};
use super::layout::*;
use super::palette::{BG, EDGE, FOUR, GOLD, PANEL, PICKED, RED, SHADOW, TEXT};
use super::ui::{Act, Item, Page, Ui, PAGES};

impl Ui {
    /// The screen as 320x240 `0xRRGGBB` pixels.
    pub fn draw(&self) -> Vec<u32> {
        let mut c = vec![BG; W * H];
        stars(&mut c);
        draw_title(&mut c);
        if let Some((head, body)) = &self.message {
            draw_message(&mut c, head, body);
            return c;
        }
        self.draw_tabs(&mut c);
        for (i, item) in self.items().iter().enumerate() {
            self.draw_item(&mut c, i, item);
        }
        let help = match self.page {
            Page::Games => "WASD MOVE  Q/E PAGE  ENTER PLAY",
            _ => "WASD MOVE  Q/E PAGE  ENTER CHANGE",
        };
        text(&mut c, centred(help.len(), 1), HELP_Y, help, EDGE, 1);
        c
    }

    fn draw_tabs(&self, c: &mut [u32]) {
        for (i, (page, name)) in PAGES.iter().enumerate() {
            let (x, y, w, h) = tab_rect(i);
            let on = *page == self.page;
            rect(c, x + TAB_SHADOW, y + TAB_SHADOW, w, h, SHADOW);
            rect(c, x, y, w, h, if on { FOUR[i + 1] } else { EDGE });
            inner(c, x, y, w, h, if on { PICKED } else { PANEL });
            let ink = if on { GOLD } else { TEXT };
            let tx = x + (w - name.len() * GLYPH) / 2;
            text(c, tx, y + TAB_TEXT_Y, name, ink, 1);
        }
    }

    fn draw_item(&self, c: &mut [u32], i: usize, item: &Item) {
        let (x, y, w, h) = item_rect(self.page, i);
        let on = i == self.sel;
        panel(c, x, y, w, h, if on { GOLD } else { EDGE });
        if on {
            inner(c, x, y, w, h, PICKED);
        }
        let ink = if on { GOLD } else { TEXT };
        if self.page == Page::Games {
            draw_tile(c, (x, y), item, ink);
        } else {
            draw_row(c, (x, y, w), item, ink);
        }
    }
}

/// The title, each letter in the next bright color over a shadow.
fn draw_title(c: &mut [u32]) {
    let title = "N64RUSTY";
    let advance = GLYPH * TITLE_SCALE;
    let tx = centred(title.len(), TITLE_SCALE);
    for (i, ch) in title.chars().enumerate() {
        let s = ch.to_string();
        let x = tx + i * advance;
        let shadow = (x + TITLE_SHADOW, TITLE_Y + TITLE_SHADOW);
        text(c, shadow.0, shadow.1, &s, SHADOW, TITLE_SCALE);
        text(c, x, TITLE_Y, &s, FOUR[i % FOUR.len()], TITLE_SCALE);
    }
}

/// The message panel: heading, wrapped body, and how to close it.
fn draw_message(c: &mut [u32], head: &str, body: &str) {
    let (x, y, w, h) = MESSAGE;
    panel(c, x, y, w, h, RED);
    text(c, centred(head.len(), 1), MESSAGE_HEAD_Y, head, GOLD, 1);
    let body = wrap(&body.to_uppercase(), MESSAGE_COLS);
    for (n, line) in body.iter().take(MESSAGE_LINES).enumerate() {
        let ly = MESSAGE_BODY_Y + n * MESSAGE_LINE;
        text(c, MESSAGE_BODY_X, ly, line, TEXT, 1);
    }
    let ok = "ENTER = OK";
    text(c, centred(ok.len(), 1), MESSAGE_OK_Y, ok, EDGE, 1);
}

/// A game tile at `x, y`: a cartridge in the game's color and its name on
/// one or two lines.
fn draw_tile(c: &mut [u32], (x, y): (usize, usize), item: &Item, ink: u32) {
    let cart = match item.act {
        Act::Play(_) => FOUR[name_hash(&item.label) % FOUR.len()],
        _ => EDGE,
    };
    for ((dx, dy, w, h), color) in [(CART, cart), (CART_LABEL, TEXT), (CART_SLOT, SHADOW)] {
        rect(c, x + dx, y + dy, w, h, color);
    }
    let tx = x + TILE_TEXT_X;
    match lines(&item.label.to_uppercase(), TILE_CHARS) {
        (one, None) => text(c, tx, y + TILE_TEXT_Y, &one, ink, 1),
        (one, Some(two)) => {
            text(c, tx, y + TILE_TEXT_Y2.0, &one, ink, 1);
            text(c, tx, y + TILE_TEXT_Y2.1, &two, ink, 1);
        }
    }
}

/// A list row at `x, y`, `w` wide: its label, cut to leave room for its
/// value on the right.
fn draw_row(c: &mut [u32], (x, y, w): (usize, usize, usize), item: &Item, ink: u32) {
    let room = match &item.value {
        Some((v, _)) => ROW_CHARS - v.len().min(VALUE_CHARS),
        None => ROW_CHARS,
    };
    let shown: String = item.label.to_uppercase().chars().take(room).collect();
    text(c, x + ROW_TEXT_X, y + ROW_TEXT_Y, &shown, ink, 1);
    if let Some((v, color)) = &item.value {
        let v: String = v.chars().take(VALUE_CHARS).collect();
        let vx = x + w - ROW_TEXT_X - v.len() * GLYPH;
        text(c, vx, y + ROW_TEXT_Y, &v, *color, 1);
    }
}

#[cfg(test)]
mod tests {
    use super::super::ui::test_ui;
    use super::*;
    use std::path::Path;

    #[test]
    fn every_screen_draws() {
        let mut shots = Vec::new();
        let mut games = test_ui(Page::Games, 20);
        games.sel = 3;
        shots.push(("games", games.draw()));
        shots.push(("settings", test_ui(Page::Settings, 0).draw()));
        shots.push(("mods", test_ui(Page::Mods, 0).draw()));
        let mut err = test_ui(Page::Games, 0);
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
