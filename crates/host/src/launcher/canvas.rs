//! Pixel drawing on the 320x240 canvas: boxes, the 8x8 font, and fitting
//! text into lines.

use super::layout::{BORDER, GLYPH, H, PANEL_SHADOW, STARS, W};
use super::palette::{EDGE, PANEL, SHADOW};

/// A few dim single pixels, the same every time.
pub(super) fn stars(c: &mut [u32]) {
    for i in 0..STARS {
        let h = i.wrapping_mul(2_654_435_761);
        let (x, y) = ((h % W as u32) as usize, ((h >> 12) % H as u32) as usize);
        c[y * W + x] = if i % 3 == 0 { EDGE } else { PANEL };
    }
}

/// A chunky box: shadow, `edge`-colored border, panel fill.
pub(super) fn panel(c: &mut [u32], x: usize, y: usize, w: usize, h: usize, edge: u32) {
    rect(c, x + PANEL_SHADOW, y + PANEL_SHADOW, w, h, SHADOW);
    rect(c, x, y, w, h, edge);
    inner(c, x, y, w, h, PANEL);
}

/// Fills the inside of a bordered box at `x, y`, `w` by `h`.
pub(super) fn inner(c: &mut [u32], x: usize, y: usize, w: usize, h: usize, color: u32) {
    rect(
        c,
        x + BORDER,
        y + BORDER,
        w - 2 * BORDER,
        h - 2 * BORDER,
        color,
    );
}

/// Fills a rectangle, clipped to the canvas.
pub(super) fn rect(c: &mut [u32], x: usize, y: usize, w: usize, h: usize, color: u32) {
    for yy in y..(y + h).min(H) {
        for xx in x..(x + w).min(W) {
            c[yy * W + xx] = color;
        }
    }
}

/// Draws ASCII text with the 8x8 pixel font, each font pixel `k` wide.
pub(super) fn text(c: &mut [u32], x: usize, y: usize, s: &str, color: u32, k: usize) {
    for (n, ch) in s.chars().enumerate() {
        let glyph = font8x8::legacy::BASIC_LEGACY
            .get(ch as usize)
            .copied()
            .unwrap_or(font8x8::legacy::BASIC_LEGACY[b'?' as usize]);
        for (row, bits) in glyph.iter().enumerate() {
            for col in 0..GLYPH {
                if bits >> col & 1 == 1 {
                    rect(c, x + (n * GLYPH + col) * k, y + row * k, k, k, color);
                }
            }
        }
    }
}

/// Splits a label into up to two lines of `n` characters, at a space
/// when one fits.
pub(super) fn lines(s: &str, n: usize) -> (String, Option<String>) {
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
pub(super) fn wrap(s: &str, n: usize) -> Vec<String> {
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

/// A stable small hash of a name, to give each game its own color.
pub(super) fn name_hash(s: &str) -> usize {
    s.bytes()
        .fold(5381usize, |h, b| h.wrapping_mul(33) ^ b as usize)
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
