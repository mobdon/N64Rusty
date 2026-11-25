//! Display-list interpreter for the F3DEX2 command encoding.
//!
//! Command numbers, field layouts and fixed-point formats follow the public
//! descriptions credited in CREDITS.md ("Graphics" section).

use std::fmt;

use crate::tmem::Tmem;
use crate::{
    mat_mul, Command, Draw, Frame, Mat4, RenderState, Transform, Vertex, Viewport, IDENTITY,
};

/// A display list could not be followed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GfxError {
    /// A read went past the end of guest RAM.
    OutOfRange { addr: u32, at: u32 },
    /// Display-list calls nested deeper than the microcode allows.
    StackOverflow { at: u32 },
    /// The list did not end within the command budget (a loop).
    TooLong { at: u32 },
}

impl fmt::Display for GfxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GfxError::OutOfRange { addr, at } => {
                write!(
                    f,
                    "read of {addr:#010x} outside RAM (command at {at:#010x})"
                )
            }
            GfxError::StackOverflow { at } => {
                write!(f, "display list calls nested too deep at {at:#010x}")
            }
            GfxError::TooLong { at } => write!(f, "display list did not end (at {at:#010x})"),
        }
    }
}

impl std::error::Error for GfxError {}

const VTX_BUFFER: usize = 32;
const DL_STACK: usize = 18;
const MTX_STACK: usize = 32;
const MAX_COMMANDS: usize = 1 << 20;
const MAX_WARNINGS: usize = 64;

// Geometry mode bits (F3DEX2).
const G_CULL_FRONT: u32 = 0x0200;
const G_CULL_BACK: u32 = 0x0400;
const G_LIGHTING: u32 = 0x0002_0000;

/// A loaded vertex, after the vertex stage.
#[derive(Clone, Copy, Default)]
struct Loaded {
    pos: [f32; 3],
    transform: u32,
    color: [f32; 4],
    /// Texture coordinate in texels, after the `G_TEXTURE` scale.
    st: [f32; 2],
}

#[derive(Clone, Copy, Default)]
struct Light {
    color: [f32; 3],
    dir: [f32; 3],
}

/// Follows display lists and records what they draw.
///
/// State (segments, matrices, modes, texture memory) carries over between
/// [`Interpreter::run`] calls, as it does in the coprocessor between tasks.
/// Call [`Interpreter::take_frame`] once per displayed frame.
pub struct Interpreter {
    segments: [u32; 16],
    modelview: Vec<Mat4>,
    projection: Mat4,
    /// The combined matrix after a `gSPInsertMatrix` edit, until the next
    /// matrix load recomputes it.
    mvp: Option<Mat4>,
    viewport: Viewport,
    vtx: [Loaded; VTX_BUFFER],
    geometry: u32,
    lights: [Light; 8],
    num_lights: usize,
    tex_scale: [f32; 2],
    tex_tile: usize,
    tex_on: bool,
    state: RenderState,
    fill_color: u32,
    prim_depth: f32,
    cimg: (u32, u32, u32), // address, width, pixel size code
    zimg: u32,
    rdphalf1: u32,
    tmem: Tmem,
    rect_transform: Option<u32>,
    /// Running the 2D (sprite and background) microcode a display list
    /// switched to with `G_LOAD_UCODE`, whose low opcodes differ.
    ucode_2d: bool,
    frame: Frame,
}

impl Default for Interpreter {
    fn default() -> Self {
        Self::new()
    }
}

impl Interpreter {
    pub fn new() -> Self {
        Self {
            segments: [0; 16],
            modelview: vec![IDENTITY],
            projection: IDENTITY,
            mvp: None,
            viewport: Viewport {
                scale: [160.0, 120.0],
                trans: [160.0, 120.0],
            },
            vtx: [Loaded::default(); VTX_BUFFER],
            geometry: 0,
            lights: [Light::default(); 8],
            num_lights: 0,
            tex_scale: [1.0; 2],
            tex_tile: 0,
            tex_on: false,
            state: RenderState {
                combine: 0,
                other_h: 0,
                other_l: 0,
                geometry: 0,
                prim: [1.0; 4],
                prim_lod: 0.0,
                env: [1.0; 4],
                blend: [0.0; 4],
                fog: [0.0; 4],
                textures: [None; 2],
                rect: false,
                texel_exact: false,
                color_image: 0,
                scissor: None,
            },
            fill_color: 0,
            prim_depth: 0.0,
            cimg: (0, 320, 2),
            zimg: u32::MAX,
            rdphalf1: 0,
            tmem: Tmem::default(),
            rect_transform: None,
            ucode_2d: false,
            frame: Frame {
                width: 320,
                ..Frame::default()
            },
        }
    }

    /// Hand over everything recorded since the last call.
    pub fn take_frame(&mut self) -> Frame {
        self.rect_transform = None;
        self.tmem.new_frame();
        let next = Frame {
            width: self.frame.width,
            color_image: self.frame.color_image,
            ..Frame::default()
        };
        std::mem::replace(&mut self.frame, next)
    }

    /// The frame being recorded.
    pub fn frame(&self) -> &Frame {
        &self.frame
    }

    /// Follow the display list at `addr` (a segmented or physical address)
    /// in `rdram`, the big-endian guest RAM.
    pub fn run(&mut self, rdram: &[u8], addr: u32) -> Result<(), GfxError> {
        let mut pc = self.phys(addr);
        let mut stack: Vec<u32> = Vec::new();
        // Every task starts on its own (3D) microcode.
        self.ucode_2d = false;
        for _ in 0..MAX_COMMANDS {
            let w0 = rd32(rdram, pc, pc)?;
            let w1 = rd32(rdram, pc + 4, pc)?;
            let at = pc;
            pc += 8;
            if self.ucode_2d && (w0 >> 24 <= 0x0B || matches!(w0 >> 24, 0xDA | 0xDC)) {
                match w0 >> 24 {
                    0x0A => self.bg_copy(rdram, w1, at)?,
                    op => self.warn(format!("unknown 2D command {op:#04x} at {at:#010x}")),
                }
                continue;
            }
            match w0 >> 24 {
                // shortcut: games switch between their 3D microcode and the
                // 2D one and back, so each load toggles; track the loaded
                // code's address if a game loads a third microcode.
                0xDD => self.ucode_2d = !self.ucode_2d,
                0x01 => self.vertices(rdram, w0, w1, at)?,
                0x02 => self.modify_vertex(w0, w1, at),
                0xED => {
                    let fixed = |v: u32, sh: u32| ((v >> sh) & 0xFFF) as f32 / 4.0;
                    self.state.scissor =
                        Some([fixed(w0, 12), fixed(w0, 0), fixed(w1, 12), fixed(w1, 0)]);
                }
                0x05 => self.tri([(w0 >> 16) & 0xFF, (w0 >> 8) & 0xFF, w0 & 0xFF], at),
                0x06 | 0x07 => {
                    self.tri([(w0 >> 16) & 0xFF, (w0 >> 8) & 0xFF, w0 & 0xFF], at);
                    self.tri([(w1 >> 16) & 0xFF, (w1 >> 8) & 0xFF, w1 & 0xFF], at);
                }
                // G_SPNOOP, G_CULLDL (drawing everything is always safe),
                // syncs, scissor and the coprocessor-only commands.
                0x00 | 0x03 | 0xE0 | 0xE6..=0xE9 | 0xF1 => {}
                0x04 => {
                    // shortcut: G_BRANCH_Z always takes the branch (the
                    // nearer, more detailed level); compare depth when a
                    // game shows the wrong level of detail.
                    pc = self.phys(self.rdphalf1);
                }
                0xD7 => {
                    // 0xFFFF is the conventional "1.0".
                    let scale = |v: u32| if v == 0xFFFF { 1.0 } else { v as f32 / 65536.0 };
                    self.tex_scale = [scale(w1 >> 16), scale(w1 & 0xFFFF)];
                    self.tex_tile = ((w0 >> 8) & 7) as usize;
                    self.tex_on = w0 & 0xFE != 0;
                }
                0xD8 => {
                    let n = (w1 / 64).max(1) as usize;
                    let keep = self.modelview.len().saturating_sub(n).max(1);
                    self.modelview.truncate(keep);
                }
                0xD9 => self.geometry = (self.geometry & (w0 & 0x00FF_FFFF)) | w1,
                0xDA => self.matrix(rdram, w0, w1, at)?,
                0xDB => self.moveword(w0, w1, at),
                0xDC => self.movemem(rdram, w0, w1, at)?,
                0xDE => {
                    if (w0 >> 16) & 0xFF == 0 {
                        if stack.len() >= DL_STACK {
                            return Err(GfxError::StackOverflow { at });
                        }
                        stack.push(pc);
                    }
                    pc = self.phys(w1);
                }
                0xDF => match stack.pop() {
                    Some(ret) => pc = ret,
                    None => return Ok(()),
                },
                0xE1 => self.rdphalf1 = w1,
                0xE2 | 0xE3 => {
                    // A malformed length past 32 bits would overflow the shift.
                    let len = ((w0 & 0xFF) + 1).min(32);
                    let shift = 32u32.saturating_sub(((w0 >> 8) & 0xFF) + len);
                    let mask = (((1u64 << len) - 1) << shift) as u32;
                    let m = if w0 >> 24 == 0xE2 {
                        &mut self.state.other_l
                    } else {
                        &mut self.state.other_h
                    };
                    *m = (*m & !mask) | (w1 & mask);
                }
                0xE4 | 0xE5 => {
                    let h1 = rd32(rdram, pc + 4, at)?;
                    let h2 = rd32(rdram, pc + 12, at)?;
                    pc += 16;
                    self.texture_rect(w0, w1, h1, h2, w0 >> 24 == 0xE5);
                }
                0xEE => self.prim_depth = ((w1 >> 16) & 0x7FFF) as f32 / 32767.0, // 15-bit depth
                0xEF => {
                    self.state.other_h = w0 & 0x00FF_FFFF;
                    self.state.other_l = w1;
                }
                0xF0 => self.tmem.load_tlut(rdram, w1),
                0xF2 => self.tmem.set_tile_size(w0, w1),
                0xF3 => self.tmem.load_block(rdram, w0, w1),
                0xF4 => self.tmem.load_tile(rdram, w0, w1),
                0xF5 => self.tmem.set_tile(w0, w1),
                0xF6 => self.fill_rect(w0, w1),
                0xF7 => self.fill_color = w1,
                0xF8 => self.state.fog = rgba(w1),
                0xF9 => self.state.blend = rgba(w1),
                0xFA => {
                    self.state.prim = rgba(w1);
                    self.state.prim_lod = (w0 & 0xFF) as f32 / 255.0;
                }
                0xFB => self.state.env = rgba(w1),
                0xFC => self.state.combine = ((w0 as u64 & 0x00FF_FFFF) << 32) | w1 as u64,
                0xFD => self.tmem.set_image(w0, self.phys(w1)),
                0xFE => self.zimg = self.phys(w1),
                0xFF => {
                    self.cimg = (self.phys(w1), (w0 & 0xFFF) + 1, (w0 >> 19) & 3);
                    self.state.color_image = self.cimg.0;
                    self.frame.color_image = self.cimg.0;
                    if self.frame.width != self.cimg.1 {
                        self.frame.width = self.cimg.1;
                        self.rect_transform = None;
                    }
                }
                op => self.warn(format!("unknown command {op:#04x} at {at:#010x}")),
            }
        }
        Err(GfxError::TooLong { at: pc })
    }

    fn warn(&mut self, msg: String) {
        if self.frame.warnings.len() < MAX_WARNINGS {
            self.frame.warnings.push(msg);
        }
    }

    /// Resolve a segmented address to a physical one.
    fn phys(&self, addr: u32) -> u32 {
        let seg = ((addr >> 24) & 0xF) as usize;
        self.segments[seg].wrapping_add(addr & 0x00FF_FFFF) & 0x00FF_FFFF
    }

    fn matrix(&mut self, rdram: &[u8], w0: u32, w1: u32, at: u32) -> Result<(), GfxError> {
        let a = self.phys(w1);
        let mut m = [[0.0f32; 4]; 4];
        for (i, row) in m.iter_mut().enumerate() {
            for (j, v) in row.iter_mut().enumerate() {
                let k = (i * 4 + j) as u32 * 2;
                let int = rd16(rdram, a + k, at)? as i16 as i32;
                let frac = rd16(rdram, a + 32 + k, at)? as i32;
                *v = ((int << 16) | frac) as f32 / 65536.0;
            }
        }
        // F3DEX2 stores the push flag inverted.
        self.mvp = None;
        let params = (w0 & 0xFF) ^ 1;
        let (push, load, projection) = (params & 1 != 0, params & 2 != 0, params & 4 != 0);
        if projection {
            self.projection = if load {
                m
            } else {
                mat_mul(&m, &self.projection)
            };
        } else {
            let top = *self.modelview.last().unwrap();
            if push && self.modelview.len() < MTX_STACK {
                self.modelview.push(top);
            }
            *self.modelview.last_mut().unwrap() = if load { m } else { mat_mul(&m, &top) };
        }
        Ok(())
    }

    fn moveword(&mut self, w0: u32, w1: u32, at: u32) {
        let (index, offset) = ((w0 >> 16) & 0xFF, w0 & 0xFFFF);
        match index {
            0x02 => self.num_lights = (w1 / 24).min(7) as usize,
            0x06 => self.segments[(offset / 4) as usize & 0xF] = w1 & 0x00FF_FFFF,
            // Fog factors, clip ratio, perspective norm: not
            // needed by this renderer.
            // Light colour (`gSPLightColor`): light n's two colour words
            // are at n * 24 and n * 24 + 4; the second is a copy.
            0x0A => {
                if offset % 24 == 0 {
                    if let Some(l) = self.lights.get_mut((offset / 24) as usize) {
                        l.color = [24, 16, 8].map(|s| ((w1 >> s) & 0xFF) as f32 / 255.0);
                    }
                }
            }
            // gSPInsertMatrix: overwrite two 16-bit halves (integer parts in
            // the first 32 bytes, fractions in the next 32) of the combined
            // modelview-projection matrix. Games build billboards this way.
            0x00 if offset < 64 => {
                let mut m = self
                    .mvp
                    .unwrap_or_else(|| mat_mul(self.modelview.last().unwrap(), &self.projection));
                let frac = offset >= 32;
                let first = ((offset % 32) / 2) as usize;
                for (k, half) in [(first, w1 >> 16), (first + 1, w1 & 0xFFFF)] {
                    let Some(v) = m.get_mut(k / 4).and_then(|r| r.get_mut(k % 4)) else {
                        continue;
                    };
                    let fixed = (*v * 65536.0).round() as i32;
                    let fixed = if frac {
                        (fixed & !0xFFFF) | half as i32
                    } else {
                        (half as i16 as i32) << 16 | (fixed & 0xFFFF)
                    };
                    *v = fixed as f32 / 65536.0;
                }
                self.mvp = Some(m);
            }
            0x04 | 0x08 | 0x0C | 0x0E => {}
            _ => self.warn(format!("moveword {index:#x} at {at:#010x}")),
        }
    }

    fn movemem(&mut self, rdram: &[u8], w0: u32, w1: u32, at: u32) -> Result<(), GfxError> {
        let a = self.phys(w1);
        let offset = ((w0 >> 8) & 0xFF) * 8;
        match w0 & 0xFF {
            8 => {
                let v = |k: u32| -> Result<f32, GfxError> {
                    Ok(rd16(rdram, a + k * 2, at)? as i16 as f32 / 4.0)
                };
                self.viewport = Viewport {
                    scale: [v(0)?, v(1)?],
                    trans: [v(4)?, v(5)?],
                };
            }
            10 => {
                // Lights start after the two look-at entries.
                let Some(n) = (offset / 24).checked_sub(2) else {
                    return Ok(()); // look-at, only used by texture generation
                };
                let b = |k: u32| rd8(rdram, a + k, at);
                let light = Light {
                    color: [
                        b(0)? as f32 / 255.0,
                        b(1)? as f32 / 255.0,
                        b(2)? as f32 / 255.0,
                    ],
                    dir: normalize([b(8)? as i8 as f32, b(9)? as i8 as f32, b(10)? as i8 as f32]),
                };
                if let Some(l) = self.lights.get_mut(n as usize) {
                    *l = light;
                }
            }
            idx => self.warn(format!("movemem {idx:#x} at {at:#010x}")),
        }
        Ok(())
    }

    fn vertices(&mut self, rdram: &[u8], w0: u32, w1: u32, at: u32) -> Result<(), GfxError> {
        let n = ((w0 >> 12) & 0xFF) as usize;
        let end = ((w0 >> 1) & 0x7F) as usize;
        if n > end || end > VTX_BUFFER {
            self.warn(format!("bad vertex load at {at:#010x}"));
            return Ok(());
        }
        let lights_mv = *self.modelview.last().unwrap();
        let (mv, projection) = match self.mvp {
            Some(m) => (m, IDENTITY),
            None => (*self.modelview.last().unwrap(), self.projection),
        };
        self.frame.transforms.push(Transform {
            modelview: mv,
            projection,
            viewport: self.viewport,
            tag: at,
        });
        let transform = self.frame.transforms.len() as u32 - 1;
        let a = self.phys(w1);
        let lit = self.geometry & G_LIGHTING != 0;
        for i in 0..n {
            let v = a + i as u32 * 16;
            let h = |k: u32| -> Result<i16, GfxError> { Ok(rd16(rdram, v + k, at)? as i16) };
            let b = |k: u32| rd8(rdram, v + k, at);
            let color = if lit {
                let n = [
                    b(12)? as i8 as f32,
                    b(13)? as i8 as f32,
                    b(14)? as i8 as f32,
                ];
                let lc = self.light(&lights_mv, n);
                [lc[0], lc[1], lc[2], b(15)? as f32 / 255.0]
            } else {
                [b(12)?, b(13)?, b(14)?, b(15)?].map(|c| c as f32 / 255.0)
            };
            self.vtx[end - n + i] = Loaded {
                pos: [h(0)? as f32, h(2)? as f32, h(4)? as f32],
                transform,
                color,
                st: [
                    h(8)? as f32 / 32.0 * self.tex_scale[0],
                    h(10)? as f32 / 32.0 * self.tex_scale[1],
                ],
            };
        }
        Ok(())
    }

    /// `G_MODIFYVTX`: overwrite one field of a loaded vertex.
    fn modify_vertex(&mut self, w0: u32, w1: u32, at: u32) {
        let i = ((w0 & 0xFFFF) / 2) as usize;
        if i >= VTX_BUFFER {
            self.warn(format!("bad vertex modify at {at:#010x}"));
            return;
        }
        match (w0 >> 16) & 0xFF {
            0x10 => self.vtx[i].color = rgba(w1),
            // Replaces the coordinate as given; the texture scale is not applied.
            0x14 => self.vtx[i].st = [(w1 >> 16) as i16 as f32 / 32.0, w1 as i16 as f32 / 32.0],
            // Screen position in quarter pixels: the vertex leaves its
            // transform and is drawn in framebuffer space.
            0x18 => {
                let t = self.rect_transform();
                let v = &mut self.vtx[i];
                v.transform = t;
                v.pos = [(w1 >> 16) as i16 as f32 / 4.0, w1 as i16 as f32 / 4.0, 0.0];
            }
            0x1C => {
                let t = self.rect_transform();
                let v = &mut self.vtx[i];
                if v.transform == t {
                    v.pos[2] = (w1 >> 16) as f32 / 32767.0 * 2.0 - 1.0;
                }
            }
            w => self.warn(format!("vertex modify {w:#x} at {at:#010x}")),
        }
    }

    /// Directional lights plus ambient (the light after the last one).
    fn light(&self, mv: &Mat4, n: [f32; 3]) -> [f32; 3] {
        let n = normalize([0, 1, 2].map(|j| (0..3).map(|k| n[k] * mv[k][j]).sum::<f32>()));
        let mut c = self.lights[self.num_lights].color;
        for l in &self.lights[..self.num_lights] {
            let d = (n[0] * l.dir[0] + n[1] * l.dir[1] + n[2] * l.dir[2]).max(0.0);
            for (c, lc) in c.iter_mut().zip(l.color) {
                *c += lc * d;
            }
        }
        c.map(|x| x.min(1.0))
    }

    fn tri(&mut self, idx: [u32; 3], at: u32) {
        let idx = idx.map(|i| (i / 2) as usize);
        if idx.iter().any(|&i| i >= VTX_BUFFER) {
            self.warn(format!("bad triangle at {at:#010x}"));
            return;
        }
        if self.geometry & (G_CULL_FRONT | G_CULL_BACK) == G_CULL_FRONT | G_CULL_BACK {
            return;
        }
        let tiles = self.tiles();
        let verts = idx.map(|i| {
            let v = self.vtx[i];
            Vertex {
                pos: v.pos,
                transform: v.transform,
                color: v.color,
                uv: tiles.map(|t| t.map_or([0.0; 2], |t| t.uv(v.st))),
            }
        });
        let mut state = self.state.clone();
        state.geometry = self.geometry;
        state.textures = tiles.map(|t| t.map(|t| t.texture));
        self.push(state, &verts);
    }

    /// Decode the draw's two tiles, if texturing is on.
    fn tiles(&mut self) -> [Option<crate::tmem::Bound>; 2] {
        if !self.tex_on {
            return [None; 2];
        }
        let tlut = (self.state.other_h >> 14) & 3;
        [0, 1].map(|k| {
            let tile = (self.tex_tile + k) & 7;
            Some(self.tmem.bind(tile, tlut, &mut self.frame.textures))
        })
    }

    fn push(&mut self, state: RenderState, verts: &[Vertex]) {
        let first = self.frame.vertices.len() as u32;
        self.frame.vertices.extend_from_slice(verts);
        if let Some(Command::Draw(d)) = self.frame.commands.last_mut() {
            if d.state == state && d.first + d.count == first {
                d.count += verts.len() as u32;
                return;
            }
        }
        self.frame.commands.push(Command::Draw(Draw {
            first,
            count: verts.len() as u32,
            state,
        }));
    }

    /// The transform that maps framebuffer pixels straight to the screen.
    fn rect_transform(&mut self) -> u32 {
        if let Some(t) = self.rect_transform {
            return t;
        }
        let (w, h) = (self.frame.width as f32, self.frame.height() as f32);
        self.frame.transforms.push(Transform {
            modelview: IDENTITY,
            projection: [
                [2.0 / w, 0.0, 0.0, 0.0],
                [0.0, -2.0 / h, 0.0, 0.0],
                [0.0, 0.0, 1.0, 0.0],
                [-1.0, 1.0, 0.0, 1.0],
            ],
            viewport: Viewport {
                scale: [w / 2.0, h / 2.0],
                trans: [w / 2.0, h / 2.0],
            },
            tag: 0,
        });
        let t = self.frame.transforms.len() as u32 - 1;
        self.rect_transform = Some(t);
        t
    }

    fn cycle_type(&self) -> u32 {
        (self.state.other_h >> 20) & 3
    }

    /// Two triangles covering `xy` (x0, y0, x1, y1 in pixels); `uv(x, y)`
    /// gives the texture coordinates at each corner.
    fn push_rect(
        &mut self,
        xy: [f32; 4],
        uv: impl Fn(usize, usize) -> [[f32; 2]; 2],
        state: RenderState,
    ) {
        let t = self.rect_transform();
        // Depth for rectangles always comes from the primitive depth.
        let z = self.prim_depth * 2.0 - 1.0;
        let corner = |x: usize, y: usize| Vertex {
            pos: [xy[x * 2], xy[1 + y * 2], z],
            transform: t,
            color: [1.0; 4],
            uv: uv(x, y),
        };
        let (a, b, c, d) = (corner(0, 0), corner(1, 0), corner(0, 1), corner(1, 1));
        self.push(state, &[a, c, b, b, c, d]);
    }

    fn fill_rect(&mut self, w0: u32, w1: u32) {
        let fixed = |v: u32, sh: u32| ((v >> sh) & 0xFFF) as f32 / 4.0;
        let (x1, y1, x0, y0) = (fixed(w0, 12), fixed(w0, 0), fixed(w1, 12), fixed(w1, 0));
        let fill = self.cycle_type() == 3;
        if fill && self.cimg.0 == self.zimg {
            self.frame.commands.push(Command::ClearDepth);
            return;
        }
        let mut state = self.state.clone();
        state.rect = true;
        state.geometry = 0;
        state.textures = [None; 2];
        // Fill and copy modes include the bottom-right edge.
        let extra = if self.cycle_type() >= 2 { 1.0 } else { 0.0 };
        if fill {
            // The fill colour bypasses the combiner: draw it as the primitive
            // colour through a combiner that passes the primitive through.
            state.prim = if self.cimg.2 == 3 {
                rgba(self.fill_color)
            } else {
                rgba5551((self.fill_color >> 16) as u16)
            };
            state.combine = crate::combiner::PRIM_ONLY;
        }
        self.push_rect(
            [x0, y0, x1 + extra, y1 + extra],
            |_, _| [[0.0; 2]; 2],
            state,
        );
    }

    /// `G_BG_COPY` (2D microcode): copies part of a background image in
    /// RAM to the screen one texel per pixel. Only 16-bit RGBA images.
    fn bg_copy(&mut self, rdram: &[u8], w1: u32, at: u32) -> Result<(), GfxError> {
        let a = self.phys(w1);
        let h = |o: u32| rd16(rdram, a + o, at);
        let (image_x, image_w, frame_x, frame_w) = (h(0)?, h(2)?, h(4)? as i16, h(6)?);
        let (image_y, image_h, frame_y, frame_h) = (h(8)?, h(10)?, h(12)? as i16, h(14)?);
        let image = self.phys(rd32(rdram, a + 16, at)?);
        let (fmt, siz) = (rd8(rdram, a + 22, at)?, rd8(rdram, a + 23, at)?);
        if (fmt, siz) != (0, 2) {
            self.warn(format!(
                "background format {fmt}/{siz} not drawn at {at:#010x}"
            ));
            return Ok(());
        }
        let (w, ht) = (u32::from(image_w / 4), u32::from(image_h / 4));
        let mut rgba = Vec::with_capacity((w * ht * 4) as usize);
        for i in 0..w * ht {
            let c = rgba5551(rd16(rdram, image + 2 * i, at)?);
            rgba.extend(c.map(|v| (v * 255.0) as u8));
        }
        let index = self.frame.textures.len() as u32;
        self.frame.textures.push(crate::Texture {
            width: w.max(1),
            height: ht.max(1),
            rgba,
            wrap: [crate::Wrap::Clamp; 2],
        });
        let (x0, y0) = (f32::from(frame_x) / 4.0, f32::from(frame_y) / 4.0);
        let (fw, fh) = (f32::from(frame_w) / 4.0, f32::from(frame_h) / 4.0);
        let (s0, t0) = (f32::from(image_x) / 32.0, f32::from(image_y) / 32.0);
        let uv = [
            [s0 / w as f32, t0 / ht as f32],
            [(s0 + fw) / w as f32, (t0 + fh) / ht as f32],
        ];
        let mut state = self.state.clone();
        state.rect = true;
        state.texel_exact = true;
        state.geometry = 0;
        state.other_h = (state.other_h & !(3 << 20)) | 2 << 20; // copy mode
        state.combine = crate::combiner::TEXEL0_ONLY;
        state.textures = [Some(index), None];
        self.push_rect(
            [x0, y0, x0 + fw, y0 + fh],
            |x, y| [[uv[x][0], uv[y][1]]; 2],
            state,
        );
        Ok(())
    }

    fn texture_rect(&mut self, w0: u32, w1: u32, h1: u32, h2: u32, flip: bool) {
        let fixed = |v: u32, sh: u32| ((v >> sh) & 0xFFF) as f32 / 4.0;
        let (x1, y1, x0, y0) = (fixed(w0, 12), fixed(w0, 0), fixed(w1, 12), fixed(w1, 0));
        let tile = ((w1 >> 24) & 7) as usize;
        let s = (h1 >> 16) as i16 as f32 / 32.0;
        let t = (h1 & 0xFFFF) as i16 as f32 / 32.0;
        let mut dsdx = (h2 >> 16) as i16 as f32 / 1024.0;
        let dtdy = (h2 & 0xFFFF) as i16 as f32 / 1024.0;
        let copy = self.cycle_type() == 2;
        let extra = if copy { 1.0 } else { 0.0 };
        if copy {
            dsdx /= 4.0; // copy mode steps four pixels per clock
        }
        let (x1, y1) = (x1 + extra, y1 + extra);
        let (w, h) = if flip {
            (y1 - y0, x1 - x0)
        } else {
            (x1 - x0, y1 - y0)
        };
        let st = [[s, t], [s + w * dsdx, t + h * dtdy]];
        let mut state = self.state.clone();
        state.rect = true;
        state.texel_exact = copy
            || (dsdx.abs() == 1.0 && dtdy.abs() == 1.0 && s.fract() == 0.0 && t.fract() == 0.0);
        state.geometry = 0;
        let tlut = (self.state.other_h >> 14) & 3;
        let b0 = self.tmem.bind(tile, tlut, &mut self.frame.textures);
        let b1 = self
            .tmem
            .bind((tile + 1) & 7, tlut, &mut self.frame.textures);
        state.textures = [Some(b0.texture), Some(b1.texture)];
        if copy {
            state.combine = crate::combiner::TEXEL0_ONLY;
        }
        let uv = move |x: usize, y: usize| {
            let p = if flip {
                [st[y][0], st[x][1]]
            } else {
                [st[x][0], st[y][1]]
            };
            [b0.uv(p), b1.uv(p)]
        };
        self.push_rect([x0, y0, x1, y1], uv, state);
    }
}

fn normalize(v: [f32; 3]) -> [f32; 3] {
    let l = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
    if l == 0.0 {
        v
    } else {
        v.map(|x| x / l)
    }
}

fn rgba(w: u32) -> [f32; 4] {
    w.to_be_bytes().map(|c| c as f32 / 255.0)
}

pub(crate) fn rgba5551(c: u16) -> [f32; 4] {
    let ch = |sh: u16| ((c >> sh) & 31) as f32 / 31.0;
    [ch(11), ch(6), ch(1), (c & 1) as f32]
}

pub(crate) fn rd8(ram: &[u8], a: u32, at: u32) -> Result<u8, GfxError> {
    ram.get(a as usize)
        .copied()
        .ok_or(GfxError::OutOfRange { addr: a, at })
}

fn rd16(ram: &[u8], a: u32, at: u32) -> Result<u16, GfxError> {
    Ok(u16::from_be_bytes([rd8(ram, a, at)?, rd8(ram, a + 1, at)?]))
}

fn rd32(ram: &[u8], a: u32, at: u32) -> Result<u32, GfxError> {
    Ok(u32::from_be_bytes([
        rd8(ram, a, at)?,
        rd8(ram, a + 1, at)?,
        rd8(ram, a + 2, at)?,
        rd8(ram, a + 3, at)?,
    ]))
}
