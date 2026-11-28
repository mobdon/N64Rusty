//! Where everything sits on the 320x240 canvas, in canvas pixels, grouped
//! by screen element. Both drawing and pointer hits read these, so a click
//! lands on what is drawn.

use super::ui::Page;

/// Canvas width in pixels: the console's picture width.
pub const W: usize = 320;
/// Canvas height in pixels: the console's picture height.
pub const H: usize = 240;

/// A rectangle on the canvas: x, y, width, height.
pub(super) type Rect = (usize, usize, usize, usize);

/// The font: each glyph is this many pixels wide and high.
pub(super) const GLYPH: usize = 8;

/// Panels and tabs: the border's thickness inside their rectangle.
pub(super) const BORDER: usize = 2;
/// Panels: how far their shadow falls right and down.
pub(super) const PANEL_SHADOW: usize = 3;

/// The title: letters this many font pixels per canvas pixel.
pub(super) const TITLE_SCALE: usize = 2;
/// The title's top.
pub(super) const TITLE_Y: usize = 6;
/// How far the title's shadow falls right and down.
pub(super) const TITLE_SHADOW: usize = 2;

/// Tabs: the first one's left edge, and the step to the next.
pub(super) const TAB_X: usize = 12;
pub(super) const TAB_STEP: usize = 100;
/// Tabs: top, width and height.
pub(super) const TAB_Y: usize = 30;
pub(super) const TAB_W: usize = 92;
pub(super) const TAB_H: usize = 16;
/// Tabs: how far their shadow falls right and down.
pub(super) const TAB_SHADOW: usize = 2;
/// Tabs: the name's top inside the tab.
pub(super) const TAB_TEXT_Y: usize = 4;

/// Game tiles, in two columns: the first's corner and the step to the next
/// column and row.
pub(super) const TILE_X: usize = 12;
pub(super) const TILE_Y: usize = 56;
pub(super) const TILE_STEP_X: usize = 152;
pub(super) const TILE_STEP_Y: usize = 33;
/// Game tiles: width and height.
pub(super) const TILE_W: usize = 144;
pub(super) const TILE_H: usize = 28;
/// Game tiles: columns.
pub(super) const TILE_COLS: usize = 2;
/// Game tiles: the cartridge picture inside the tile.
pub(super) const CART: Rect = (6, 6, 12, 16);
/// Game tiles: the cartridge's label.
pub(super) const CART_LABEL: Rect = (8, 8, 8, 6);
/// Game tiles: the cartridge's slot.
pub(super) const CART_SLOT: Rect = (8, 18, 8, 2);
/// Game tiles: the name's left edge, and its top as one line or as two.
pub(super) const TILE_TEXT_X: usize = 22;
pub(super) const TILE_TEXT_Y: usize = 10;
pub(super) const TILE_TEXT_Y2: (usize, usize) = (5, 15);
/// Game tiles: characters per line of the name.
pub(super) const TILE_CHARS: usize = 14;

/// List rows (Mods, Settings): the first's corner and the step to the next.
pub(super) const ROW_X: usize = 12;
pub(super) const ROW_Y: usize = 56;
pub(super) const ROW_STEP: usize = 21;
/// List rows: width and height.
pub(super) const ROW_W: usize = 296;
pub(super) const ROW_H: usize = 19;
/// List rows: the label's left edge (and the value's distance from the
/// right edge), and the text's top.
pub(super) const ROW_TEXT_X: usize = 8;
pub(super) const ROW_TEXT_Y: usize = 6;
/// List rows: characters for label and value together, and at most for the
/// value.
pub(super) const ROW_CHARS: usize = 34;
pub(super) const VALUE_CHARS: usize = 14;

/// The message panel: its rectangle.
pub(super) const MESSAGE: Rect = (20, 50, 280, 160);
/// The message panel: the heading's top.
pub(super) const MESSAGE_HEAD_Y: usize = 62;
/// The message panel: the body's corner, line step, characters per line and
/// lines shown.
pub(super) const MESSAGE_BODY_X: usize = 32;
pub(super) const MESSAGE_BODY_Y: usize = 82;
pub(super) const MESSAGE_LINE: usize = 10;
pub(super) const MESSAGE_COLS: usize = 32;
pub(super) const MESSAGE_LINES: usize = 10;
/// The message panel: the "ENTER = OK" line's top.
pub(super) const MESSAGE_OK_Y: usize = 196;

/// The help line's top, at the bottom of the screen.
pub(super) const HELP_Y: usize = 228;

/// The background's stars: how many.
pub(super) const STARS: u32 = 60;

/// The left edge that centres `chars` characters `k` font pixels wide.
pub(super) fn centred(chars: usize, k: usize) -> usize {
    (W - chars * GLYPH * k) / 2
}

/// Tab `i`.
pub(super) fn tab_rect(i: usize) -> Rect {
    (TAB_X + i * TAB_STEP, TAB_Y, TAB_W, TAB_H)
}

/// Item `i` on a page: a tile in two columns, or a list row.
pub(super) fn item_rect(page: Page, i: usize) -> Rect {
    match page {
        Page::Games => (
            TILE_X + (i % TILE_COLS) * TILE_STEP_X,
            TILE_Y + (i / TILE_COLS) * TILE_STEP_Y,
            TILE_W,
            TILE_H,
        ),
        _ => (ROW_X, ROW_Y + i * ROW_STEP, ROW_W, ROW_H),
    }
}
