//! Builds synthetic display lists in a fake guest RAM. Every command is
//! encoded by hand here from the F3DEX2 layout; no game data is involved.
#![allow(dead_code, clippy::precedence)]

use n64rusty_gfx::Combiner;

pub const RAM_SIZE: usize = 1 << 20;

/// Big-endian guest RAM with a bump allocator.
pub struct Ram {
    pub bytes: Vec<u8>,
    next: u32,
}

impl Default for Ram {
    fn default() -> Self {
        Self {
            bytes: vec![0; RAM_SIZE],
            next: 0x1000,
        }
    }
}

impl Ram {
    /// Copy `data` into RAM, 8-byte aligned; returns its physical address.
    pub fn put(&mut self, data: &[u8]) -> u32 {
        let at = self.next;
        self.bytes[at as usize..at as usize + data.len()].copy_from_slice(data);
        self.next = (at + data.len() as u32).next_multiple_of(8);
        at
    }

    /// Place a display list; returns its address.
    pub fn dl(&mut self, dl: &Dl) -> u32 {
        let bytes: Vec<u8> = dl.0.iter().flat_map(|w| w.to_be_bytes()).collect();
        self.put(&bytes)
    }

    pub fn matrix(&mut self, m: [[f32; 4]; 4]) -> u32 {
        let fixed: Vec<i32> = m.iter().flatten().map(|v| (v * 65536.0) as i32).collect();
        let mut b = Vec::new();
        for f in &fixed {
            b.extend_from_slice(&((f >> 16) as i16).to_be_bytes());
        }
        for f in &fixed {
            b.extend_from_slice(&(*f as u16).to_be_bytes());
        }
        self.put(&b)
    }

    pub fn vertices(&mut self, v: &[V]) -> u32 {
        let mut b = Vec::new();
        for v in v {
            for c in [v.pos[0], v.pos[1], v.pos[2], 0, v.st[0], v.st[1]] {
                b.extend_from_slice(&c.to_be_bytes());
            }
            b.extend_from_slice(&v.rgba);
        }
        self.put(&b)
    }

    pub fn viewport(&mut self, scale: [i16; 2], trans: [i16; 2]) -> u32 {
        let mut b = Vec::new();
        for c in [
            scale[0] * 4,
            scale[1] * 4,
            511,
            0,
            trans[0] * 4,
            trans[1] * 4,
            511,
            0,
        ] {
            b.extend_from_slice(&c.to_be_bytes());
        }
        self.put(&b)
    }
}

/// A vertex: position, texel coordinate in S10.5, colour or normal.
#[derive(Clone, Copy)]
pub struct V {
    pub pos: [i16; 3],
    pub st: [i16; 2],
    pub rgba: [u8; 4],
}

pub fn v(x: i16, y: i16, z: i16, rgba: [u8; 4]) -> V {
    V {
        pos: [x, y, z],
        st: [0, 0],
        rgba,
    }
}

pub const PUSH: u32 = 1;
pub const LOAD: u32 = 2;
pub const PROJECTION: u32 = 4;

pub const G_ZBUFFER: u32 = 0x1;
pub const G_SHADE: u32 = 0x4;
pub const G_CULL_FRONT: u32 = 0x200;
pub const G_CULL_BACK: u32 = 0x400;
pub const G_LIGHTING: u32 = 0x2_0000;
pub const G_SHADING_SMOOTH: u32 = 0x20_0000;

pub const CYCLE_1: u32 = 0;
pub const CYCLE_FILL: u32 = 3 << 20;
pub const FILTER_POINT: u32 = 0;
/// Depth compare and update.
pub const ZBUF: u32 = 0x30;
/// Force blend with IN * IN_ALPHA + MEM * (1 - IN_ALPHA), both cycles.
pub const XLU: u32 = 0x4000 | (1 << 22) | (1 << 20);

// Combiner input codes.
pub const COMBINED: u8 = 0;
pub const TEXEL0: u8 = 1;
pub const PRIM: u8 = 3;
pub const SHADE: u8 = 4;
pub const ENV: u8 = 5;

/// Combiner outputting one input, colour and alpha.
pub fn pass(input: u8) -> u64 {
    Combiner::both([15, 15, 31, input], [7, 7, 7, input]).to_raw()
}

/// A display list under construction.
#[derive(Default)]
pub struct Dl(pub Vec<u32>);

impl Dl {
    fn c(&mut self, w0: u32, w1: u32) -> &mut Self {
        self.0.extend([w0, w1]);
        self
    }
    pub fn vtx(&mut self, addr: u32, n: u32, v0: u32) -> &mut Self {
        self.c(0x01 << 24 | n << 12 | (v0 + n) << 1, addr)
    }
    pub fn tri(&mut self, a: u32, b: u32, c: u32) -> &mut Self {
        self.c(0x05 << 24 | (a * 2) << 16 | (b * 2) << 8 | c * 2, 0)
    }
    pub fn tri2(&mut self, a: [u32; 3], b: [u32; 3]) -> &mut Self {
        let i = |t: [u32; 3]| (t[0] * 2) << 16 | (t[1] * 2) << 8 | t[2] * 2;
        self.c(0x06 << 24 | i(a), i(b))
    }
    pub fn mtx(&mut self, addr: u32, params: u32) -> &mut Self {
        self.c(0xDA << 24 | 0x38 << 16 | (params ^ PUSH), addr)
    }
    pub fn pop(&mut self, n: u32) -> &mut Self {
        self.c(0xD8 << 24 | 0x38 << 16 | 2, 64 * n)
    }
    pub fn geometry(&mut self, clear: u32, set: u32) -> &mut Self {
        self.c(0xD9 << 24 | (!clear & 0xFF_FFFF), set)
    }
    pub fn segment(&mut self, seg: u32, base: u32) -> &mut Self {
        self.c(0xDB << 24 | 0x06 << 16 | seg * 4, base)
    }
    pub fn num_lights(&mut self, n: u32) -> &mut Self {
        self.c(0xDB << 24 | 0x02 << 16, n * 24)
    }
    /// Light `n`, counting from 1 (the ambient light is `num_lights + 1`).
    pub fn light(&mut self, addr: u32, n: u32) -> &mut Self {
        self.c(
            0xDC << 24 | ((16 - 1) / 8) << 19 | ((n * 24 + 24) / 8) << 8 | 10,
            addr,
        )
    }
    pub fn viewport(&mut self, addr: u32) -> &mut Self {
        self.c(0xDC << 24 | ((16 - 1) / 8) << 19 | 8, addr)
    }
    pub fn call(&mut self, addr: u32) -> &mut Self {
        self.c(0xDE << 24, addr)
    }
    pub fn branch(&mut self, addr: u32) -> &mut Self {
        self.c(0xDE << 24 | 1 << 16, addr)
    }
    pub fn end(&mut self) -> &mut Self {
        self.c(0xDF << 24, 0)
    }
    pub fn texture(&mut self, on: bool, tile: u32) -> &mut Self {
        self.c(0xD7 << 24 | tile << 8 | (on as u32) << 1, 0xFFFF_FFFF)
    }
    pub fn other_modes(&mut self, h: u32, l: u32) -> &mut Self {
        self.c(0xEF << 24 | h, l)
    }
    pub fn combine(&mut self, raw: u64) -> &mut Self {
        self.c(0xFC << 24 | (raw >> 32) as u32, raw as u32)
    }
    pub fn prim(&mut self, rgba: u32) -> &mut Self {
        self.c(0xFA << 24, rgba)
    }
    pub fn env(&mut self, rgba: u32) -> &mut Self {
        self.c(0xFB << 24, rgba)
    }
    pub fn fill_color(&mut self, c: u32) -> &mut Self {
        self.c(0xF7 << 24, c)
    }
    /// Fill rectangle, corners inclusive in fill mode, in whole pixels.
    pub fn fill_rect(&mut self, x0: u32, y0: u32, x1: u32, y1: u32) -> &mut Self {
        self.c(
            0xF6 << 24 | (x1 * 4) << 12 | y1 * 4,
            (x0 * 4) << 12 | y0 * 4,
        )
    }
    /// Texture rectangle in whole pixels; `st` in S10.5, steps in S5.10.
    pub fn tex_rect(&mut self, tile: u32, xy: [u32; 4], st: [u16; 2], step: [u16; 2]) -> &mut Self {
        self.c(
            0xE4 << 24 | (xy[2] * 4) << 12 | xy[3] * 4,
            tile << 24 | (xy[0] * 4) << 12 | xy[1] * 4,
        );
        self.c(0xE1 << 24, (st[0] as u32) << 16 | st[1] as u32);
        self.c(0xF1 << 24, (step[0] as u32) << 16 | step[1] as u32)
    }
    /// Scissor box in whole pixels.
    pub fn scissor(&mut self, x0: u32, y0: u32, x1: u32, y1: u32) -> &mut Self {
        self.c(
            0xED << 24 | (x0 * 4) << 12 | y0 * 4,
            (x1 * 4) << 12 | y1 * 4,
        )
    }
    /// `G_MODIFYVTX`: field offset `what` (0x10 colour, 0x14 st, 0x18
    /// screen xy, 0x1C screen z) of loaded vertex `v`.
    pub fn modify_vtx(&mut self, what: u32, v: u32, value: u32) -> &mut Self {
        self.c(0x02 << 24 | what << 16 | v * 2, value)
    }
    pub fn cimg(&mut self, addr: u32, width: u32) -> &mut Self {
        self.c(0xFF << 24 | 2 << 19 | (width - 1), addr)
    }
    pub fn zimg(&mut self, addr: u32) -> &mut Self {
        self.c(0xFE << 24, addr)
    }
    pub fn timg(&mut self, fmt: u32, siz: u32, width: u32, addr: u32) -> &mut Self {
        self.c(0xFD << 24 | fmt << 21 | siz << 19 | (width - 1), addr)
    }
    pub fn tile(&mut self, fmt: u32, siz: u32, line: u32, tmem: u32, tile: u32) -> &mut Self {
        self.c(
            0xF5 << 24 | fmt << 21 | siz << 19 | line << 9 | tmem,
            tile << 24,
        )
    }
    pub fn prim_depth(&mut self, z: u16) -> &mut Self {
        self.c(0xEE << 24, (z as u32) << 16)
    }
    /// Load block for textures whose rows are one 64-bit word (dxt 1.0).
    pub fn load_block(&mut self, tile: u32, texels: u32) -> &mut Self {
        self.c(0xF3 << 24, tile << 24 | (texels - 1) << 12 | 0x800)
    }
    pub fn load_tlut(&mut self, tile: u32, count: u32) -> &mut Self {
        self.c(0xF0 << 24, tile << 24 | ((count - 1) << 2) << 12)
    }
    pub fn tile_size(&mut self, tile: u32, w: u32, h: u32) -> &mut Self {
        self.c(0xF2 << 24, tile << 24 | ((w - 1) * 4) << 12 | (h - 1) * 4)
    }
}

/// The usual frame setup: a 320-wide colour image at 0x10_0000 (outside
/// the fake RAM, never read), a viewport covering it, and a projection that
/// maps object coordinates -100..100 to the screen.
pub fn setup(ram: &mut Ram, dl: &mut Dl) {
    let vp = ram.viewport([160, 120], [160, 120]);
    let proj = ram.matrix([
        [0.01, 0.0, 0.0, 0.0],
        [0.0, 0.01, 0.0, 0.0],
        [0.0, 0.0, 0.01, 0.0],
        [0.0, 0.0, 0.0, 1.0],
    ]);
    let ident = ram.matrix(n64rusty_gfx::IDENTITY);
    dl.cimg(0x0F_0000, 320)
        .zimg(0x0E_0000)
        .viewport(vp)
        .mtx(proj, LOAD | PROJECTION)
        .mtx(ident, LOAD);
}
