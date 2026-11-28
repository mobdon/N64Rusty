//! Display-list interpreter for the F3DEX2 command encoding.
//!
//! Command numbers, field layouts and fixed-point formats follow the public
//! descriptions credited in CREDITS.md ("Graphics" section).

use std::fmt;

use crate::tmem::Tmem;
use crate::{
    image_height, mat_mul, Command, Draw, Frame, Mat4, RenderState, Transform, Vertex, Viewport,
    IDENTITY,
};

/// A display list could not be followed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GfxError {
    /// A read went past the end of guest RAM.
    OutOfRange {
        /// The address read.
        addr: u32,
        /// The command that read it.
        at: u32,
    },
    /// Display-list calls nested deeper than the microcode allows.
    StackOverflow {
        /// The call that went too deep.
        at: u32,
    },
    /// The list did not end within the command budget (a loop).
    TooLong {
        /// Where the list had got to.
        at: u32,
    },
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
/// Bytes per matrix on the RSP stack: G_POPMTX's w1 counts these.
const MTX_BYTES: u32 = 64;
/// Bytes per pixel a [`Readback`] hands back (RGBA8).
const RGBA_BYTES: usize = 4;
const MAX_COMMANDS: usize = 1 << 20;
const MAX_WARNINGS: usize = 64;
/// Largest background side `G_BG_COPY` draws, in texels. The 2D
/// microcode's background holds its sizes as u10.2 (at most 1024 texels); a
/// larger one is a bad pointer or garbage, and would ask for a huge texture.
const MAX_BG_SIZE: u32 = 1024;
/// Most colour images kept ready to read back. Games draw into a few frame
/// buffers and pictures; the oldest goes first past this, so a game that
/// keeps drawing into new addresses does not keep a frame for each.
const MAX_DRAWN: usize = 64;

// Geometry mode bits (F3DEX2).
pub(crate) const G_ZBUFFER: u32 = 0x0001;
pub(crate) const G_CULL_FRONT: u32 = 0x0200;
pub(crate) const G_CULL_BACK: u32 = 0x0400;
/// Shade colours are blended across a triangle (Gouraud), not flat.
pub(crate) const G_SHADING_SMOOTH: u32 = 0x0020_0000;
const G_LIGHTING: u32 = 0x0002_0000;
const G_TEXTURE_GEN: u32 = 0x0004_0000;
const G_TEXTURE_GEN_LINEAR: u32 = 0x0008_0000;
/// Other modes (high word): texture perspective correction on.
const G_TP_PERSP: u32 = 1 << 19;
/// Other modes (high word): the texture filter field (0: point sampled).
pub(crate) const G_MDSFT_TEXTFILT: u32 = 12;

// Cycle types (other modes, high word, bits 20-21).
pub(crate) const G_CYC_2CYCLE: u32 = 1;
pub(crate) const G_CYC_COPY: u32 = 2;
pub(crate) const G_CYC_FILL: u32 = 3;

/// The cycle type in the high other-modes word `other_h`.
pub(crate) fn cycle_type(other_h: u32) -> u32 {
    (other_h >> 20) & 3
}

// Command numbers (the top byte of w0), F3DEX2.
const G_SPNOOP: u32 = 0x00;
const G_VTX: u32 = 0x01;
const G_MODIFYVTX: u32 = 0x02;
const G_CULLDL: u32 = 0x03;
const G_BRANCH_Z: u32 = 0x04;
const G_TRI1: u32 = 0x05;
const G_TRI2: u32 = 0x06;
const G_QUAD: u32 = 0x07;
/// Four triangles in one command (a microcode some games use): the low
/// four bits of the command number are part of the first triangle.
const G_TRI4_FIRST: u32 = 0x10;
const G_TRI4_LAST: u32 = 0x1F;
const G_TEXTURE: u32 = 0xD7;
const G_POPMTX: u32 = 0xD8;
const G_GEOMETRYMODE: u32 = 0xD9;
const G_MTX: u32 = 0xDA;
const G_MOVEWORD: u32 = 0xDB;
const G_MOVEMEM: u32 = 0xDC;
const G_LOAD_UCODE: u32 = 0xDD;
const G_DL: u32 = 0xDE;
const G_ENDDL: u32 = 0xDF;
const G_NOOP: u32 = 0xE0;
const G_RDPHALF_1: u32 = 0xE1;
const G_SETOTHERMODE_L: u32 = 0xE2;
const G_SETOTHERMODE_H: u32 = 0xE3;
const G_TEXRECT: u32 = 0xE4;
const G_TEXRECTFLIP: u32 = 0xE5;
/// The first and last of the four sync commands (load, pipe, tile, full).
const G_RDPLOADSYNC: u32 = 0xE6;
const G_RDPFULLSYNC: u32 = 0xE9;
const G_SETCONVERT: u32 = 0xEC;
const G_SETSCISSOR: u32 = 0xED;
const G_SETPRIMDEPTH: u32 = 0xEE;
const G_RDPSETOTHERMODE: u32 = 0xEF;
const G_LOADTLUT: u32 = 0xF0;
const G_RDPHALF_2: u32 = 0xF1;
const G_SETTILESIZE: u32 = 0xF2;
const G_LOADBLOCK: u32 = 0xF3;
const G_LOADTILE: u32 = 0xF4;
const G_SETTILE: u32 = 0xF5;
const G_FILLRECT: u32 = 0xF6;
const G_SETFILLCOLOR: u32 = 0xF7;
const G_SETFOGCOLOR: u32 = 0xF8;
const G_SETBLENDCOLOR: u32 = 0xF9;
const G_SETPRIMCOLOR: u32 = 0xFA;
const G_SETENVCOLOR: u32 = 0xFB;
const G_SETCOMBINE: u32 = 0xFC;
const G_SETTIMG: u32 = 0xFD;
const G_SETZIMG: u32 = 0xFE;
const G_SETCIMG: u32 = 0xFF;
/// The 2D microcode's own commands: every number up to this one, and the
/// numbers the 3D one uses for `G_MTX` and `G_MOVEMEM`.
const S2D_LAST_LOW: u32 = 0x0B;
/// The 2D microcode's background copy.
const G_BG_COPY: u32 = 0x0A;

// Move-word indices (`G_MOVEWORD`).
const G_MW_MATRIX: u32 = 0x00;
const G_MW_NUMLIGHT: u32 = 0x02;
const G_MW_CLIP: u32 = 0x04;
const G_MW_SEGMENT: u32 = 0x06;
const G_MW_FOG: u32 = 0x08;
const G_MW_LIGHTCOL: u32 = 0x0A;
const G_MW_FORCEMTX: u32 = 0x0C;
const G_MW_PERSPNORM: u32 = 0x0E;
/// The light space of the variant that keeps normals apart.
const G_MW_LIGHT_SPACE: u32 = 0x10;

// Move-memory indices (`G_MOVEMEM`).
const G_MV_VIEWPORT: u32 = 8;
const G_MV_LIGHT: u32 = 10;
/// The normal table of the variant that keeps normals apart.
const G_MV_NORMALS: u32 = 14;

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
    /// A point light of the microcode variant: its position (camera space)
    /// and strength (see [`Interpreter::light_variant`]).
    point: Option<([f32; 3], f32)>,
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
    /// Look-at directions for texture generation (s, t).
    look_at: [[f32; 3]; 2],
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
    /// Running the 3D microcode variant that keeps vertex normals in a
    /// table of their own and lights in 48-byte places (see
    /// [`Interpreter::set_microcode_data`]).
    normals_apart: bool,
    /// The game has started a task on that variant: its other 3D
    /// microcodes are the same variant under the usual name.
    /// shortcut: one game, one 3D layout; tell them apart by their code if
    /// a game mixes the variant with the standard microcode.
    variant_game: bool,
    /// That variant's light space (move-word 0x10): twelve words, the
    /// per-axis scale's whole parts, its fractions and the offsets, each
    /// pair of words twice (see [`Interpreter::light_space_of`]).
    light_space: [u32; 12],
    /// That variant's normal table: two bytes per vertex buffer place.
    normals: u32,
    /// The data of the microcode a task starts on, if the host named it.
    start_data: Option<u32>,
    frame: Frame,
    /// Colour images drawn on the GPU since they were last read back into
    /// RAM, newest last.
    drawn: Vec<Drawn>,
    /// Drop a drawn image's record once it has not been drawn into for this
    /// many handed-over frames (see [`Interpreter::set_readback_frames`]).
    readback_frames: Option<u32>,
    /// The running list reads drawn images back ([`Interpreter::run_readback`]):
    /// only then are draws noted in `drawn` and frames kept for them.
    reading_back: bool,
}

/// A colour image drawn on the GPU and not yet read back.
#[derive(Clone, Debug)]
struct Drawn {
    image: u32,
    width: u32,
    /// Pixel size code (3: 32-bit, 1: 8-bit, else 16-bit).
    siz: u32,
    height: u32,
    /// The handed-over frame that last drew into it, for an image read in
    /// a later frame (a picture drawn once, shown for many frames).
    frame: Option<std::sync::Arc<Frame>>,
    /// Read back into RAM since it was last drawn into.
    read: bool,
    /// Handed-over frames since it was last drawn into.
    age: u32,
}

impl Drawn {
    fn drawn_by(&self, d: &Draw) -> bool {
        draws_into(d, self.image, self.width)
    }
}

/// Whether `d` draws into the colour image at `image`, `width` wide.
fn draws_into(d: &Draw, image: u32, width: u32) -> bool {
    d.state.color_image == image && d.state.color_width == width
}

/// What a later read back of an image `f` drew needs: all of it but the
/// warnings, written over `s` to reuse its buffers. Texture pixels are
/// shared, not copied.
fn snapshot(mut s: Frame, f: &Frame) -> Frame {
    s.width = f.width;
    s.color_image = f.color_image;
    s.transforms.clone_from(&f.transforms);
    s.vertices.clone_from(&f.vertices);
    s.textures.clone_from(&f.textures);
    s.commands.clone_from(&f.commands);
    s.warnings.clear();
    s.hud.clone_from(&f.hud);
    s.image_height = f.image_height;
    s.warnings_dropped = 0;
    s
}

/// Reads `image` (`width` x `height`) back through `rb` from `frame`,
/// lent for the call with only the draws into that image (others may share
/// its address) and the image's size, then put back as it was.
fn read_drawn(
    frame: &mut Frame,
    rb: &mut dyn Readback,
    image: u32,
    width: u32,
    height: u32,
) -> Option<Vec<u8>> {
    let only = (frame.commands.iter())
        .filter(|c| !matches!(c, Command::Draw(x) if !draws_into(x, image, width)))
        .cloned()
        .collect();
    let commands = std::mem::replace(&mut frame.commands, only);
    let size = (frame.width, frame.color_image, frame.image_height);
    (frame.width, frame.color_image, frame.image_height) = (width, image, height);
    let lent = Lent {
        frame,
        commands,
        size,
    };
    rb.read(lent.frame, image, width, height)
}

/// A frame lent for a read back, put back as it was when dropped (also if
/// the read panics: the interpreter keeps going after a panic).
struct Lent<'a> {
    frame: &'a mut Frame,
    commands: Vec<Command>,
    /// Width, colour image and image height.
    size: (u32, u32, u32),
}

impl Drop for Lent<'_> {
    fn drop(&mut self) {
        let f = &mut *self.frame;
        f.commands = std::mem::take(&mut self.commands);
        (f.width, f.color_image, f.image_height) = self.size;
    }
}

/// Draws what was recorded into one colour image and hands back its
/// pixels, for games that read what they drew (a texture loaded from the
/// frame buffer, an image drawn to show inside another).
pub trait Readback {
    /// RGBA8 of `image` (`width` x `height`, top row first), drawn from the
    /// draws in `frame` aimed at it; `None` if it cannot be drawn. The
    /// interpreter hands over `frame` with only those draws, and with its
    /// `width`, `color_image` and `image_height` already naming the image.
    fn read(&mut self, frame: &Frame, image: u32, width: u32, height: u32) -> Option<Vec<u8>>;
}

/// RAM as a display list runs over it: read-only, or writable when
/// drawing results go back into it.
enum Ram<'a> {
    Shared(&'a [u8]),
    Mut(&'a mut [u8]),
}

impl Ram<'_> {
    fn r(&self) -> &[u8] {
        match self {
            Ram::Shared(b) => b,
            Ram::Mut(b) => b,
        }
    }

    fn w(&mut self) -> Option<&mut [u8]> {
        match self {
            Ram::Shared(_) => None,
            Ram::Mut(b) => Some(b),
        }
    }
}

impl Default for Interpreter {
    fn default() -> Self {
        Self::new()
    }
}

impl Interpreter {
    /// An interpreter in the state a game starts in: no segments, an
    /// identity matrix stack and the usual 3D microcode.
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
            look_at: [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0]],
            tex_scale: [1.0; 2],
            tex_tile: 0,
            tex_on: false,
            state: RenderState {
                combine: 0,
                // Perspective correction on until a list says otherwise, as
                // games set it.
                other_h: G_TP_PERSP,
                other_l: 0,
                geometry: 0,
                prim: [1.0; 4],
                prim_lod: 0.0,
                k5: 0.0,
                env: [1.0; 4],
                blend: [0.0; 4],
                fog: [0.0; 4],
                fog_factor: [0.0; 2],
                textures: [None; 2],
                rect: false,
                texel_exact: false,
                picture: false,
                color_image: 0,
                color_width: 320,
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
            normals_apart: false,
            variant_game: false,
            normals: 0,
            light_space: [0; 12],
            start_data: None,
            frame: Frame {
                width: 320,
                ..Frame::default()
            },
            drawn: Vec::new(),
            readback_frames: None,
            reading_back: false,
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
        let f = std::mem::replace(&mut self.frame, next);
        let by_f: Vec<bool> = (self.drawn.iter())
            .map(|d| f.draws().any(|x| d.drawn_by(x)))
            .collect();
        // The snapshots replaced lend their buffers to the new one, if no
        // other image still holds them.
        let replaced: Vec<_> = (self.drawn.iter_mut().zip(&by_f))
            .filter(|(_, by_f)| **by_f)
            .filter_map(|(d, _)| d.frame.take())
            .collect();
        let spare = replaced
            .into_iter()
            .find_map(|s| std::sync::Arc::try_unwrap(s).ok());
        let snap = by_f
            .contains(&true)
            .then(|| std::sync::Arc::new(snapshot(spare.unwrap_or_default(), &f)));
        for (d, by_f) in self.drawn.iter_mut().zip(by_f) {
            if by_f {
                d.frame = snap.clone();
                d.age = 0;
            } else {
                d.age += 1;
            }
        }
        if let Some(n) = self.readback_frames {
            self.drawn.retain(|d| d.age <= n);
        }
        f
    }

    /// Forget a drawn image once it has not been drawn into for `n` frames,
    /// so a texture load from memory the game has since reused for
    /// something else does not get the old picture written over it. Without
    /// this a picture drawn once stays ready to read back for good.
    pub fn set_readback_frames(&mut self, n: Option<u32>) {
        self.readback_frames = n;
    }

    /// The frame being recorded.
    pub fn frame(&self) -> &Frame {
        &self.frame
    }

    /// Names the data of the microcode the coming tasks start on (the
    /// task's own pointer to it), so the command set follows the name in
    /// it. Without this every task starts on the usual 3D set.
    pub fn set_microcode_data(&mut self, data: Option<u32>) {
        self.start_data = data;
    }

    /// Follow the display list at `addr` (a segmented or physical address)
    /// in `rdram`, the big-endian guest RAM.
    pub fn run(&mut self, rdram: &[u8], addr: u32) -> Result<(), GfxError> {
        self.exec(Ram::Shared(rdram), addr, None)
    }

    /// Like [`Interpreter::run`], and what the RDP would leave in RAM is
    /// put there: fill and copy rectangles are done in place, and an image
    /// drawn on the GPU is read back through `rb` before a texture load
    /// reads it.
    pub fn run_readback(
        &mut self,
        rdram: &mut [u8],
        addr: u32,
        rb: &mut dyn Readback,
    ) -> Result<(), GfxError> {
        self.exec(Ram::Mut(rdram), addr, Some(rb))
    }

    fn exec(
        &mut self,
        mut ram: Ram,
        addr: u32,
        mut rb: Option<&mut dyn Readback>,
    ) -> Result<(), GfxError> {
        let mut pc = self.phys(addr);
        let mut stack: Vec<u32> = Vec::new();
        self.reading_back = rb.is_some();
        // Every task starts on its own (3D) microcode.
        self.ucode_2d = false;
        self.normals_apart = false;
        if let Some(data) = self.start_data {
            self.microcode(ram.r(), data);
        }
        for _ in 0..MAX_COMMANDS {
            let w0 = rd32(ram.r(), pc, pc)?;
            let w1 = rd32(ram.r(), pc + 4, pc)?;
            let at = pc;
            pc += 8;
            if self.ucode_2d && (w0 >> 24 <= S2D_LAST_LOW || matches!(w0 >> 24, G_MTX | G_MOVEMEM))
            {
                match w0 >> 24 {
                    G_BG_COPY => self.bg_copy(ram.r(), w1, at)?,
                    op => self.warn(format!("unknown 2D command {op:#04x} at {at:#010x}")),
                }
                continue;
            }
            match w0 >> 24 {
                G_LOAD_UCODE => self.load_ucode(ram.r()),
                G_VTX => self.vertices(ram.r(), w0, w1, at)?,
                G_MODIFYVTX => self.modify_vertex(w0, w1, at),
                G_SETSCISSOR => self.state.scissor = Some(rect_coords(w0, w1)),
                G_TRI1 => self.tri(tri_indices(w0), at),
                G_TRI2 | G_QUAD => {
                    self.tri(tri_indices(w0), at);
                    self.tri(tri_indices(w1), at);
                }
                // Four triangles in one command (a microcode some games
                // use), each corner a 5-bit vertex number: the opcode's low
                // four bits and the first word hold the first two
                // triangles, with the third corner of the first split
                // across both words. A triangle with one vertex three times
                // fills an unused place.
                G_TRI4_FIRST..=G_TRI4_LAST => {
                    let f = |v: u32, s: u32| (v >> s) & 31;
                    let tris = [
                        [f(w0, 23), f(w0, 18), ((w0 >> 15) & 7) << 2 | w1 >> 30],
                        [f(w0, 10), f(w0, 5), f(w0, 0)],
                        [f(w1, 25), f(w1, 20), f(w1, 15)],
                        [f(w1, 10), f(w1, 5), f(w1, 0)],
                    ];
                    for t in tris {
                        if t[0] != t[1] || t[1] != t[2] {
                            self.tri(t.map(|i| i * 2), at);
                        }
                    }
                }
                // G_SPNOOP, G_CULLDL (drawing everything is always safe),
                // syncs, scissor and the coprocessor-only commands.
                G_SPNOOP | G_CULLDL | G_NOOP | G_RDPLOADSYNC..=G_RDPFULLSYNC | G_RDPHALF_2 => {}
                // SETCONVERT: only K5 (a signed 9-bit value) is a colour
                // combiner input; the rest convert YUV textures.
                G_SETCONVERT => self.state.k5 = ((w1 << 23) as i32 >> 23) as f32 / 255.0,
                G_BRANCH_Z => {
                    // shortcut: G_BRANCH_Z always takes the branch (the
                    // nearer, more detailed level); compare depth when a
                    // game shows the wrong level of detail.
                    pc = self.phys(self.rdphalf1);
                }
                G_TEXTURE => self.texture(w0, w1),
                G_POPMTX => self.pop_matrix(w1),
                G_GEOMETRYMODE => self.geometry = (self.geometry & (w0 & 0x00FF_FFFF)) | w1,
                G_MTX => self.matrix(ram.r(), w0, w1, at)?,
                G_MOVEWORD => self.moveword(w0, w1, at),
                G_MOVEMEM => self.movemem(ram.r(), w0, w1, at)?,
                G_DL => {
                    if (w0 >> 16) & 0xFF == 0 {
                        if stack.len() >= DL_STACK {
                            return Err(GfxError::StackOverflow { at });
                        }
                        stack.push(pc);
                    }
                    pc = self.phys(w1);
                }
                G_ENDDL => match stack.pop() {
                    Some(ret) => pc = ret,
                    None => return Ok(()),
                },
                G_RDPHALF_1 => self.rdphalf1 = w1,
                G_SETOTHERMODE_L | G_SETOTHERMODE_H => self.set_other_mode(w0, w1),
                G_TEXRECT | G_TEXRECTFLIP => {
                    let h1 = rd32(ram.r(), pc + 4, at)?;
                    let h2 = rd32(ram.r(), pc + 12, at)?;
                    pc += 16;
                    self.texture_rect(w0, w1, h1, h2, w0 >> 24 == G_TEXRECTFLIP, ram.w());
                }
                G_SETPRIMDEPTH => self.prim_depth = ((w1 >> 16) & 0x7FFF) as f32 / 32767.0, // 15-bit depth
                G_RDPSETOTHERMODE => {
                    self.state.other_h = w0 & 0x00FF_FFFF;
                    self.state.other_l = w1;
                }
                G_LOADTLUT | G_LOADBLOCK | G_LOADTILE if rb.is_some() => {
                    if let (Some(rb), Some(m)) = (rb.as_deref_mut(), ram.w()) {
                        self.read_back(m, rb);
                    }
                    match w0 >> 24 {
                        G_LOADTLUT => self.tmem.load_tlut(ram.r(), w1),
                        G_LOADBLOCK => self.tmem.load_block(ram.r(), w0, w1),
                        _ => self.tmem.load_tile(ram.r(), w0, w1),
                    }
                }
                G_LOADTLUT => self.tmem.load_tlut(ram.r(), w1),
                G_SETTILESIZE => self.tmem.set_tile_size(w0, w1),
                G_LOADBLOCK => self.tmem.load_block(ram.r(), w0, w1),
                G_LOADTILE => self.tmem.load_tile(ram.r(), w0, w1),
                G_SETTILE => self.tmem.set_tile(w0, w1),
                G_FILLRECT => self.fill_rect(w0, w1, ram.w()),
                G_SETFILLCOLOR => self.fill_color = w1,
                G_SETFOGCOLOR => self.state.fog = rgba(w1),
                G_SETBLENDCOLOR => self.state.blend = rgba(w1),
                G_SETPRIMCOLOR => {
                    self.state.prim = rgba(w1);
                    self.state.prim_lod = (w0 & 0xFF) as f32 / 255.0;
                }
                G_SETENVCOLOR => self.state.env = rgba(w1),
                G_SETCOMBINE => self.state.combine = ((w0 as u64 & 0x00FF_FFFF) << 32) | w1 as u64,
                G_SETTIMG => self.tmem.set_image(w0, self.phys(w1)),
                G_SETZIMG => self.zimg = self.phys(w1),
                G_SETCIMG => self.color_image(w0, w1),
                op => self.warn(format!("unknown command {op:#04x} at {at:#010x}")),
            }
        }
        Err(GfxError::TooLong { at: pc })
    }

    /// `G_LOAD_UCODE`: a load names the new microcode's data in the half
    /// word before it. shortcut: data with no name in it toggles between 3D
    /// and 2D, as games with those two do.
    fn load_ucode(&mut self, rdram: &[u8]) {
        let data = self.phys(self.rdphalf1);
        if !self.microcode(rdram, data) {
            self.ucode_2d = !self.ucode_2d;
        }
    }

    /// `G_TEXTURE`: the texture coordinate scale, the tile and on or off.
    fn texture(&mut self, w0: u32, w1: u32) {
        // 0xFFFF is the conventional "1.0".
        let scale = |v: u32| if v == 0xFFFF { 1.0 } else { v as f32 / 65536.0 };
        self.tex_scale = [scale(w1 >> 16), scale(w1 & 0xFFFF)];
        self.tex_tile = ((w0 >> 8) & 7) as usize;
        self.tex_on = w0 & 0xFE != 0;
    }

    /// `G_POPMTX`: pop `w1` bytes' worth of model-view matrices.
    fn pop_matrix(&mut self, w1: u32) {
        let n = (w1 / MTX_BYTES) as usize;
        // F3DEX2 skips a pop below the stack base (bltz); a zero-byte pop
        // removes nothing but still reloads the modelview like any pop.
        if n < self.modelview.len() {
            self.modelview.truncate(self.modelview.len() - n);
            // F3DEX2 reloads the modelview and marks the combined matrix stale (GLideN64: CHANGED_MATRIX).
            self.mvp = None;
        }
    }

    /// `G_SETOTHERMODE_L` and `_H`: replace a run of bits of one word.
    fn set_other_mode(&mut self, w0: u32, w1: u32) {
        // A malformed length past 32 bits would overflow the shift.
        let len = ((w0 & 0xFF) + 1).min(32);
        let shift = 32u32.saturating_sub(((w0 >> 8) & 0xFF) + len);
        let mask = (((1u64 << len) - 1) << shift) as u32;
        let m = if w0 >> 24 == G_SETOTHERMODE_L {
            &mut self.state.other_l
        } else {
            &mut self.state.other_h
        };
        *m = (*m & !mask) | (w1 & mask);
    }

    /// `G_SETCIMG`: the colour image drawn into from now on.
    fn color_image(&mut self, w0: u32, w1: u32) {
        self.cimg = (self.phys(w1), (w0 & 0xFFF) + 1, (w0 >> 19) & 3);
        self.state.color_image = self.cimg.0;
        self.state.color_width = self.cimg.1;
        self.frame.color_image = self.cimg.0;
        if self.frame.width != self.cimg.1 {
            self.frame.width = self.cimg.1;
            self.rect_transform = None;
        }
    }

    fn warn(&mut self, msg: String) {
        if self.frame.warnings.len() < MAX_WARNINGS {
            self.frame.warnings.push(msg);
        } else {
            self.frame.warnings_dropped += 1;
        }
    }

    /// Picks the command set from the name in the microcode data at `data`
    /// (the text every microcode carries near its start); returns whether
    /// one was found.
    fn microcode(&mut self, rdram: &[u8], data: u32) -> bool {
        let at = data as usize & 0x00FF_FFFF;
        let Some(d) = rdram.get(at..(at + 0x800).min(rdram.len())) else {
            return false;
        };
        // The first name only: the next microcode's data, with its own
        // name, often follows in RAM (a 3D one before the 2D one).
        let tag = b"RSP Gfx ucode ";
        let Some(i) = d.windows(tag.len()).position(|w| w == tag) else {
            return false;
        };
        let name = &d[i + tag.len()..];
        self.ucode_2d = name.starts_with(b"S2DEX");
        self.variant_game |= name.starts_with(b"F3DEXBG");
        self.normals_apart = self.variant_game && !self.ucode_2d;
        true
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
            } else if push {
                self.warn(format!(
                    "matrix push at {at:#010x} dropped: the stack holds {MTX_STACK}; the top is replaced"
                ));
            }
            *self.modelview.last_mut().unwrap() = if load { m } else { mat_mul(&m, &top) };
        }
        Ok(())
    }

    fn moveword(&mut self, w0: u32, w1: u32, at: u32) {
        let (index, offset) = ((w0 >> 16) & 0xFF, w0 & 0xFFFF);
        match index {
            G_MW_NUMLIGHT => self.num_lights = (w1 / self.light_size()).min(7) as usize,
            G_MW_SEGMENT => self.segments[(offset / 4) as usize & 0xF] = w1 & 0x00FF_FFFF,
            G_MW_LIGHT_SPACE if self.normals_apart => {
                self.light_space[(offset / 4) as usize % 12] = w1
            }
            // Fog: multiplier and offset of the fog factor.
            G_MW_FOG => {
                self.state.fog_factor = [(w1 >> 16) as i16 as f32, w1 as i16 as f32];
            }
            // Clip ratio, perspective norm: not needed by this renderer.
            // Light colour (`gSPLightColor`): light n's two colour words
            // are at n * 24 and n * 24 + 4; the second is a copy.
            G_MW_LIGHTCOL => {
                if offset % 24 == 0 {
                    if let Some(l) = self.lights.get_mut((offset / 24) as usize) {
                        l.color = [24, 16, 8].map(|s| ((w1 >> s) & 0xFF) as f32 / 255.0);
                    }
                }
            }
            // gSPInsertMatrix: overwrite two 16-bit halves (integer parts in
            // the first 32 bytes, fractions in the next 32) of the combined
            // modelview-projection matrix. Games build billboards this way.
            G_MW_MATRIX if offset < 64 => {
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
            G_MW_CLIP | G_MW_FORCEMTX | G_MW_PERSPNORM => {}
            _ => self.warn(format!("moveword {index:#x} at {at:#010x}")),
        }
    }

    fn movemem(&mut self, rdram: &[u8], w0: u32, w1: u32, at: u32) -> Result<(), GfxError> {
        let a = self.phys(w1);
        let offset = ((w0 >> 8) & 0xFF) * 8;
        match w0 & 0xFF {
            G_MV_VIEWPORT => {
                let v = |k: u32| -> Result<f32, GfxError> {
                    Ok(rd16(rdram, a + k * 2, at)? as i16 as f32 / 4.0)
                };
                self.viewport = Viewport {
                    scale: [v(0)?, v(1)?],
                    trans: [v(4)?, v(5)?],
                };
            }
            G_MV_LIGHT => {
                // Lights start after the two look-at entries.
                let b = |k: u32| rd8(rdram, a + k, at);
                let place = offset / self.light_size();
                let Some(n) = place.checked_sub(2) else {
                    let dir = [b(8)? as i8 as f32, b(9)? as i8 as f32, b(10)? as i8 as f32];
                    self.look_at[place as usize] = normalize(dir);
                    return Ok(());
                };
                // The variant's 48-byte places: a strength byte at 12 (whole
                // and sixteenths in its two nibbles; 0: not a point light)
                // and the position at 32.
                let point = if self.normals_apart && b(12)? != 0 {
                    let s = b(12)?;
                    let h = |k: u32| -> Result<f32, GfxError> {
                        Ok(rd16(rdram, a + 32 + 2 * k, at)? as i16 as f32)
                    };
                    let strength = f32::from(s >> 4) + f32::from(s & 15) / 16.0;
                    Some(([h(0)?, h(1)?, h(2)?], strength))
                } else {
                    None
                };
                let light = Light {
                    color: [
                        b(0)? as f32 / 255.0,
                        b(1)? as f32 / 255.0,
                        b(2)? as f32 / 255.0,
                    ],
                    dir: normalize([b(8)? as i8 as f32, b(9)? as i8 as f32, b(10)? as i8 as f32]),
                    point,
                };
                if let Some(l) = self.lights.get_mut(n as usize) {
                    *l = light;
                }
            }
            G_MV_NORMALS if self.normals_apart => self.normals = a,
            idx => self.warn(format!("movemem {idx:#x} at {at:#010x}")),
        }
        Ok(())
    }

    /// Bytes per place in the light table.
    fn light_size(&self) -> u32 {
        if self.normals_apart {
            48
        } else {
            24
        }
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
        let texgen = lit && self.geometry & G_TEXTURE_GEN != 0;
        for i in 0..n {
            let v = a + i as u32 * 16;
            let h = |k: u32| -> Result<i16, GfxError> { Ok(rd16(rdram, v + k, at)? as i16) };
            let b = |k: u32| rd8(rdram, v + k, at);
            let apart = lit && self.normals_apart;
            let normal = if apart {
                // Two bytes in the table at the vertex's buffer place, the
                // third in the vertex; its colour bytes stay a colour.
                let t = self.normals + (end - n + i) as u32 * 2;
                [
                    rd8(rdram, t, at)? as i8 as f32,
                    rd8(rdram, t + 1, at)? as i8 as f32,
                    b(7)? as i8 as f32,
                ]
            } else {
                [
                    b(12)? as i8 as f32,
                    b(13)? as i8 as f32,
                    b(14)? as i8 as f32,
                ]
            };
            let color = if apart {
                let pos = [h(0)? as f32, h(2)? as f32, h(4)? as f32];
                let lc = self.light_variant(self.light_space_of(pos, &mv, &projection));
                let c = |k: usize| -> Result<f32, GfxError> {
                    Ok(lc[k] * b(12 + k as u32)? as f32 / 255.0)
                };
                [c(0)?, c(1)?, c(2)?, b(15)? as f32 / 255.0]
            } else if lit {
                let lc = self.light(&lights_mv, normal);
                [lc[0], lc[1], lc[2], b(15)? as f32 / 255.0]
            } else {
                [b(12)?, b(13)?, b(14)?, b(15)?].map(|c| c as f32 / 255.0)
            };
            let mut st = [
                h(8)? as f32 / 32.0 * self.tex_scale[0],
                h(10)? as f32 / 32.0 * self.tex_scale[1],
            ];
            if texgen {
                // Reflection mapping: the coordinates come from the normal
                // seen along the look-at directions, over the texture scale
                // (a scale of 0x07C0 spans a 32-texel texture).
                let e = eye_normal(&lights_mv, normal);
                for k in 0..2 {
                    let d = (0..3).map(|j| e[j] * self.look_at[k][j]).sum::<f32>();
                    let u = if self.geometry & G_TEXTURE_GEN_LINEAR != 0 {
                        (-d).clamp(-1.0, 1.0).acos() / std::f32::consts::PI
                    } else {
                        d * 0.5 + 0.5
                    };
                    st[k] = u * self.tex_scale[k] * 1024.0;
                }
            }
            self.vtx[end - n + i] = Loaded {
                pos: [h(0)? as f32, h(2)? as f32, h(4)? as f32],
                transform,
                color,
                st,
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

    /// Lighting of the microcode variant, as its vertex code works it out:
    /// ambient plus, for each point light, its colour times `min(1, 32768 *
    /// strength / d^2)`, `d` the distance from the vertex in light space.
    /// It has no directional term: a light with no strength adds nothing,
    /// and games place a light near each model to shade it by direction.
    fn light_variant(&self, at: [f32; 3]) -> [f32; 3] {
        let mut c = self.lights[self.num_lights].color;
        for l in &self.lights[..self.num_lights] {
            let Some((p, strength)) = l.point else {
                continue;
            };
            let d2: f32 = (0..3).map(|j| (at[j] - p[j]).powi(2)).sum();
            let k = (32768.0 * strength / d2.max(1.0)).min(1.0);
            for (c, lc) in c.iter_mut().zip(l.color) {
                *c += lc * k;
            }
        }
        c.map(|x| x.min(1.0))
    }

    /// A vertex's position in the variant's light space: its clip position
    /// plus an offset, times a scale, per axis (the scale's whole and
    /// fraction parts and the offset set with move-word 0x10). The game sets
    /// these to undo its projection; before it does, camera space.
    fn light_space_of(&self, pos: [f32; 3], mv: &Mat4, projection: &Mat4) -> [f32; 3] {
        let eye: [f32; 4] =
            [0, 1, 2, 3].map(|j| (0..3).map(|k| pos[k] * mv[k][j]).sum::<f32>() + mv[3][j]);
        if self.light_space == [0; 12] {
            return [eye[0], eye[1], eye[2]];
        }
        let clip: [f32; 4] = [0, 1, 2, 3].map(|j| (0..4).map(|k| eye[k] * projection[k][j]).sum());
        let lane = |word: usize, i: usize| {
            let w = self.light_space[word + i / 2];
            (if i.is_multiple_of(2) { w >> 16 } else { w & 0xFFFF }) as u16
        };
        [0, 1, 2].map(|i| {
            let scale = f32::from(lane(0, i) as i16) + f32::from(lane(4, i)) / 65536.0;
            (clip[i] + f32::from(lane(8, i) as i16)) * scale
        })
    }

    /// Directional lights plus ambient (the light after the last one).
    fn light(&self, mv: &Mat4, n: [f32; 3]) -> [f32; 3] {
        let n = eye_normal(mv, n);
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
        let bilerp = (self.state.other_h >> G_MDSFT_TEXTFILT) & 3 != 0;
        // Perspective correction off: the console takes the coordinates at
        // half scale (games' billboards such as the sun are made for it).
        let half = if self.state.other_h & G_TP_PERSP == 0 { 0.5 } else { 1.0 };
        let verts = idx.map(|i| {
            let v = self.vtx[i];
            let st = v.st.map(|c| c * half);
            Vertex {
                pos: v.pos,
                transform: v.transform,
                color: v.color,
                uv: tiles.map(|t| t.map_or([0.0; 2], |t| t.uv(st, bilerp))),
            }
        });
        let mut state = self.state.clone();
        state.geometry = self.geometry;
        state.textures = tiles.map(|t| t.map(|t| t.texture));
        self.push(state, &verts);
    }

    /// Decode the draw's tiles, if texturing is on: the second only when
    /// the combiner reads it.
    fn tiles(&mut self) -> [Option<crate::tmem::Bound>; 2] {
        if !self.tex_on {
            return [None; 2];
        }
        let tlut = (self.state.other_h >> 14) & 3;
        let second = self.reads_texel1();
        [0, 1].map(|k| {
            let tile = (self.tex_tile + k) & 7;
            (k == 0 || second).then(|| self.tmem.bind(tile, tlut, &mut self.frame.textures))
        })
    }

    /// Whether the combiner reads texel 1 (the tile after the draw's), in
    /// the cycles that run.
    fn reads_texel1(&self) -> bool {
        crate::combiner::Combiner::from_raw(self.state.combine)
            .reads_texel1(self.cycle_type() == G_CYC_2CYCLE)
    }

    fn push(&mut self, state: RenderState, verts: &[Vertex]) {
        if self.reading_back {
            self.note_drawn();
        }
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

    /// Notes that the colour image is drawn into, for a later read back.
    fn note_drawn(&mut self) {
        // The image's height is the scissor's bottom (the game draws inside
        // it), else a guess from its width.
        let (image, width, siz) = self.cimg;
        let h = self
            .state
            .scissor
            .map_or(image_height(width), |s| s[3].ceil().max(1.0) as u32);
        match self
            .drawn
            .iter()
            .position(|d| d.image == image && d.width == width)
        {
            Some(k) => {
                let mut d = self.drawn.remove(k);
                d.height = d.height.max(h);
                // A frame buffer is also drawn over as an 8-bit image for a
                // pass of its own; it is read back at its full pixel size.
                d.siz = d.siz.max(siz);
                d.read = false;
                self.drawn.push(d);
            }
            None => {
                if self.drawn.len() >= MAX_DRAWN {
                    let forgotten = self.drawn.remove(0);
                    self.warn(format!(
                        "drawn image {:#010x} forgotten: more than {MAX_DRAWN} drawn images",
                        forgotten.image
                    ));
                }
                self.drawn.push(Drawn {
                    image,
                    width,
                    siz,
                    height: h,
                    frame: None,
                    read: false,
                    age: 0,
                })
            }
        }
    }

    /// Reads back a colour image drawn on the GPU that the coming texture
    /// load starts inside.
    fn read_back(&mut self, ram: &mut [u8], rb: &mut dyn Readback) {
        let (src, src_width, src_siz) = self.tmem.image();
        // A load of another pixel size is a texture that only lies inside
        // the image's guessed extent (its height is a guess from its width),
        // not the picture read back.
        let inside = |d: &Drawn| {
            let len = d.width * d.height * pixel_bytes(d.siz);
            src >= d.image && src < d.image + len && src_siz == d.siz
        };
        // Images of other widths can share the address: the load's own row
        // width names the one it reads.
        let Some(k) = (self.drawn.iter())
            .rposition(|d| inside(d) && d.width == src_width)
            .or_else(|| self.drawn.iter().rposition(inside))
        else {
            return;
        };
        // Kept once read: the loads after it (one strip of the image each)
        // must not fall through to another image at the same address.
        if self.drawn[k].read {
            return;
        }
        self.drawn[k].read = true;
        let (a, width, height, siz) = {
            let d = &self.drawn[k];
            (d.image, d.width, d.height, d.siz)
        };
        let here = self.frame.draws().any(|x| draws_into(x, a, width));
        // Lent, not copied: the frame being recorded, or the handed-over
        // one (copied only if other images share it).
        let from = if here {
            &mut self.frame
        } else {
            match self.drawn[k].frame.as_mut() {
                Some(f) => std::sync::Arc::make_mut(f),
                None => return,
            }
        };
        let Some(px) = read_drawn(from, rb, a, width, height) else {
            return;
        };
        let want = width as usize * height as usize * RGBA_BYTES;
        if px.len() != want {
            self.warn(format!(
                "drawn image {a:#010x} not read back: {} bytes for {width} x {height}, not {want}",
                px.len(),
            ));
            return;
        }
        put_pixels(ram, a, siz, &px);
    }

    /// Pixels (`x0..x1`, `y0..y1`) of the colour image inside the scissor.
    fn cpu_area(&self, x0: f32, y0: f32, x1: f32, y1: f32) -> (u32, u32, u32, u32) {
        let [sx0, sy0, sx1, sy1] = self.state.scissor.unwrap_or([0.0, 0.0, f32::MAX, f32::MAX]);
        let w = self.cimg.1 as f32;
        (
            x0.max(sx0).max(0.0) as u32,
            y0.max(sy0).max(0.0) as u32,
            x1.min(sx1).min(w).max(0.0) as u32,
            y1.min(sy1).max(0.0) as u32,
        )
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
        cycle_type(self.state.other_h)
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
            // The RDP gives rectangles no shade: the input reads zero.
            color: [0.0; 4],
            uv: uv(x, y),
        };
        let (a, b, c, d) = (corner(0, 0), corner(1, 0), corner(0, 1), corner(1, 1));
        self.push(state, &[a, c, b, b, c, d]);
    }

    fn fill_rect(&mut self, w0: u32, w1: u32, ram: Option<&mut [u8]>) {
        let [x1, y1, x0, y0] = rect_coords(w0, w1);
        let fill = self.cycle_type() == G_CYC_FILL;
        if let (true, Some(ram)) = (fill, ram) {
            // The fill colour, as the RDP writes it: two 16-bit pixels or
            // one 32-bit pixel per word.
            let (ax, ay, bx, by) = self.cpu_area(x0, y0, x1 + 1.0, y1 + 1.0);
            let (a, w, siz) = self.cimg;
            for y in ay..by {
                for x in ax..bx {
                    let i = y * w + x;
                    if siz == 3 {
                        put_raw(ram, a + 4 * i, &self.fill_color.to_be_bytes());
                    } else if siz == 1 {
                        // Four 8-bit pixels per word.
                        put_raw(ram, a + i, &[self.fill_color.to_be_bytes()[x as usize % 4]]);
                    } else {
                        let c = if x % 2 == 0 {
                            self.fill_color >> 16
                        } else {
                            self.fill_color
                        };
                        put_raw(ram, a + 2 * i, &(c as u16).to_be_bytes());
                    }
                }
            }
        }
        if fill && self.cimg.0 == self.zimg {
            self.frame.commands.push(Command::ClearDepth);
            return;
        }
        let mut state = self.state.clone();
        state.rect = true;
        state.geometry = 0;
        state.textures = [None; 2];
        // Fill and copy modes include the bottom-right edge.
        let extra = if self.cycle_type() >= G_CYC_COPY { 1.0 } else { 0.0 };
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
        let fits = |side: u32| (1..=MAX_BG_SIZE).contains(&side);
        if !fits(w) || !fits(ht) {
            self.warn(format!(
                "background {w}x{ht} not drawn at {at:#010x}: sides must be 1 to {MAX_BG_SIZE}"
            ));
            return Ok(());
        }
        let mut rgba = Vec::with_capacity((w * ht * 4) as usize);
        for i in 0..w * ht {
            let c = rgba5551(rd16(rdram, image + 2 * i, at)?);
            rgba.extend(c.map(|v| (v * 255.0) as u8));
        }
        let index = self.frame.textures.len() as u32;
        self.frame.textures.push(crate::Texture {
            width: w.max(1),
            height: ht.max(1),
            rgba: rgba.into(),
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
        state.picture = true;
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

    fn texture_rect(
        &mut self,
        w0: u32,
        w1: u32,
        h1: u32,
        h2: u32,
        flip: bool,
        ram: Option<&mut [u8]>,
    ) {
        let [x1, y1, x0, y0] = rect_coords(w0, w1);
        let tile = ((w1 >> 24) & 7) as usize;
        let s = (h1 >> 16) as i16 as f32 / 32.0;
        let t = (h1 & 0xFFFF) as i16 as f32 / 32.0;
        let mut dsdx = (h2 >> 16) as i16 as f32 / 1024.0;
        let dtdy = (h2 & 0xFFFF) as i16 as f32 / 1024.0;
        let copy = self.cycle_type() == G_CYC_COPY;
        let extra = if copy { 1.0 } else { 0.0 };
        if copy {
            dsdx /= 4.0; // copy mode steps four pixels per clock
        }
        let (x1, y1) = (x1 + extra, y1 + extra);
        if let (true, false, Some(ram)) = (copy, flip, ram) {
            // Copy mode moves texels straight into the colour image.
            let tlut = (self.state.other_h >> 14) & 3;
            let alpha_test = self.state.other_l & 3 != 0;
            let (ax, ay, bx, by) = self.cpu_area(x0, y0, x1, y1);
            let (a, w, siz) = self.cimg;
            for y in ay..by {
                for x in ax..bx {
                    let ss = s + (x as f32 - x0) * dsdx;
                    let tt = t + (y as f32 - y0) * dtdy;
                    let c = self.tmem.texel_at(tile, ss, tt, tlut);
                    if !(alpha_test && c[3] == 0) {
                        put_pixel(ram, a, siz, y * w + x, c);
                    }
                }
            }
        }
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
        if copy {
            state.combine = crate::combiner::TEXEL0_ONLY;
        }
        let b0 = self.tmem.bind(tile, tlut, &mut self.frame.textures);
        // Copy mode reads only texel 0.
        let b1 = (!copy && self.reads_texel1()).then(|| {
            self.tmem
                .bind((tile + 1) & 7, tlut, &mut self.frame.textures)
        });
        state.textures = [Some(b0.texture), b1.map(|b| b.texture)];
        let uv = move |x: usize, y: usize| {
            let p = if flip {
                [st[y][0], st[x][1]]
            } else {
                [st[x][0], st[y][1]]
            };
            [b0.uv(p, false), b1.map_or([0.0; 2], |b| b.uv(p, false))]
        };
        self.push_rect([x0, y0, x1, y1], uv, state);
    }
}

/// A vertex normal turned by the modelview matrix, unit length.
fn eye_normal(mv: &Mat4, n: [f32; 3]) -> [f32; 3] {
    normalize([0, 1, 2].map(|j| (0..3).map(|k| n[k] * mv[k][j]).sum::<f32>()))
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

/// The four 10.2 fixed-point coordinates of a scissor or rectangle
/// command, in the order the words hold them: `[w0 >> 12, w0, w1 >> 12, w1]`.
fn rect_coords(w0: u32, w1: u32) -> [f32; 4] {
    let fixed = |v: u32, sh: u32| ((v >> sh) & 0xFFF) as f32 / 4.0;
    [fixed(w0, 12), fixed(w0, 0), fixed(w1, 12), fixed(w1, 0)]
}

/// The three vertex-buffer indices of a triangle in the low 24 bits of `w`.
fn tri_indices(w: u32) -> [u32; 3] {
    [(w >> 16) & 0xFF, (w >> 8) & 0xFF, w & 0xFF]
}

/// Writes `bytes` at physical `at`, if it is in RAM.
fn put_raw(ram: &mut [u8], at: u32, bytes: &[u8]) {
    if let Some(d) = ram.get_mut(at as usize..at as usize + bytes.len()) {
        d.copy_from_slice(bytes);
    }
}

/// Bytes per pixel of a colour image with pixel size code `siz`.
fn pixel_bytes(siz: u32) -> u32 {
    match siz {
        3 => 4,
        1 => 1,
        _ => 2,
    }
}

/// Writes RGBA8 `c` as pixel `i` of the colour image at `image` with pixel
/// size code `siz` (3: 32-bit, 1: 8-bit, which keeps the red channel, else
/// 16-bit RGBA5551).
fn put_pixel(ram: &mut [u8], image: u32, siz: u32, i: u32, c: [u8; 4]) {
    if siz == 3 {
        put_raw(ram, image + 4 * i, &c);
    } else if siz == 1 {
        put_raw(ram, image + i, &c[..1]);
    } else {
        put_raw(ram, image + 2 * i, &to_rgba5551(c).to_be_bytes());
    }
}

/// RGBA8 `c` as an RGBA5551 pixel (alpha set from 128 up).
fn to_rgba5551(c: [u8; 4]) -> u16 {
    let ch = |v: u8| u16::from(v >> 3);
    ch(c[0]) << 11 | ch(c[1]) << 6 | ch(c[2]) << 1 | u16::from(c[3] >= 128)
}

/// Writes the RGBA8 pixels `px` as the colour image at `image` with pixel
/// size code `siz`, as [`put_pixel`] does each one, into one slice of RAM:
/// pixels past the end of RAM are dropped.
fn put_pixels(ram: &mut [u8], image: u32, siz: u32, px: &[u8]) {
    let Some(dst) = ram.get_mut(image as usize..) else {
        return;
    };
    let px = px.as_chunks::<4>().0;
    match siz {
        3 => {
            let n = px.len().min(dst.len() / 4) * 4;
            dst[..n].copy_from_slice(&px.as_flattened()[..n]);
        }
        1 => dst.iter_mut().zip(px).for_each(|(d, c)| *d = c[0]),
        _ => (dst.as_chunks_mut::<2>().0.iter_mut())
            .zip(px)
            .for_each(|(d, c)| *d = to_rgba5551(*c).to_be_bytes()),
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Reads back nothing: these lists load no texture.
    struct NoReadback;

    impl Readback for NoReadback {
        fn read(&mut self, _: &Frame, _: u32, _: u32, _: u32) -> Option<Vec<u8>> {
            None
        }
    }

    /// The per-pixel writes `put_pixels` stands in for: the reference.
    fn put_each(ram: &mut [u8], image: u32, siz: u32, px: &[u8]) {
        for (i, p) in px.as_chunks::<4>().0.iter().enumerate() {
            put_pixel(ram, image, siz, i as u32, *p);
        }
    }

    #[test]
    fn put_pixels_matches_put_pixel_also_past_the_end_of_ram() {
        let px: Vec<u8> = (0..64 * 4).map(|k| (k * 37 + 11) as u8).collect();
        for siz in 0..4 {
            // Inside, odd start, straddling the end, at the end, past it.
            for image in [0, 3, 1000, 1023, 1024, 2000] {
                let mut want = vec![0xA5; 1024];
                let mut got = want.clone();
                put_each(&mut want, image, siz, &px);
                put_pixels(&mut got, image, siz, &px);
                assert_eq!(got, want, "siz {siz} image {image}");
            }
        }
    }

    /// `cargo test -p n64rusty-gfx --release put_pixels_timing -- --ignored
    /// --nocapture`: read-back RAM writes per pixel against in bulk.
    #[test]
    #[ignore = "timing, not a check"]
    fn put_pixels_timing() {
        let px: Vec<u8> = (0..640 * 480 * 4).map(|k| k as u8).collect();
        let mut ram = vec![0; 8 << 20];
        let mut time = |put: fn(&mut [u8], u32, u32, &[u8]), siz| {
            let t = std::time::Instant::now();
            for _ in 0..100 {
                put(std::hint::black_box(&mut ram), 0x10_0000, siz, &px);
            }
            t.elapsed() / 100
        };
        for siz in [2, 3] {
            let each = time(put_each, siz);
            let bulk = time(put_pixels, siz);
            println!("640x480 siz {siz}: per pixel {each:?}, bulk {bulk:?}");
        }
    }

    /// A list drawing a 1-cycle rectangle into each of `images` colour
    /// images, 8 pixels wide, at the start of a RAM it returns.
    fn rects_into(images: usize) -> Vec<u8> {
        const IMAGES_AT: u32 = 0x10000;
        let mut words = Vec::new();
        for k in 0..images as u32 {
            let cimg = (0xFF << 24) | (2 << 19) | (8 - 1);
            let fill_rect = (0xF6 << 24) | ((4 * 4) << 12) | (4 * 4);
            words.extend([cimg, IMAGES_AT + k * 0x100, fill_rect, 0]);
        }
        words.extend([0xDF << 24, 0]);
        let mut ram: Vec<u8> = words.iter().flat_map(|w| w.to_be_bytes()).collect();
        ram.resize(0x2_0000, 0);
        ram
    }

    #[test]
    fn warnings_past_the_limit_are_counted_not_kept() {
        // Unknown commands, each a warning, then the end of the list.
        let extra = 5;
        let mut words = Vec::new();
        for _ in 0..MAX_WARNINGS + extra {
            words.extend([0x08 << 24, 0]);
        }
        words.extend([G_ENDDL << 24, 0]);
        let ram: Vec<u8> = words.iter().flat_map(|w| w.to_be_bytes()).collect();
        let mut it = Interpreter::new();
        it.run(&ram, 0).unwrap();
        let f = it.take_frame();
        assert_eq!(f.warnings.len(), MAX_WARNINGS);
        assert_eq!(f.warnings_dropped, extra as u32);
        // The count starts again with the next frame.
        assert_eq!(it.take_frame().warnings_dropped, 0);
    }

    #[test]
    fn a_list_run_without_read_back_keeps_no_drawn_images_or_frames() {
        let ram = rects_into(2);
        let mut it = Interpreter::new();
        it.run(&ram, 0).unwrap();
        assert_eq!(it.take_frame().draws().count(), 2);
        assert!(it.drawn.is_empty());
    }

    #[test]
    fn a_list_run_with_read_back_keeps_a_frame_for_its_drawn_images() {
        let mut ram = rects_into(2);
        let mut it = Interpreter::new();
        it.run_readback(&mut ram, 0, &mut NoReadback).unwrap();
        it.take_frame();
        assert_eq!(it.drawn.len(), 2);
        assert!(it.drawn.iter().all(|d| d.frame.is_some()));
    }

    #[test]
    fn drawn_images_past_the_limit_forget_the_oldest() {
        let mut ram = rects_into(MAX_DRAWN + 1);
        let mut it = Interpreter::new();
        it.run_readback(&mut ram, 0, &mut NoReadback).unwrap();
        assert_eq!(it.drawn.len(), MAX_DRAWN);
        assert_eq!(it.drawn[0].image, 0x10100, "the first image is gone");
        let warnings = &it.take_frame().warnings;
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("0x00010000 forgotten"), "{warnings:?}");
    }
}
