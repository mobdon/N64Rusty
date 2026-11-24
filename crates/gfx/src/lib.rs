//! High-level N64 graphics: display lists in, wgpu draws out.
//!
//! The game's graphics microcode is not run. Instead [`Interpreter`] reads
//! the display lists the game builds in guest RAM, does the vertex work the
//! microcode would do (matrix stack, lighting, texture coordinates), and
//! records triangles plus the colour-combiner and render-mode state they were
//! drawn with into a [`Frame`]. [`Renderer`] then draws a frame with wgpu,
//! using fragment shaders generated from the combiner settings, and can read
//! the result back for a headless PNG capture ([`Capture`]).
//!
//! The command encoding followed is the common "F3DEX2" variant.
//!
//! # Room for widescreen and high refresh
//!
//! A frame keeps vertices in object space and every vertex points at a
//! [`Transform`] that holds the model-view and projection matrices and the
//! viewport separately. Nothing is baked into screen space until
//! [`Renderer::render`]. A later step can therefore widen the projection, or
//! render extra frames between two game frames by blending matching
//! transforms (matched by [`Transform::tag`]), without touching game logic
//! or game speed: the game still produces exactly one [`Frame`] per frame.

#![forbid(unsafe_code)]

mod capture;
mod combiner;
mod dl;
mod render;
mod tmem;

pub use capture::{write_png, Capture};
pub use combiner::{fragment_shader, Combiner};
pub use dl::{GfxError, Interpreter};
pub use render::{Image, RenderError, Renderer};

/// A 4x4 matrix in N64 layout: row vectors, so a point transforms as
/// `p * m` and `m[3]` holds the translation.
pub type Mat4 = [[f32; 4]; 4];

/// Viewport in N64 framebuffer pixels: `screen = ndc * scale + trans`, with
/// the y axis flipped (`screen_y = trans_y - ndc_y * scale_y`), as the
/// microcode does.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Viewport {
    pub scale: [f32; 2],
    pub trans: [f32; 2],
}

/// The matrices a group of vertices was loaded with.
#[derive(Clone, Debug, PartialEq)]
pub struct Transform {
    pub modelview: Mat4,
    pub projection: Mat4,
    pub viewport: Viewport,
    /// Physical address of the vertex-load command that used this
    /// transform. Display lists are usually rebuilt from the same code each
    /// frame, so this identifies "the same object" across frames.
    pub tag: u32,
}

/// One vertex of a recorded triangle, still in object space.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Vertex {
    pub pos: [f32; 3],
    /// Index into [`Frame::transforms`].
    pub transform: u32,
    /// Shade colour (vertex colour, or the lit colour), 0 to 1.
    pub color: [f32; 4],
    /// Normalised texture coordinates for the draw's two tiles.
    pub uv: [[f32; 2]; 2],
}

/// A decoded texture, ready to upload.
#[derive(Clone, Debug, PartialEq)]
pub struct Texture {
    pub width: u32,
    pub height: u32,
    /// RGBA8, row-major.
    pub rgba: Vec<u8>,
    /// Wrap mode for s and t.
    pub wrap: [Wrap; 2],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Wrap {
    Repeat,
    Mirror,
    Clamp,
}

/// Everything about the pipeline that a triangle was drawn with.
#[derive(Clone, Debug, PartialEq)]
pub struct RenderState {
    /// The raw `SETCOMBINE` words (`w0 & 0xFFFFFF` << 32 | `w1`).
    pub combine: u64,
    /// Other modes, high word (cycle type, filtering, palette type).
    pub other_h: u32,
    /// Other modes, low word (alpha compare, depth source, blender).
    pub other_l: u32,
    /// Microcode geometry mode (depth buffer, culling, lighting).
    pub geometry: u32,
    pub prim: [f32; 4],
    /// Primitive LOD fraction, set with the primitive colour.
    pub prim_lod: f32,
    pub env: [f32; 4],
    pub blend: [f32; 4],
    pub fog: [f32; 4],
    /// Indices into [`Frame::textures`] for tile 0 and tile 1 of the draw.
    pub textures: [Option<u32>; 2],
    /// Screen-space rectangle (fill or texture rectangle), not a triangle.
    pub rect: bool,
    /// Address of the colour image this was drawn into.
    pub color_image: u32,
    /// Scissor box in framebuffer pixels (x0, y0, x1, y1), if one was set.
    pub scissor: Option<[f32; 4]>,
}

/// A run of triangles sharing one [`RenderState`].
#[derive(Clone, Debug, PartialEq)]
pub struct Draw {
    /// First vertex in [`Frame::vertices`]; three vertices per triangle.
    pub first: u32,
    pub count: u32,
    pub state: RenderState,
}

/// What the renderer does, in order.
#[derive(Clone, Debug, PartialEq)]
pub enum Command {
    Draw(Draw),
    /// The game filled the depth buffer (a fill rectangle into the depth
    /// image), which on a GPU is a depth clear.
    ClearDepth,
}

/// Everything recorded from one frame's display lists.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Frame {
    /// Width of the colour image in pixels, from `SETCIMG`.
    pub width: u32,
    /// Address of the colour image the frame is drawn into (the last one
    /// set). Draws into other images (render-to-texture) are skipped.
    pub color_image: u32,
    pub transforms: Vec<Transform>,
    pub vertices: Vec<Vertex>,
    pub textures: Vec<Texture>,
    pub commands: Vec<Command>,
    /// Commands that were skipped, with their address.
    pub warnings: Vec<String>,
}

impl Frame {
    /// The N64 framebuffer height, assuming the usual 4:3 shape.
    pub fn height(&self) -> u32 {
        (self.width * 3 / 4).max(1)
    }

    /// The draws, in order.
    pub fn draws(&self) -> impl Iterator<Item = &Draw> {
        self.commands.iter().filter_map(|c| match c {
            Command::Draw(d) => Some(d),
            Command::ClearDepth => None,
        })
    }
}

/// Multiply two N64 matrices (`a` applied first).
pub fn mat_mul(a: &Mat4, b: &Mat4) -> Mat4 {
    let mut r = [[0.0; 4]; 4];
    for (i, row) in r.iter_mut().enumerate() {
        for (j, v) in row.iter_mut().enumerate() {
            *v = (0..4).map(|k| a[i][k] * b[k][j]).sum();
        }
    }
    r
}

pub const IDENTITY: Mat4 = [
    [1.0, 0.0, 0.0, 0.0],
    [0.0, 1.0, 0.0, 0.0],
    [0.0, 0.0, 1.0, 0.0],
    [0.0, 0.0, 0.0, 1.0],
];
