//! The window icon: a block in the four colors with a white "R".

use super::layout::GLYPH;
use super::palette::{FOUR, TEXT};

/// The icon's width and height in pixels.
const SIZE: usize = 32;
/// Each quarter of the icon, in one of the four colors.
const QUARTER: usize = SIZE / 2;
/// The letter's distance from the top-left corner.
const INSET: usize = 4;
/// Icon pixels per font pixel of the letter.
const LETTER_SCALE: usize = 3;

/// The window icon: a block in the four colors with a white "R".
pub fn icon() -> Option<winit::window::Icon> {
    let mut px = vec![0u8; SIZE * SIZE * 4];
    let glyph = font8x8::legacy::BASIC_LEGACY[b'R' as usize];
    for y in 0..SIZE {
        for x in 0..SIZE {
            let gx = x.wrapping_sub(INSET) / LETTER_SCALE;
            let gy = y.wrapping_sub(INSET) / LETTER_SCALE;
            let ink = gx < GLYPH && gy < GLYPH && glyph[gy] >> gx & 1 == 1;
            let quarter = FOUR[(y / QUARTER) * 2 + x / QUARTER];
            let color = if ink { TEXT } else { quarter };
            px[(y * SIZE + x) * 4..][..4].copy_from_slice(&[
                (color >> 16) as u8,
                (color >> 8) as u8,
                color as u8,
                255,
            ]);
        }
    }
    winit::window::Icon::from_rgba(px, SIZE as u32, SIZE as u32).ok()
}
