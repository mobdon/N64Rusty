//! Texture memory: tile descriptors, loads into the 4 KiB TMEM, and decoding
//! a tile to RGBA8.
//!
//! TMEM is kept linear: loads write rows at the tile's line stride, and
//! decoding reads them back the same way. The hardware's odd-row word swap
//! and its split of 32-bit texels across two banks cancel out between load
//! and sample when a game loads and samples with matching tiles, which is
//! how display lists are written.
//! shortcut: loads and samples that rely on the raw bank layout (rare
//! tricks) decode wrongly; emulate the layout if a game shows it.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};

use crate::dl::rgba5551;
use crate::{Texture, Wrap};

const MAX_SIZE: u32 = 1024;
/// Clamped tiles up to this many texels are decoded exactly (see `bind`).
const EXACT_TEXELS: u32 = 128 * 128;

#[derive(Clone, Copy, Default, Hash)]
pub(crate) struct Tile {
    fmt: u32,
    siz: u32,
    line: u32,
    tmem: u32,
    palette: u32,
    cm: [u32; 2],
    mask: [u32; 2],
    shift: [u32; 2],
    ul: [u32; 2], // 10.2 fixed point
    lr: [u32; 2],
}

/// A tile turned into a texture, with what is needed to map texel
/// coordinates to normalised ones.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Bound {
    pub texture: u32,
    offset: [f32; 2],
    scale: [f32; 2],
    size: [f32; 2],
}

impl Bound {
    /// Normalised coordinates for texel coordinates `st`. With `bilerp`,
    /// texel centres sit on whole coordinates, as the console filters (a GPU
    /// puts them at .5): tiles meant to overlap by one texel meet seamlessly.
    pub fn uv(&self, st: [f32; 2], bilerp: bool) -> [f32; 2] {
        let half = if bilerp { 0.5 } else { 0.0 };
        [0, 1].map(|k| (st[k] * self.scale[k] - self.offset[k] + half) / self.size[k])
    }
}

pub(crate) struct Tmem {
    mem: Box<[u8; 4096]>,
    tiles: [Tile; 8],
    image: (u32, u32, u32), // address, width, size code
    /// Decoded tiles still valid for the current frame, by (tile, palette type).
    cache: HashMap<(usize, u32), Bound>,
    /// Texture index by source key (see [`Tmem::bind`]), for this frame.
    seen: HashMap<u64, u32>,
    /// Decoded textures by source key, kept across frames: most textures
    /// are the same from one frame to the next.
    decoded: HashMap<u64, Texture>,
}

/// Decoded textures kept across frames before the cache starts over.
const DECODED_MAX: usize = 2048;

/// A fast hasher for TMEM contents (multiply and rotate over 8 bytes).
pub(crate) struct Fx(pub u64);

impl Hasher for Fx {
    fn write(&mut self, bytes: &[u8]) {
        for c in bytes.chunks(8) {
            let mut w = [0u8; 8];
            w[..c.len()].copy_from_slice(c);
            self.0 =
                (self.0.rotate_left(5) ^ u64::from_le_bytes(w)).wrapping_mul(0x517C_C1B7_2722_0A95);
        }
    }
    fn finish(&self) -> u64 {
        self.0
    }
}

impl Default for Tmem {
    fn default() -> Self {
        Self {
            mem: Box::new([0; 4096]),
            tiles: [Tile::default(); 8],
            image: (0, 1, 2),
            cache: HashMap::new(),
            seen: HashMap::new(),
            decoded: HashMap::new(),
        }
    }
}

/// Bytes swapped on odd TMEM rows: 32-bit words, or for 32-bit texels
/// (kept here as whole texels, not split into two banks) pairs of texels.
fn odd_row_swap(siz: u32) -> usize {
    if siz == 3 {
        8
    } else {
        4
    }
}

fn bits(siz: u32) -> u32 {
    4 << siz
}

impl Tmem {
    /// Forget decoded textures (a new frame has a new texture list).
    pub fn new_frame(&mut self) {
        self.cache.clear();
        self.seen.clear();
    }

    pub fn set_image(&mut self, w0: u32, addr: u32) {
        self.image = (addr, (w0 & 0xFFF) + 1, (w0 >> 19) & 3);
    }

    pub fn set_tile(&mut self, w0: u32, w1: u32) {
        self.cache.clear();
        let t = &mut self.tiles[((w1 >> 24) & 7) as usize];
        let ul = t.ul;
        let lr = t.lr;
        *t = Tile {
            fmt: (w0 >> 21) & 7,
            siz: (w0 >> 19) & 3,
            line: (w0 >> 9) & 0x1FF,
            tmem: w0 & 0x1FF,
            palette: (w1 >> 20) & 0xF,
            cm: [(w1 >> 8) & 3, (w1 >> 18) & 3],
            mask: [(w1 >> 4) & 0xF, (w1 >> 14) & 0xF],
            shift: [w1 & 0xF, (w1 >> 10) & 0xF],
            ul,
            lr,
        };
    }

    pub fn set_tile_size(&mut self, w0: u32, w1: u32) {
        self.cache.clear();
        let t = &mut self.tiles[((w1 >> 24) & 7) as usize];
        t.ul = [(w0 >> 12) & 0xFFF, w0 & 0xFFF];
        t.lr = [(w1 >> 12) & 0xFFF, w1 & 0xFFF];
    }

    fn stride(t: &Tile) -> usize {
        t.line as usize * if t.siz == 3 { 16 } else { 8 }
    }

    fn copy(&mut self, rdram: &[u8], src: usize, dst: usize, len: usize) {
        for k in 0..len {
            let d = dst + k;
            if d >= 4096 {
                break;
            }
            self.mem[d] = rdram.get(src + k).copied().unwrap_or(0);
        }
    }

    /// Swap the halves of each `2 * half` bytes in `len` bytes from `at`,
    /// as the hardware stores odd texture rows.
    fn swap_words(&mut self, at: usize, len: usize, half: usize) {
        for w in (at..at + len).step_by(2 * half) {
            for k in 0..half {
                self.mem.swap((w + k) & 0xFFF, (w + k + half) & 0xFFF);
            }
        }
    }

    pub fn load_block(&mut self, rdram: &[u8], w0: u32, w1: u32) {
        self.cache.clear();
        let tile = self.tiles[((w1 >> 24) & 7) as usize];
        let (addr, width, siz) = self.image;
        let (uls, ult) = ((w0 >> 12) & 0xFFF, w0 & 0xFFF);
        let lrs = (w1 >> 12) & 0xFFF;
        let texels = lrs.saturating_sub(uls) as usize + 1;
        let start = ((ult * width + uls) * bits(siz) / 8) as usize;
        let len = (texels * bits(siz) as usize).div_ceil(8);
        let dst = tile.tmem as usize * 8;
        self.copy(rdram, addr as usize + start, dst, len);
        // The load counts rows with `dxt` (1.11 fixed point per 64-bit word)
        // and swaps the words of odd rows; with dxt 0 the game pre-swapped them.
        let (dxt, half) = (w1 & 0xFFF, odd_row_swap(siz));
        for g in (0..len).step_by(2 * half) {
            if ((g / 8) as u32 * dxt) & 0x800 != 0 {
                self.swap_words(dst + g, 2 * half, half);
            }
        }
    }

    pub fn load_tile(&mut self, rdram: &[u8], w0: u32, w1: u32) {
        self.cache.clear();
        let tile = self.tiles[((w1 >> 24) & 7) as usize];
        let (addr, width, siz) = self.image;
        let (uls, ult) = (((w0 >> 12) & 0xFFF) >> 2, (w0 & 0xFFF) >> 2);
        let (lrs, lrt) = (((w1 >> 12) & 0xFFF) >> 2, (w1 & 0xFFF) >> 2);
        let row_bytes = ((lrs.saturating_sub(uls) + 1) * bits(siz)).div_ceil(8) as usize;
        for (r, y) in (ult..=lrt).enumerate() {
            let src = addr + (y * width + uls) * bits(siz) / 8;
            let dst = tile.tmem as usize * 8 + r * Self::stride(&tile);
            self.copy(rdram, src as usize, dst, row_bytes);
            if r % 2 == 1 {
                self.swap_words(dst, row_bytes, odd_row_swap(siz));
            }
        }
    }

    /// Palette loads write each 16-bit entry four times, one per bank.
    pub fn load_tlut(&mut self, rdram: &[u8], w1: u32) {
        self.cache.clear();
        let tile = self.tiles[((w1 >> 24) & 7) as usize];
        let count = ((w1 >> 14) & 0x3FF) as usize + 1;
        let src = self.image.0 as usize;
        for i in 0..count {
            for copy in 0..4 {
                let d = tile.tmem as usize * 8 + i * 8 + copy * 2;
                self.copy(rdram, src + i * 2, d, 2);
            }
        }
    }

    fn palette(&self, index: u32, tlut: u32) -> [u8; 4] {
        let a = 0x800 + (index as usize & 0xFF) * 8;
        let c = u16::from_be_bytes([self.mem[a & 0xFFF], self.mem[(a + 1) & 0xFFF]]);
        if tlut == 3 {
            let (i, al) = ((c >> 8) as u8, c as u8);
            [i, i, i, al]
        } else {
            rgba5551(c).map(|v| (v * 255.0).round() as u8)
        }
    }

    /// The texture image the next load reads: (address, width, size code).
    pub fn image(&self) -> (u32, u32, u32) {
        self.image
    }

    /// RGBA8 at texel coordinates (`s`, `t`) of `tile`, as a copy-mode
    /// rectangle reads it: from the tile's corner, wrapped by its mask.
    pub fn texel_at(&self, tile: usize, s: f32, t: f32, tlut: u32) -> [u8; 4] {
        let tl = self.tiles[tile & 7];
        let c = |v: f32, k: usize| {
            let x = (v - tl.ul[k] as f32 / 4.0).max(0.0) as u32;
            wrap_mask(x, tl.mask[k], tl.cm[k])
        };
        self.texel(&tl, c(s, 0), c(t, 1), tlut)
    }

    /// RGBA8 of texel (`x`, `y`) of `t`.
    fn texel(&self, t: &Tile, x: u32, y: u32, tlut: u32) -> [u8; 4] {
        let row = t.tmem as usize * 8 + y as usize * Self::stride(t);
        let bitpos = x as usize * bits(t.siz) as usize;
        // Odd rows are stored with their 32-bit words swapped.
        let swap = if y % 2 == 1 { odd_row_swap(t.siz) } else { 0 };
        let at = |k: usize| self.mem[((row + bitpos / 8 + k) ^ swap) & 0xFFF];
        let nib = || {
            if bitpos.is_multiple_of(8) {
                at(0) >> 4
            } else {
                at(0) & 0xF
            }
        };
        match (t.fmt, t.siz) {
            (0, 2) => rgba5551(u16::from_be_bytes([at(0), at(1)])).map(|v| (v * 255.0) as u8),
            (0, 3) => [at(0), at(1), at(2), at(3)],
            // With the palette switched on (other modes), every 4- and
            // 8-bit texel is an index into it, whatever format the tile
            // names: some games leave that at RGBA.
            (f, 0) if f == 2 || tlut >= 2 => self.palette((t.palette << 4) | nib() as u32, tlut),
            (f, 1) if f == 2 || tlut >= 2 => self.palette(at(0) as u32, tlut),
            (3, 0) => {
                let n = nib();
                let i = ((n >> 1) as u16 * 255 / 7) as u8;
                [i, i, i, (n & 1) * 255]
            }
            (3, 1) => {
                let (i, a) = ((at(0) >> 4) * 17, (at(0) & 0xF) * 17);
                [i, i, i, a]
            }
            (3, 2) => [at(0), at(0), at(0), at(1)],
            (_, 0) => [nib() * 17; 4],
            (_, 1) => [at(0); 4],
            // YUV and unusual combinations: a loud colour so they show up.
            _ => [255, 0, 255, 255],
        }
    }

    /// Decode `tile` (if not already this frame) into `textures`.
    pub fn bind(&mut self, tile: usize, tlut: u32, textures: &mut Vec<Texture>) -> Bound {
        if let Some(b) = self.cache.get(&(tile, tlut)) {
            return *b;
        }
        let t = self.tiles[tile];
        let mut size = [0u32; 2];
        let mut wrap = [Wrap::Repeat; 2];
        let mut scale = [1.0f32; 2];
        let tile_size = |k: usize| (t.lr[k].saturating_sub(t.ul[k]) >> 2) + 1;
        // A clamped tile far larger than its mask repeats the masked texels
        // until the clamp. Small ones are decoded exactly, since the clamp
        // after the repeats is visible (a mirrored emblem, a face detail).
        // Big ones would mean hundreds of copies on every draw: decode one
        // period and let it wrap.
        // shortcut: on big tiles the clamp at the far edge is lost; decode
        // it exactly if a game shows a seam there.
        let big = tile_size(0) * tile_size(1) > EXACT_TEXELS;
        for k in 0..2 {
            let tile_size = tile_size(k);
            let period = 1 << t.mask[k].min(10);
            // With no mask the hardware cannot wrap, so it clamps whatever
            // the clamp bit says.
            let clamp = (t.cm[k] & 2 != 0 || t.mask[k] == 0)
                && !(big && t.mask[k] != 0 && tile_size > period);
            size[k] = if t.mask[k] != 0 && !clamp {
                1 << t.mask[k].min(10)
            } else {
                tile_size
            }
            .min(MAX_SIZE);
            wrap[k] = if clamp {
                Wrap::Clamp
            } else if t.cm[k] & 1 != 0 {
                Wrap::Mirror
            } else {
                Wrap::Repeat
            };
            scale[k] = match t.shift[k] {
                0 => 1.0,
                s @ 1..=10 => 1.0 / (1 << s) as f32,
                s => (1 << (16 - s)) as f32,
            };
        }
        // The texture follows from the tile, the palette type and TMEM.
        let mut h = Fx(0);
        (t, tlut).hash(&mut h);
        h.write(&self.mem[..]);
        let key = h.finish();
        let texture = match self.seen.get(&key) {
            Some(&i) => i,
            None => {
                if self.decoded.len() >= DECODED_MAX {
                    self.decoded.clear();
                }
                let tex = match self.decoded.get(&key) {
                    Some(tex) => tex.clone(),
                    None => {
                        let mut rgba = Vec::with_capacity((size[0] * size[1] * 4) as usize);
                        for y in 0..size[1] {
                            for x in 0..size[0] {
                                let (x, y) = (
                                    wrap_mask(x, t.mask[0], t.cm[0]),
                                    wrap_mask(y, t.mask[1], t.cm[1]),
                                );
                                rgba.extend_from_slice(&self.texel(&t, x, y, tlut));
                            }
                        }
                        let tex = Texture {
                            width: size[0],
                            height: size[1],
                            rgba,
                            wrap,
                        };
                        self.decoded.insert(key, tex.clone());
                        tex
                    }
                };
                textures.push(tex);
                let i = textures.len() as u32 - 1;
                self.seen.insert(key, i);
                i
            }
        };
        let b = Bound {
            texture,
            offset: [t.ul[0] as f32 / 4.0, t.ul[1] as f32 / 4.0],
            scale,
            size: size.map(|s| s as f32),
        };
        self.cache.insert((tile, tlut), b);
        b
    }
}

/// Apply a tile mask to a texel coordinate, mirroring every other period
/// when the mirror bit is set.
fn wrap_mask(c: u32, mask: u32, cm: u32) -> u32 {
    if mask == 0 {
        return c;
    }
    let period = 1 << mask.min(10);
    let m = c & (period - 1);
    if cm & 1 != 0 && (c >> mask.min(10)) & 1 != 0 {
        period - 1 - m
    } else {
        m
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ia4_texels_use_full_intensity_range() {
        let mut m = Tmem::default();
        m.mem[0] = 0xF6; // texels 0xF (i 7, a 1) and 0x6 (i 3, a 0)
        let t = Tile {
            fmt: 3,
            siz: 0,
            ..Tile::default()
        };
        assert_eq!(m.texel(&t, 0, 0, 0), [255, 255, 255, 255]);
        assert_eq!(m.texel(&t, 1, 0, 0), [109, 109, 109, 0]);
    }

    #[test]
    fn an_8_bit_texel_goes_through_the_palette_when_it_is_on() {
        let mut m = Tmem::default();
        m.mem[0] = 5; // texel 0 is index 5
        m.mem[0x800 + 5 * 8..0x800 + 5 * 8 + 2].copy_from_slice(&0xF801u16.to_be_bytes());
        // The tile says RGBA, 8 bits.
        let t = Tile {
            fmt: 0,
            siz: 1,
            ..Tile::default()
        };
        assert_eq!(m.texel(&t, 0, 0, 2), [255, 0, 0, 255], "palette on: red");
        assert_eq!(m.texel(&t, 0, 0, 0), [5; 4], "palette off: the byte");
    }

    /// Odd rows are stored swapped but read back in order, whether the load
    /// swapped them (load block with dxt, load tile) or the game did (dxt 0).
    #[test]
    fn odd_rows_read_back_in_order() {
        // 4x2 texels, IA16 then RGBA32; texel i has last byte i + 1.
        for (fmt, siz, bytes) in [(3u32, 2u32, 2usize), (0, 3, 4)] {
            let lin: Vec<u8> = (1..=8u8)
                .flat_map(|t| {
                    let mut v = vec![0; bytes];
                    v[bytes - 1] = t;
                    v
                })
                .collect();
            let mut pre = lin.clone();
            pre[4 * bytes..].rotate_left(2 * bytes);
            let dxt = 0x800 / (bytes as u32 / 2);
            for (ram, load) in [(&lin, dxt), (&pre, 0), (&lin, 0xF4)] {
                let mut m = Tmem::default();
                m.set_image(siz << 19 | 3, 0);
                m.set_tile(fmt << 21 | siz << 19 | 1 << 9, 0);
                match load {
                    0xF4 => m.load_tile(ram, 0, 12 << 12 | 4),
                    _ => m.load_block(ram, 0, 7 << 12 | load),
                }
                let t = m.tiles[0];
                let got: Vec<u8> = (0..8).map(|i| m.texel(&t, i % 4, i / 4, 0)[3]).collect();
                assert_eq!(got, [1, 2, 3, 4, 5, 6, 7, 8], "siz {siz} load {load:#x}");
            }
        }
    }

    /// A small clamped tile twice its mask keeps the clamp: mirrored once,
    /// then the edge texel, not a third copy.
    #[test]
    fn small_clamped_tile_decodes_exactly() {
        let mut m = Tmem::default();
        m.tiles[0] = Tile {
            fmt: 0,
            siz: 2,
            cm: [3, 3],
            mask: [3, 3],
            lr: [15 << 2, 15 << 2],
            ..Tile::default()
        };
        let mut textures = Vec::new();
        let b = m.bind(0, 0, &mut textures);
        let tex = &textures[b.texture as usize];
        assert_eq!((tex.width, tex.height), (16, 16));
        assert_eq!(tex.wrap, [Wrap::Clamp; 2]);
    }
}
