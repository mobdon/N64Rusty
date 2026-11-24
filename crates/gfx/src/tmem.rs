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
use std::hash::{DefaultHasher, Hash, Hasher};

use crate::dl::rgba5551;
use crate::{Texture, Wrap};

const MAX_SIZE: u32 = 1024;

#[derive(Clone, Copy, Default)]
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
    /// Normalised coordinates for texel coordinates `st`.
    pub fn uv(&self, st: [f32; 2]) -> [f32; 2] {
        [0, 1].map(|k| (st[k] * self.scale[k] - self.offset[k]) / self.size[k])
    }
}

pub(crate) struct Tmem {
    mem: Box<[u8; 4096]>,
    tiles: [Tile; 8],
    image: (u32, u32, u32), // address, width, size code
    /// Decoded tiles still valid for the current frame, by (tile, palette type).
    cache: HashMap<(usize, u32), Bound>,
    /// Texture index by content hash, for this frame.
    seen: HashMap<u64, u32>,
}

impl Default for Tmem {
    fn default() -> Self {
        Self {
            mem: Box::new([0; 4096]),
            tiles: [Tile::default(); 8],
            image: (0, 1, 2),
            cache: HashMap::new(),
            seen: HashMap::new(),
        }
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

    pub fn load_block(&mut self, rdram: &[u8], w0: u32, w1: u32) {
        self.cache.clear();
        let tile = self.tiles[((w1 >> 24) & 7) as usize];
        let (addr, width, siz) = self.image;
        let (uls, ult) = ((w0 >> 12) & 0xFFF, w0 & 0xFFF);
        let lrs = (w1 >> 12) & 0xFFF;
        let texels = lrs.saturating_sub(uls) as usize + 1;
        let start = ((ult * width + uls) * bits(siz) / 8) as usize;
        let len = (texels * bits(siz) as usize).div_ceil(8);
        self.copy(rdram, addr as usize + start, tile.tmem as usize * 8, len);
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

    /// RGBA8 of texel (`x`, `y`) of `t`.
    fn texel(&self, t: &Tile, x: u32, y: u32, tlut: u32) -> [u8; 4] {
        let row = t.tmem as usize * 8 + y as usize * Self::stride(t);
        let bitpos = x as usize * bits(t.siz) as usize;
        let at = |k: usize| self.mem[(row + bitpos / 8 + k) & 0xFFF];
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
            (2, 0) => self.palette((t.palette << 4) | nib() as u32, tlut),
            (2, 1) => self.palette(at(0) as u32, tlut),
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
        for k in 0..2 {
            let tile_size = (t.lr[k].saturating_sub(t.ul[k]) >> 2) + 1;
            let clamp = t.cm[k] & 2 != 0;
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
        let mut h = DefaultHasher::new();
        tex.hash(&mut h);
        let key = h.finish();
        let texture = match self.seen.get(&key) {
            Some(&i) if textures.get(i as usize) == Some(&tex) => i,
            _ => {
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

impl Hash for Texture {
    fn hash<H: Hasher>(&self, state: &mut H) {
        (self.width, self.height, &self.rgba, self.wrap).hash(state);
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
}
