//! Draws a [`Frame`] with wgpu into an offscreen image.
//!
//! Headless by design: no window or surface is ever created. Vertices are
//! transformed at draw time from object space, by the vertex shader with
//! the frame's transforms in a uniform buffer (or on the CPU, for a draw
//! whose transforms do not fit), so the transforms in a frame can still be
//! changed (widescreen, interpolation) right up to the call to
//! [`Renderer::render`].

use std::collections::HashMap;
use std::fmt;
use std::sync::OnceLock;

use crate::combiner::{shader, ShaderKey, VF_BACKDROP, VF_FOG, VF_RAW, VF_STRETCH};
use crate::dl::{
    cycle_type, G_CULL_BACK, G_CULL_FRONT, G_CYC_2CYCLE, G_CYC_COPY, G_CYC_FILL, G_MDSFT_TEXTFILT,
    G_SHADING_SMOOTH, G_ZBUFFER,
};
use crate::{mat_mul, Command, Frame, RenderState, Wrap};

/// Pixels read back from the GPU.
#[derive(Clone, Debug, PartialEq)]
pub struct Image {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// RGBA8, row-major, top row first.
    pub rgba: Vec<u8>,
}

impl Image {
    /// The RGBA of the pixel at (`x`, `y`); panics outside the image.
    pub fn pixel(&self, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * self.width + x) * 4) as usize;
        [
            self.rgba[i],
            self.rgba[i + 1],
            self.rgba[i + 2],
            self.rgba[i + 3],
        ]
    }
}

/// The GPU could not be set up or used.
#[derive(Debug)]
pub enum RenderError {
    /// No GPU or software adapter was found.
    NoAdapter(String),
    /// The device could not be created, or reported an error.
    Device(String),
    /// The picture could not be read back.
    Readback(String),
    /// A size or pixel buffer the device cannot take; nothing was drawn.
    Size(String),
}

impl fmt::Display for RenderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RenderError::NoAdapter(e) => write!(f, "no graphics adapter: {e}"),
            RenderError::Device(e) => write!(f, "graphics device: {e}"),
            RenderError::Readback(e) => write!(f, "reading the image back: {e}"),
            RenderError::Size(e) => write!(f, "bad size: {e}"),
        }
    }
}

impl std::error::Error for RenderError {}

const COLOR_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;
const DEPTH_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Depth32Float;
/// Bytes in one RGBA8 pixel.
const BYTES_PER_PIXEL: u64 = 4;
/// Bytes per draw in the uniform buffer (five vec4s, padded to the
/// guaranteed dynamic-offset alignment).
const UNIFORM_STRIDE: u64 = 256;
/// The uniforms a draw uses: prim, env, blend, misc and fog colours.
const UNIFORM_SIZE: u64 = 80;
/// Floats per vertex: position, colour, two texture coordinates, and the
/// extra vertex-stage values (fog multiplier and offset, x parameters).
const VERTEX_FLOATS: usize = 16;
/// Words after the floats: transform index and flags (`VF_*`).
const VERTEX_WORDS: usize = 2;
/// Bytes per vertex in the vertex buffer.
const VERTEX_BYTES: u64 = ((VERTEX_FLOATS + VERTEX_WORDS) * 4) as u64;
/// Bytes per transform in the transform window: the clip matrix, then
/// whether it is flat (orthographic) in the first float of a vec4.
const TRANSFORM_BYTES: u64 = 80;
/// Transforms the vertex stage's window holds at most.
const TRANSFORM_WINDOW_MAX: u64 = 1024;
/// Below this many transforms in a window (a device with tiny uniform
/// buffers) every vertex is moved on the CPU.
const TRANSFORM_WINDOW_MIN: u64 = 32;

// Blender inputs: P and M pick a colour, A and B a factor.
const BL_IN: u32 = 0;
const BL_MEM: u32 = 1;
const BL_FOG: u32 = 3;
const BL_A_SHADE: u32 = 2;
/// Blender A input: zero.
const BL_A_ZERO: u32 = 3;
/// Blender B input: one minus A.
const BL_B_ONE_MINUS_A: u32 = 0;
/// Geometry mode: the vertex shade alpha is the fog factor.
const G_FOG: u32 = 0x0001_0000;

// Other modes, low word (render mode).
/// Alpha compare against the blend colour's alpha.
const G_AC_THRESHOLD: u32 = 1;
/// The two alpha compare bits.
const G_AC_MASK: u32 = 3;
/// Rectangles take their depth from the primitive depth.
const G_ZS_PRIM: u32 = 0x0004;
const Z_CMP: u32 = 0x0010;
const Z_UPD: u32 = 0x0020;
/// The depth mode field; [`ZMODE_DEC`] is decal.
const ZMODE_SHIFT: u32 = 10;
const ZMODE_DEC: u32 = 3;
/// Coverage times alpha with alpha as coverage: the "texture edge" modes.
const CVG_X_ALPHA_SEL: u32 = 0x3000;
const FORCE_BL: u32 = 0x4000;

// Widescreen tuning, as fractions of the frame's width unless named.
/// A row of background rectangles starts by this much from the left edge
/// and ends past [`BG_ROW_END`] to stretch edge to edge.
const BG_ROW_START: f32 = 0.05;
const BG_ROW_END: f32 = 0.95;
/// A box spans the frame's width when it starts by this much and ends
/// past [`SPAN_END`]: the border games often leave.
const SPAN_START: f32 = 0.1;
const SPAN_END: f32 = 0.9;
/// A backdrop covers at least this much of the clip-space width (2), nine
/// tenths of the view.
const BACKDROP_MIN_SPAN: f32 = 1.8;
/// A flat overlay reaches past this in clip space on every side.
const OVERLAY_EDGE: f32 = 0.98;
/// Vertices of one flat overlay have the same w to within this fraction.
const OVERLAY_FLAT_W: f32 = 1e-3;
/// A HUD panel is at least this many pixels wide, and narrower than
/// [`PANEL_MAX_WIDTH`] of the frame.
const PANEL_MIN_PX: f32 = 100.0;
const PANEL_MAX_WIDTH: f32 = 0.9;
/// A 3D viewport at least this wide is the scene, not a HUD piece.
const HUD_3D_VIEWPORT: f32 = 0.5;

/// Everything that needs its own pipeline.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct PipeKey {
    shader: ShaderKey,
    /// Source and destination factors, or opaque.
    blend: Option<(wgpu::BlendFactor, wgpu::BlendFactor)>,
    depth_test: bool,
    depth_write: bool,
    decal: bool,
    /// The faces culled, if any.
    cull: Option<wgpu::Face>,
}

fn pipe_key(s: &RenderState) -> PipeKey {
    let cycle = cycle_type(s.other_h);
    let two_cycle = cycle == G_CYC_2CYCLE;
    let copy_or_fill = cycle >= G_CYC_COPY;
    // Coverage times alpha ("texture edge" modes) behaves like a test at
    // one half. With alpha compare also on, both apply: a compare against a
    // zero blend alpha alone would draw the texture's clear parts solid.
    let alpha_test = match (
        cycle == G_CYC_FILL,
        s.other_l & G_AC_MASK == G_AC_THRESHOLD,
        s.other_l & CVG_X_ALPHA_SEL == CVG_X_ALPHA_SEL,
    ) {
        (true, _, _) => 0,
        (_, true, true) => 3,
        (_, true, false) => 1,
        (_, false, true) => 2,
        _ => 0,
    };
    // In two-cycle mode the second blender cycle writes the pixel.
    let sh = if two_cycle { 16 } else { 18 };
    let field = |k: u32| (s.other_l >> (sh + k * 4)) & 3;
    let (b, m, a, p) = (field(0), field(1), field(2), field(3));
    let force_blend = s.other_l & FORCE_BL != 0;
    // shortcut: only "IN * A + MEM * B" blends; fog and blend-colour mixes
    // are drawn opaque. Add them when a game needs them.
    let blend = (!copy_or_fill && force_blend && p == BL_IN && m == BL_MEM).then(|| {
        let src = if a == BL_A_ZERO {
            wgpu::BlendFactor::Zero
        } else {
            wgpu::BlendFactor::SrcAlpha
        };
        let dst = match b {
            BL_B_ONE_MINUS_A if a == BL_A_ZERO => wgpu::BlendFactor::One,
            BL_B_ONE_MINUS_A => wgpu::BlendFactor::OneMinusSrcAlpha,
            1 | 2 => wgpu::BlendFactor::One, // memory alpha, one
            _ => wgpu::BlendFactor::Zero,
        };
        (src, dst)
    });
    // Fogged surfaces: in two-cycle mode the first blender cycle is
    // "FOG * SHADE_A + IN * (1 - A)", the second writes the pixel as usual.
    let first = |k: u32| (s.other_l >> (18 + k * 4)) & 3;
    let fog = two_cycle && !s.rect && first(3) == BL_FOG && first(2) == BL_A_SHADE;
    let zbuf = if s.rect {
        s.other_l & G_ZS_PRIM != 0 // rectangles compare only with the primitive depth
    } else {
        s.geometry & G_ZBUFFER != 0
    };
    let depth = zbuf && !copy_or_fill;
    let cull = if s.rect || copy_or_fill {
        None
    } else if s.geometry & G_CULL_BACK != 0 {
        Some(wgpu::Face::Back)
    } else if s.geometry & G_CULL_FRONT != 0 {
        Some(wgpu::Face::Front)
    } else {
        None
    };
    PipeKey {
        shader: ShaderKey {
            combine: s.combine,
            two_cycle,
            alpha_test,
            flat: !s.rect && s.geometry & G_SHADING_SMOOTH == 0,
            sharp: false,
            fog,
        },
        blend,
        depth_test: depth && s.other_l & Z_CMP != 0,
        depth_write: depth && s.other_l & Z_UPD != 0,
        decal: (s.other_l >> ZMODE_SHIFT) & 3 == ZMODE_DEC,
        cull,
    }
}

/// The process-wide instance and adapter, found once.
///
/// Every renderer opens its own device on this adapter. Enumerating
/// adapters again for each renderer let one thread's enumeration unload a
/// driver while another renderer's device was calling into it (the Vulkan
/// loader drops drivers it probed), which crashed parallel tests on Linux.
fn adapter() -> Result<&'static wgpu::Adapter, RenderError> {
    gpu().map(|(_, a)| a)
}

fn gpu() -> Result<(&'static wgpu::Instance, &'static wgpu::Adapter), RenderError> {
    static ADAPTER: OnceLock<Result<(wgpu::Instance, wgpu::Adapter), String>> = OnceLock::new();
    let found = ADAPTER.get_or_init(|| {
        let mut desc = wgpu::InstanceDescriptor::new_without_display_handle();
        desc.backends = wgpu::Backends::VULKAN
            | wgpu::Backends::METAL
            | wgpu::Backends::DX12
            | wgpu::Backends::GL;
        let instance = wgpu::Instance::new(desc);
        let mut opts = wgpu::RequestAdapterOptions::default();
        let adapter = pollster::block_on(instance.request_adapter(&opts)).or_else(|_| {
            opts.force_fallback_adapter = true;
            pollster::block_on(instance.request_adapter(&opts)).map_err(|e| e.to_string())
        })?;
        Ok((instance, adapter))
    });
    match found {
        Ok((instance, adapter)) => Ok((instance, adapter)),
        Err(e) => Err(RenderError::NoAdapter(e.clone())),
    }
}

/// Where the renderer's time went since the last [`Renderer::take_stats`]
/// (host time in microseconds), for a per-frame profile line.
#[derive(Clone, Copy, Debug, Default)]
pub struct Stats {
    /// Pictures drawn.
    pub frames: u32,
    /// Draws recorded.
    pub draws: u64,
    /// Recording a picture: vertices, uniforms, bindings and uploads.
    pub record_us: u64,
    /// Of which uploading textures.
    pub upload_us: u64,
    /// Submitting, and waiting for a read-back or a present.
    pub submit_us: u64,
    /// Textures uploaded.
    pub textures_new: u64,
    /// Bytes of the textures uploaded (top level only).
    pub texture_bytes: u64,
}

/// A headless wgpu renderer for [`Frame`]s.
pub struct Renderer {
    device: wgpu::Device,
    queue: wgpu::Queue,
    adapter_name: String,
    uniform_layout: wgpu::BindGroupLayout,
    texture_layout: wgpu::BindGroupLayout,
    layout: wgpu::PipelineLayout,
    shaders: HashMap<ShaderKey, wgpu::ShaderModule>,
    pipelines: HashMap<PipeKey, wgpu::RenderPipeline>,
    samplers: HashMap<(Wrap, Wrap, bool), wgpu::Sampler>,
    white: wgpu::TextureView,
    widescreen: bool,
    sharp: bool,
    /// Colour and depth targets of the last size drawn.
    targets: Option<(u32, u32, wgpu::Texture, wgpu::Texture)>,
    stats: Stats,
    /// Uploaded textures by content, kept across pictures: most are the
    /// same from one frame to the next. Off with
    /// `N64RUSTY_TEXTURE_CACHE=off`.
    uploaded: Option<HashMap<u64, Uploaded>>,
    /// Content keys of the texture pixels seen in the last pictures, by
    /// the shared pixels themselves, so the same pixels are not hashed
    /// again picture after picture.
    known: HashMap<PixelsId, Known>,
    /// Pictures drawn so far, to know which textures are still in use.
    pictures: u64,
    /// Vertex, uniform and read-back buffers kept from the last picture.
    /// Each draw is submitted before the next one writes them again.
    vertex_buf: Option<wgpu::Buffer>,
    uniform_buf: Option<wgpu::Buffer>,
    transform_buf: Option<wgpu::Buffer>,
    readback_buf: Option<wgpu::Buffer>,
    /// The vertex buffer's bytes, kept to be written again next picture.
    vertex_scratch: Vec<u8>,
    /// The transforms the vertex stage sees at once on this device.
    window: TransformWindow,
    /// Move vertices to clip space in the vertex shader. Off (every vertex
    /// moved on the CPU, the reference path) with
    /// `N64RUSTY_GPU_VERTICES=off`, or when the device's uniform buffers
    /// are too small for a useful window.
    gpu_vertices: bool,
    /// Texture bindings by what they bind, kept with the uploaded textures.
    bindings: HashMap<BindKey, wgpu::BindGroup>,
    /// Keep the vertex and uniform buffers across pictures. Off with
    /// `N64RUSTY_BUFFER_REUSE=off`, read when the renderer is made.
    buffer_reuse: bool,
    /// See [`Renderer::set_max_height`].
    max_height: Option<u32>,
}

/// The size to draw a `width` x `height` place at when pictures are at
/// most `max` lines high: the same shape, at least one pixel each way.
fn capped(width: u32, height: u32, max: Option<u32>) -> (u32, u32) {
    match max {
        Some(m) if height > m && m > 0 => {
            let w = (u64::from(width) * u64::from(m) + u64::from(height) / 2) / u64::from(height);
            ((w as u32).max(1), m)
        }
        _ => (width, height),
    }
}

/// Content key and wrap of each texture slot, and linear filtering.
type BindKey = ([Option<(u64, [Wrap; 2])>; 2], bool);

/// The run of transforms the vertex stage sees during one draw: `len`
/// transforms from a first one that is a multiple of `step`, so that its
/// byte offset meets the device's uniform offset alignment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TransformWindow {
    len: u32,
    step: u32,
}

impl TransformWindow {
    /// The window for a device with these uniform limits; `None` when it
    /// would hold too few transforms to be of use.
    fn from_limits(max_binding: u64, offset_align: u32) -> Option<Self> {
        let len = (max_binding / TRANSFORM_BYTES).min(TRANSFORM_WINDOW_MAX);
        let align = u64::from(offset_align.max(1));
        let step = align / gcd(TRANSFORM_BYTES, align);
        (len >= TRANSFORM_WINDOW_MIN && len >= 2 * step).then_some(Self {
            len: len as u32,
            step: step as u32,
        })
    }

    /// The first transform of the window for a draw whose vertices use
    /// transforms `lo` to `hi`, if they fit in one window.
    fn base(&self, lo: u32, hi: u32) -> Option<u32> {
        let base = lo / self.step * self.step;
        (hi >= lo && hi - base < self.len).then_some(base)
    }
}

fn gcd(a: u64, b: u64) -> u64 {
    if b == 0 {
        a
    } else {
        gcd(b, a % b)
    }
}

struct Uploaded {
    view: wgpu::TextureView,
    bytes: u64,
    last_used: u64,
}

/// Which shared texture pixels: their address and length, and the
/// texture's width and height. Unique while [`Known`] holds the pixels.
type PixelsId = (usize, usize, u32, u32);

/// The content key of shared texture pixels.
struct Known {
    /// Held so the address cannot be reused by other pixels.
    _pixels: std::sync::Arc<[u8]>,
    key: u64,
    last_used: u64,
}

/// Pictures a texture's pixels stay known after their last use.
const KNOWN_PICTURES: u64 = 4;

/// The content key of `t`: a hash of its size and pixels.
fn content_key(t: &crate::Texture) -> u64 {
    use std::hash::Hasher;
    let mut h = crate::tmem::Fx(0);
    h.write_u32(t.width);
    h.write_u32(t.height);
    h.write(&t.rgba);
    h.finish()
}

/// Uploaded textures kept, by count and by bytes: thousands of small
/// textures cost a view each even under the byte limit.
const UPLOADED_MAX: usize = 2048;
const UPLOADED_MAX_BYTES: u64 = 256 << 20;
/// Texture bindings kept; all are dropped past this.
const BINDINGS_MAX: usize = 2048;

/// A graphics device several renderers can share: open it once with
/// [`Gpu::open`] and make each renderer on it with [`Renderer::on`], so a
/// window and an off-screen renderer use one device instead of two.
#[derive(Clone)]
pub struct Gpu {
    device: wgpu::Device,
    queue: wgpu::Queue,
    adapter_name: String,
}

impl Gpu {
    /// Opens a device on the first GPU or software adapter.
    pub fn open() -> Result<Gpu, RenderError> {
        let adapter = adapter()?;
        let info = adapter.get_info();
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("n64rusty-gfx"),
            // All the adapter offers: the lowest limits cap textures at
            // 2048 pixels, smaller than a full-screen window.
            required_limits: adapter.limits(),
            ..Default::default()
        }))
        .map_err(|e| RenderError::Device(e.to_string()))?;
        Ok(Gpu {
            device,
            queue,
            adapter_name: format!("{} ({:?})", info.name, info.backend),
        })
    }
}

impl Renderer {
    /// Open the first GPU or software adapter, with no window, on a device
    /// of its own.
    pub fn new() -> Result<Self, RenderError> {
        Ok(Self::on(&Gpu::open()?))
    }

    /// A renderer on `gpu`'s device, shared with the other renderers on it.
    pub fn on(gpu: &Gpu) -> Self {
        let Gpu {
            device,
            queue,
            adapter_name,
        } = gpu.clone();
        let limits = device.limits();
        let found = TransformWindow::from_limits(
            limits.max_uniform_buffer_binding_size,
            limits.min_uniform_buffer_offset_alignment,
        );
        // Without a window the shaders still declare one transform.
        let window = found.unwrap_or(TransformWindow { len: 1, step: 1 });
        let gpu_vertices =
            found.is_some() && std::env::var_os("N64RUSTY_GPU_VERTICES").is_none_or(|v| v != "off");
        let uniform_entry = |binding, visibility, size| wgpu::BindGroupLayoutEntry {
            binding,
            visibility,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Uniform,
                has_dynamic_offset: true,
                min_binding_size: wgpu::BufferSize::new(size),
            },
            count: None,
        };
        let uniform_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("uniforms"),
            entries: &[
                uniform_entry(0, wgpu::ShaderStages::VERTEX_FRAGMENT, UNIFORM_SIZE),
                uniform_entry(
                    1,
                    wgpu::ShaderStages::VERTEX,
                    u64::from(window.len) * TRANSFORM_BYTES,
                ),
            ],
        });
        let texture_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("textures"),
            entries: &[
                texture_layout_entry(0),
                sampler_layout_entry(1),
                texture_layout_entry(2),
                sampler_layout_entry(3),
            ],
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("n64"),
            bind_group_layouts: &[Some(&uniform_layout), Some(&texture_layout)],
            immediate_size: 0,
        });
        let white = upload(&device, &queue, 1, 1, &[255; 4]);
        Self {
            device,
            queue,
            adapter_name,
            uniform_layout,
            texture_layout,
            layout,
            shaders: HashMap::new(),
            pipelines: HashMap::new(),
            samplers: HashMap::new(),
            white,
            widescreen: false,
            sharp: true,
            targets: None,
            stats: Stats::default(),
            uploaded: (std::env::var("N64RUSTY_TEXTURE_CACHE").as_deref() != Ok("off"))
                .then(HashMap::new),
            pictures: 0,
            vertex_buf: None,
            uniform_buf: None,
            transform_buf: None,
            readback_buf: None,
            vertex_scratch: Vec::new(),
            window,
            gpu_vertices,
            bindings: HashMap::new(),
            known: HashMap::new(),
            buffer_reuse: std::env::var_os("N64RUSTY_BUFFER_REUSE").is_none_or(|v| v != "off"),
            max_height: None,
        }
    }

    /// Caps the height of the picture drawn for a window (and for
    /// [`Renderer::render_letterboxed`]) at `max` lines, scaled up to the
    /// window afterwards: fewer pixels to draw on a large screen. `None`
    /// (the default) draws at the window's own resolution, and so does
    /// `Some(0)`.
    pub fn set_max_height(&mut self, max: Option<u32>) {
        self.max_height = max;
    }

    /// Widescreen: a target wider than the game's frame shows more of the
    /// 3D scene at the sides instead of black bars. Everything keeps its
    /// shape: the 4:3 picture sits in the middle, and a rectangle or
    /// scissor box spanning the frame's whole width (backgrounds, the 3D
    /// view) is widened to the target. Off by default.
    pub fn set_widescreen(&mut self, on: bool) {
        self.widescreen = on;
    }

    /// Sharp textures (the default): magnified textures keep crisp texel
    /// edges with a one-screen-pixel blend between texels, instead of the
    /// console's soft filtering stretched to the window. Off gives the
    /// console look.
    pub fn set_sharp_textures(&mut self, on: bool) {
        self.sharp = on;
    }

    /// How many times wider than the frame's own shape a `width` x
    /// `height` target is drawn: 1 unless widescreen is on and the target
    /// is wider.
    fn wide(&self, frame: &Frame, width: u32, height: u32) -> f32 {
        // A room drawn on a pre-rendered picture has nothing made for the
        // sides (its 3D is only the parts in front of the picture): 4:3.
        let picture = || {
            frame
                .draws()
                .any(|d| d.state.picture && d.state.color_image == frame.color_image)
        };
        if !self.widescreen || picture() {
            return 1.0;
        }
        let r = (width as f32 * frame.height() as f32) / (height as f32 * frame.width as f32);
        if r.is_finite() {
            r.max(1.0)
        } else {
            1.0
        }
    }

    /// Where a frame goes in a `width` x `height` window: all of it in
    /// widescreen, else the largest centred rectangle of the frame's shape.
    fn place(&self, frame: &Frame, width: u32, height: u32) -> (u32, u32, u32, u32) {
        if self.wide(frame, width, height) > 1.0 {
            (0, 0, width, height)
        } else {
            fit(width, height, frame.width, frame.height())
        }
    }

    /// Time spent since the last [`Renderer::take_stats`].
    pub fn stats(&self) -> Stats {
        self.stats
    }

    /// Time spent since the last call (see [`Stats`]), and start again.
    pub fn take_stats(&mut self) -> Stats {
        std::mem::take(&mut self.stats)
    }

    /// Name and backend of the adapter in use.
    pub fn adapter_name(&self) -> &str {
        &self.adapter_name
    }

    fn pipeline(&mut self, key: PipeKey) -> &wgpu::RenderPipeline {
        if !self.pipelines.contains_key(&key) {
            let device = &self.device;
            let transforms = self.window.len;
            let module = self.shaders.entry(key.shader).or_insert_with(|| {
                device.create_shader_module(wgpu::ShaderModuleDescriptor {
                    label: Some("combiner"),
                    source: wgpu::ShaderSource::Wgsl(shader(key.shader, transforms).into()),
                })
            });
            let f = std::mem::size_of::<f32>() as u64;
            let attr = |format, floats: u64, shader_location| wgpu::VertexAttribute {
                format,
                offset: floats * f,
                shader_location,
            };
            use wgpu::VertexFormat::{Float32x2, Float32x4, Uint32x2};
            let attrs = [
                attr(Float32x4, 0, 0),
                attr(Float32x4, 4, 1),
                attr(Float32x2, 8, 2),
                attr(Float32x2, 10, 3),
                attr(Float32x4, 12, 4),
                attr(Uint32x2, VERTEX_FLOATS as u64, 5),
            ];
            let blend = key.blend.map(|(src, dst)| {
                let c = wgpu::BlendComponent {
                    src_factor: src,
                    dst_factor: dst,
                    operation: wgpu::BlendOperation::Add,
                };
                wgpu::BlendState { color: c, alpha: c }
            });
            let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("n64"),
                layout: Some(&self.layout),
                vertex: wgpu::VertexState {
                    module,
                    entry_point: Some("vs"),
                    compilation_options: Default::default(),
                    buffers: &[Some(wgpu::VertexBufferLayout {
                        array_stride: VERTEX_BYTES,
                        step_mode: wgpu::VertexStepMode::Vertex,
                        attributes: &attrs,
                    })],
                },
                primitive: wgpu::PrimitiveState {
                    front_face: wgpu::FrontFace::Ccw,
                    cull_mode: key.cull,
                    ..Default::default()
                },
                depth_stencil: Some(wgpu::DepthStencilState {
                    format: DEPTH_FORMAT,
                    depth_write_enabled: Some(key.depth_write),
                    depth_compare: Some(if key.depth_test {
                        wgpu::CompareFunction::LessEqual
                    } else {
                        wgpu::CompareFunction::Always
                    }),
                    stencil: Default::default(),
                    bias: if key.decal {
                        wgpu::DepthBiasState {
                            constant: -2,
                            slope_scale: -1.0,
                            clamp: 0.0,
                        }
                    } else {
                        Default::default()
                    },
                }),
                multisample: Default::default(),
                fragment: Some(wgpu::FragmentState {
                    module,
                    entry_point: Some("fs"),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: COLOR_FORMAT,
                        blend,
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                multiview_mask: None,
                cache: None,
            });
            self.pipelines.insert(key, pipeline);
        }
        &self.pipelines[&key]
    }

    fn sampler(&mut self, wrap: [Wrap; 2], linear: bool) -> wgpu::Sampler {
        let device = &self.device;
        let mode = |w| match w {
            Wrap::Repeat => wgpu::AddressMode::Repeat,
            Wrap::Mirror => wgpu::AddressMode::MirrorRepeat,
            Wrap::Clamp => wgpu::AddressMode::ClampToEdge,
        };
        let filter = if linear {
            wgpu::FilterMode::Linear
        } else {
            wgpu::FilterMode::Nearest
        };
        self.samplers
            .entry((wrap[0], wrap[1], linear))
            .or_insert_with(|| {
                device.create_sampler(&wgpu::SamplerDescriptor {
                    address_mode_u: mode(wrap[0]),
                    address_mode_v: mode(wrap[1]),
                    mag_filter: filter,
                    min_filter: filter,
                    mipmap_filter: if linear {
                        wgpu::MipmapFilterMode::Linear
                    } else {
                        wgpu::MipmapFilterMode::Nearest
                    },
                    ..Default::default()
                })
            })
            .clone()
    }

    /// Draw `frame` at `width` x `height` pixels and read it back. The
    /// frame's native framebuffer is scaled to fill the image.
    pub fn render(&mut self, frame: &Frame, width: u32, height: u32) -> Result<Image, RenderError> {
        self.check_size("image", width, height)?;
        let (target, encoder) = self.draw(frame, &frame.transforms, width, height);
        self.read_back(encoder, &target)
    }

    /// An error unless the device can make a `width` x `height` texture
    /// named `what`.
    fn check_size(&self, what: &str, width: u32, height: u32) -> Result<(), RenderError> {
        let max = self.device.limits().max_texture_dimension_2d;
        if width == 0 || height == 0 || width > max || height > max {
            return Err(RenderError::Size(format!(
                "{what} of {width} x {height}: each side must be 1 to {max}"
            )));
        }
        Ok(())
    }

    /// Submits `encoder` and reads `target` back.
    fn read_back(
        &mut self,
        mut encoder: wgpu::CommandEncoder,
        target: &wgpu::Texture,
    ) -> Result<Image, RenderError> {
        let (width, height, size) = (target.width(), target.height(), target.size());
        // Read back, with rows padded to the copy alignment.
        let row = width * 4;
        let padded =
            row.div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let need = u64::from(padded) * u64::from(height);
        let max = self.device.limits().max_buffer_size;
        if need > max {
            return Err(RenderError::Size(format!(
                "a {width} x {height} image needs {need} bytes to read back, the device allows {max}"
            )));
        }
        let readback = match self.readback_buf.take() {
            Some(b) if b.size() >= need => b,
            _ => self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("readback"),
                size: need,
                usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                mapped_at_creation: false,
            }),
        };
        encoder.copy_texture_to_buffer(
            target.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: &readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded),
                    rows_per_image: Some(height),
                },
            },
            size,
        );
        let t0 = std::time::Instant::now();
        self.queue.submit([encoder.finish()]);
        let (tx, rx) = std::sync::mpsc::channel();
        readback.map_async(wgpu::MapMode::Read, .., move |r| {
            let _ = tx.send(r);
        });
        self.device
            .poll(wgpu::PollType::wait_indefinitely())
            .map_err(|e| RenderError::Readback(e.to_string()))?;
        rx.recv()
            .map_err(|e| RenderError::Readback(e.to_string()))?
            .map_err(|e| RenderError::Readback(e.to_string()))?;
        let data = readback
            .get_mapped_range(..)
            .map_err(|e| RenderError::Readback(e.to_string()))?;
        // In usize: a 32768-pixel square is 4 GiB, past u32.
        let mut rgba = Vec::with_capacity(row as usize * height as usize);
        for y in 0..height {
            let start = y as usize * padded as usize;
            rgba.extend_from_slice(&data[start..start + row as usize]);
        }
        drop(data);
        readback.unmap();
        self.readback_buf = Some(readback);
        self.stats.submit_us += t0.elapsed().as_micros() as u64;
        Ok(Image {
            width,
            height,
            rgba,
        })
    }

    /// The colour and depth targets for a `width` x `height` picture. The
    /// window draws at one size frame after frame: the last ones are kept.
    fn targets(&mut self, width: u32, height: u32) -> (wgpu::Texture, wgpu::Texture) {
        let size = wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        };
        let reuse = matches!(&self.targets, Some((w, h, ..)) if (*w, *h) == (width, height));
        if !reuse {
            let target = self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("color"),
                size,
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: COLOR_FORMAT,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                    | wgpu::TextureUsages::COPY_SRC
                    | wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            });
            let depth = self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("depth"),
                format: DEPTH_FORMAT,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                ..target_desc(size)
            });
            self.targets = Some((width, height, target, depth));
        }
        let (_, _, target, depth) = self.targets.clone().expect("targets were just made");
        (target, depth)
    }

    /// Records the passes that draw `frame` into a new `width` x `height`
    /// colour texture; nothing is submitted yet.
    fn draw(
        &mut self,
        frame: &Frame,
        transforms: &[crate::Transform],
        width: u32,
        height: u32,
    ) -> (wgpu::Texture, wgpu::CommandEncoder) {
        let t0 = std::time::Instant::now();
        let (target, depth) = self.targets(width, height);
        let color_view = target.create_view(&Default::default());
        let depth_view = depth.create_view(&Default::default());

        // Vertices, transformed to clip space. In widescreen everything is
        // narrowed about the centre to keep its shape, except rectangles
        // spanning the whole frame (backgrounds), which stretch from edge
        // to edge of the target, over the border the game left.
        // A background may also be a row of tiles: rectangles on the same
        // lines, edge to edge, that together span the frame.
        let wide = self.wide(frame, width, height);
        let draws: Vec<_> = frame
            .draws()
            // shortcut: draws into other colour images (render-to-texture)
            // are skipped; give them their own targets when a game needs it.
            .filter(|d| d.state.color_image == frame.color_image)
            .collect();
        let scratch = std::mem::take(&mut self.vertex_scratch);
        let gpu = (self.window, self.gpu_vertices);
        let data = vertex_data(frame, transforms, &draws, wide, gpu, scratch);
        let bases = &data.bases;
        let vbuf = kept_buffer(
            &self.device,
            &self.queue,
            &mut self.vertex_buf,
            &data.vertices,
            wgpu::BufferUsages::VERTEX,
            self.buffer_reuse,
        );
        let used = bases.iter().any(Option::is_some);
        let xbuf = match &self.transform_buf {
            // No draw reads the window: last picture's buffer binds as well.
            Some(b) if !used && self.buffer_reuse && b.size() >= data.transforms.len() as u64 => {
                b.clone()
            }
            _ => kept_buffer(
                &self.device,
                &self.queue,
                &mut self.transform_buf,
                &data.transforms,
                wgpu::BufferUsages::UNIFORM,
                self.buffer_reuse,
            ),
        };
        let shift = &data.shift;

        // Per-draw uniforms.
        let uniforms = draw_uniforms(&draws, wide, bases);
        let ubuf = kept_buffer(
            &self.device,
            &self.queue,
            &mut self.uniform_buf,
            &uniforms,
            wgpu::BufferUsages::UNIFORM,
            self.buffer_reuse,
        );
        let binding = |binding, buffer, size| wgpu::BindGroupEntry {
            binding,
            resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                buffer,
                offset: 0,
                size: wgpu::BufferSize::new(size),
            }),
        };
        let ubind = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("uniforms"),
            layout: &self.uniform_layout,
            entries: &[
                binding(0, &ubuf, UNIFORM_SIZE),
                binding(1, &xbuf, u64::from(self.window.len) * TRANSFORM_BYTES),
            ],
        });

        let t1 = std::time::Instant::now();
        let views = self.upload_textures(&frame.textures);
        self.stats.upload_us += t1.elapsed().as_micros() as u64;

        // Prepare pipelines and texture bindings before recording passes.
        let prepared = self.prepare(frame, &draws, &views);

        let mut encoder = self.device.create_command_encoder(&Default::default());
        let mut first_pass = true;
        let mut clear_depth = true;
        let mut next = 0;
        let mut commands = frame.commands.iter().peekable();
        while first_pass || commands.peek().is_some() {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("frame"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &color_view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: if first_pass {
                            wgpu::LoadOp::Clear(wgpu::Color::BLACK)
                        } else {
                            wgpu::LoadOp::Load
                        },
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &depth_view,
                    depth_ops: Some(wgpu::Operations {
                        load: if clear_depth {
                            wgpu::LoadOp::Clear(1.0)
                        } else {
                            wgpu::LoadOp::Load
                        },
                        store: wgpu::StoreOp::Store,
                    }),
                    stencil_ops: None,
                }),
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            first_pass = false;
            clear_depth = false;
            pass.set_vertex_buffer(0, vbuf.slice(..));
            for cmd in commands.by_ref() {
                let d = match cmd {
                    Command::ClearDepth => {
                        clear_depth = true;
                        break;
                    }
                    Command::Draw(d) if d.state.color_image == frame.color_image => d,
                    Command::Draw(_) => continue,
                };
                let i = next;
                next += 1;
                let (key, bind) = &prepared[i];
                let [mut x, y, w, h] = scissor(d.state.scissor, frame, width, height, wide);
                // A HUD piece moved to the edge takes its own scissor box along.
                let moved = shift.get(d.first as usize).copied().unwrap_or(0.0);
                if moved != 0.0 && w < width {
                    let px = (moved * width as f32 / 2.0).round() as i64;
                    x = (x as i64 + px).clamp(0, (width - w) as i64) as u32;
                }
                if w == 0 || h == 0 {
                    continue;
                }
                pass.set_scissor_rect(x, y, w, h);
                pass.set_pipeline(&self.pipelines[key]);
                let base = u64::from(bases[i].unwrap_or(0));
                pass.set_bind_group(
                    0,
                    &ubind,
                    &[
                        (i as u64 * UNIFORM_STRIDE) as u32,
                        (base * TRANSFORM_BYTES) as u32,
                    ],
                );
                pass.set_bind_group(1, bind, &[]);
                pass.draw(d.first..d.first + d.count, 0..1);
            }
        }

        self.vertex_scratch = data.vertices;
        self.stats.frames += 1;
        self.stats.draws += draws.len() as u64;
        self.stats.record_us += t0.elapsed().as_micros() as u64;
        (target, encoder)
    }

    /// The pipeline (made if new) and texture binding of each of `draws`,
    /// with `views` the frame's uploaded textures.
    fn prepare(
        &mut self,
        frame: &Frame,
        draws: &[&crate::Draw],
        views: &[(Option<u64>, wgpu::TextureView)],
    ) -> Vec<(PipeKey, wgpu::BindGroup)> {
        let mut prepared = Vec::with_capacity(draws.len());
        for d in draws {
            let mut key = pipe_key(&d.state);
            // Two textures blended (a stretched base under a fine detail
            // layer, as the games' floors are) only look right filtered as
            // the console does: kept crisp, the base shows as hard blocks.
            key.shader.sharp = self.sharp
                && !crate::combiner::Combiner::from_raw(d.state.combine)
                    .reads_texel1(key.shader.two_cycle);
            self.pipeline(key);
            let linear = cycle_type(d.state.other_h) != G_CYC_COPY
                && (d.state.other_h >> G_MDSFT_TEXTFILT) & 3 != 0
                && (self.sharp || !d.state.texel_exact);
            let mut entries = Vec::new();
            let mut bkey: BindKey = ([None; 2], linear);
            let mut keyed = true;
            for (k, slot) in d.state.textures.iter().enumerate() {
                let (view, wrap) = match slot.and_then(|i| Some((views.get(i as usize)?, i))) {
                    Some(((content, v), i)) => {
                        let wrap = frame.textures[i as usize].wrap;
                        bkey.0[k] = content.map(|c| (c, wrap));
                        keyed &= content.is_some();
                        (v.clone(), wrap)
                    }
                    None => (self.white.clone(), [Wrap::Repeat; 2]),
                };
                entries.push((view, self.sampler(wrap, linear), k as u32 * 2));
            }
            if let Some(bind) = self.bindings.get(&bkey).filter(|_| keyed) {
                prepared.push((key, bind.clone()));
                continue;
            }
            let bind = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("textures"),
                layout: &self.texture_layout,
                entries: &entries
                    .iter()
                    .flat_map(|(v, s, b)| {
                        [
                            wgpu::BindGroupEntry {
                                binding: *b,
                                resource: wgpu::BindingResource::TextureView(v),
                            },
                            wgpu::BindGroupEntry {
                                binding: b + 1,
                                resource: wgpu::BindingResource::Sampler(s),
                            },
                        ]
                    })
                    .collect::<Vec<_>>(),
            });
            if keyed {
                if self.bindings.len() >= BINDINGS_MAX {
                    self.bindings.clear();
                }
                self.bindings.insert(bkey, bind.clone());
            }
            prepared.push((key, bind));
        }
        prepared
    }

    /// A view of each of `textures`, uploading only those not kept from
    /// an earlier picture.
    /// With the cache on, each view comes with its content key.
    fn upload_textures(
        &mut self,
        textures: &[crate::Texture],
    ) -> Vec<(Option<u64>, wgpu::TextureView)> {
        self.pictures += 1;
        let fresh = |stats: &mut Stats, t: &crate::Texture| {
            stats.textures_new += 1;
            stats.texture_bytes += t.rgba.len() as u64;
            upload(&self.device, &self.queue, t.width, t.height, &t.rgba)
        };
        let Some(kept) = self.uploaded.as_mut() else {
            return textures
                .iter()
                .map(|t| (None, fresh(&mut self.stats, t)))
                .collect();
        };
        let views = textures
            .iter()
            .map(|t| {
                let id = (t.rgba.as_ptr() as usize, t.rgba.len(), t.width, t.height);
                let known = self.known.entry(id).or_insert_with(|| Known {
                    _pixels: t.rgba.clone(),
                    key: content_key(t),
                    last_used: 0,
                });
                known.last_used = self.pictures;
                let key = known.key;
                let k = kept.entry(key).or_insert_with(|| Uploaded {
                    view: fresh(&mut self.stats, t),
                    bytes: t.rgba.len() as u64,
                    last_used: 0,
                });
                k.last_used = self.pictures;
                (Some(key), k.view.clone())
            })
            .collect();
        let now = self.pictures;
        self.known.retain(|_, k| k.last_used + KNOWN_PICTURES > now);
        let bytes: u64 = kept.values().map(|u| u.bytes).sum();
        if kept.len() > UPLOADED_MAX || bytes > UPLOADED_MAX_BYTES {
            kept.retain(|_, u| u.last_used == self.pictures);
            // Bindings hold views of what was dropped.
            self.bindings.clear();
        }
        views
    }
}

/// `data` written into the buffer kept in `slot`, made anew (at the next
/// power of two) only when it has grown too small or `reuse` is off. Safe
/// because every picture is submitted before the next one writes the
/// buffer again.
fn kept_buffer(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    slot: &mut Option<wgpu::Buffer>,
    data: &[u8],
    usage: wgpu::BufferUsages,
    reuse: bool,
) -> wgpu::Buffer {
    let size = (data.len() as u64).max(64).next_multiple_of(4);
    let buf = match slot.take() {
        Some(b) if b.size() >= size && reuse => b,
        _ => device.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: size.next_power_of_two(),
            usage: usage | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        }),
    };
    queue.write_buffer(&buf, 0, data);
    *slot = Some(buf.clone());
    buf
}

/// Whether pixels `x0..x1` span a frame `fw` wide, give or take the border
/// games often leave (a scissor of 10 to 310 in a 320-wide frame).
fn spans_width(x0: f32, x1: f32, fw: f32) -> bool {
    x0 <= fw * SPAN_START && x1 >= fw * SPAN_END
}

/// The scissor box scaled from framebuffer pixels to the target, clamped to
/// it, as x, y, width, height. With `wide` above 1 (widescreen) a box
/// spanning the whole frame width spans the whole target; others are
/// narrowed about the centre as the geometry is.
fn scissor(s: Option<[f32; 4]>, frame: &Frame, width: u32, height: u32, wide: f32) -> [u32; 4] {
    let [mut x0, y0, mut x1, y1] = s.unwrap_or([0.0, 0.0, f32::MAX, f32::MAX]);
    let fw = frame.width as f32;
    if wide > 1.0 {
        if spans_width(x0, x1, fw) {
            (x0, x1) = (0.0, f32::MAX);
        } else {
            let c = fw * wide / 2.0;
            (x0, x1) = (c + (x0 - fw / 2.0), c + (x1 - fw / 2.0));
        }
    }
    let sx = width as f32 / (fw * wide);
    let sy = height as f32 / frame.height() as f32;
    let px = |v: f32, scale: f32, max: u32| ((v * scale).round().max(0.0) as u32).min(max);
    let (x0, x1) = (px(x0, sx, width), px(x1, sx, width));
    let (y0, y1) = (px(y0, sy, height), px(y1, sy, height));
    [x0, y0, x1.saturating_sub(x0), y1.saturating_sub(y0)]
}

fn target_desc(size: wgpu::Extent3d) -> wgpu::TextureDescriptor<'static> {
    wgpu::TextureDescriptor {
        label: None,
        size,
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: COLOR_FORMAT,
        usage: wgpu::TextureUsages::empty(),
        view_formats: &[],
    }
}

fn upload(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    width: u32,
    height: u32,
    rgba: &[u8],
) -> wgpu::TextureView {
    // Smaller copies for textures seen from afar: drawn at a higher
    // resolution than the console's, distant ground would shimmer.
    let levels = 32 - width.max(height).max(1).leading_zeros();
    let tex = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("n64 texture"),
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        mip_level_count: levels,
        ..target_desc(wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        })
    });
    let (mut w, mut h) = (width, height);
    let mut level = std::borrow::Cow::Borrowed(rgba);
    for mip in 0..levels {
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                mip_level: mip,
                ..tex.as_image_copy()
            },
            &level,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(w * 4),
                rows_per_image: Some(h),
            },
            wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
        );
        let (next, nw, nh) = half(&level, w, h);
        (level, w, h) = (next.into(), nw, nh);
    }
    tex.create_view(&Default::default())
}

/// RGBA8 `w` x `h` averaged down to half size in each direction (at least
/// one pixel).
fn half(src: &[u8], w: u32, h: u32) -> (Vec<u8>, u32, u32) {
    let (nw, nh) = ((w / 2).max(1), (h / 2).max(1));
    let mut out = Vec::with_capacity((nw * nh * 4) as usize);
    for y in 0..nh {
        for x in 0..nw {
            for c in 0..4 {
                let px = |dx: u32, dy: u32| {
                    let (sx, sy) = ((2 * x + dx).min(w - 1), (2 * y + dy).min(h - 1));
                    u32::from(src[((sy * w + sx) * 4 + c) as usize])
                };
                out.push(((px(0, 0) + px(1, 0) + px(0, 1) + px(1, 1) + 2) / 4) as u8);
            }
        }
    }
    (out, nw, nh)
}

fn floats(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|f| f.to_le_bytes()).collect()
}

/// Per transform: model-view times projection times the viewport, mapping
/// object space to wgpu clip space (y up, depth 0 to 1).
fn clip_matrices(frame: &Frame, transforms: &[crate::Transform]) -> Vec<crate::Mat4> {
    let (w, h) = (frame.width as f32, frame.height() as f32);
    transforms
        .iter()
        .map(|t| {
            let [sx, sy] = t.viewport.scale;
            let [tx, ty] = t.viewport.trans;
            // N64 clip -> screen pixels -> wgpu clip, all multiplied by w.
            let vp = [
                [2.0 * sx / w, 0.0, 0.0, 0.0],
                [0.0, 2.0 * sy / h, 0.0, 0.0],
                [0.0, 0.0, 0.5, 0.0],
                [2.0 * tx / w - 1.0, 1.0 - 2.0 * ty / h, 0.5, 1.0],
            ];
            mat_mul(&mat_mul(&t.modelview, &t.projection), &vp)
        })
        .collect()
}

/// Per vertex, the pixel span (x0, x1) a background rectangle stretches
/// across in widescreen: rectangles spanning the whole frame (backgrounds)
/// stretch from edge to edge of the target, over the border the game left.
/// A background may also be a row of tiles: rectangles on the same lines,
/// edge to edge, that together span the frame. Empty when the target is
/// no wider than the frame.
fn stretched_backgrounds(frame: &Frame, wide: f32) -> Vec<Option<(f32, f32)>> {
    if wide <= 1.0 {
        return Vec::new(); // nothing stretches
    }
    let mut stretch = vec![None; frame.vertices.len()];
    let mut rows = HashMap::<(i32, i32), Vec<_>>::new();
    // Neighbouring rectangles with one state share a draw: look at each
    // one (two triangles, six vertices) on its own.
    let quads = frame
        .draws()
        // A pre-rendered picture has 3D drawn into it (a room around a
        // character): stretched, the two would part. It keeps its shape.
        .filter(|d| d.state.rect && !d.state.picture && wide > 1.0)
        .flat_map(|d| {
            let end = (d.first + d.count) as usize;
            (d.first as usize..end)
                .step_by(6)
                .map(move |q| q..(q + 6).min(end))
        });
    for range in quads {
        let bound = |k: usize, f: fn(f32, f32) -> f32, start: f32| {
            frame.vertices[range.clone()]
                .iter()
                .map(|v| v.pos[k])
                .fold(start, f)
        };
        let (x0, x1) = (bound(0, f32::min, f32::MAX), bound(0, f32::max, f32::MIN));
        let (y0, y1) = (bound(1, f32::min, f32::MAX), bound(1, f32::max, f32::MIN));
        let row = (y0.round() as i32, y1.round() as i32);
        rows.entry(row).or_default().push((x0, x1, range));
    }
    for mut row in rows.into_values() {
        row.sort_by(|a, b| a.0.total_cmp(&b.0));
        let edge_to_edge = row.windows(2).all(|w| w[1].0 <= w[0].1 + 1.0);
        let x0 = row[0].0;
        let x1 = row.iter().map(|r| r.1).fold(f32::MIN, f32::max);
        // Stricter than one rectangle: a menu grid (portraits from 25 to
        // 295 of 320) must keep its place under its cursor.
        let fw = frame.width as f32;
        if edge_to_edge && x0 <= fw * BG_ROW_START && x1 >= fw * BG_ROW_END {
            for (_, _, range) in row {
                stretch[range].fill(Some((x0, x1)));
            }
        }
    }
    stretch
}

/// Per vertex, whether it is part of a backdrop or overlay that widescreen
/// stretches like a background; `at` gives a vertex's clip position. Empty
/// when the target is no wider than the frame.
fn backdrops(frame: &Frame, wide: f32, at: impl Fn(&crate::Vertex) -> [f32; 4]) -> Vec<bool> {
    if wide <= 1.0 {
        return Vec::new(); // nothing is narrowed
    }
    // A backdrop drawn first (a sky made for the original view's width)
    // ends at the 4:3 edges: stretch it like a background. It is the
    // leading run of 3D draws, after nothing but fills, that each cover
    // nine tenths of the width.
    let mut backdrop = vec![false; frame.vertices.len()];
    let mut started = false;
    if wide > 1.0 {
        for d in frame
            .draws()
            .filter(|d| d.state.color_image == frame.color_image)
        {
            // Fills (a clear) come before a backdrop; any other
            // rectangle (a background picture) is the backdrop itself,
            // and 3D after it is the scene.
            if d.state.rect {
                let fill =
                    cycle_type(d.state.other_h) == G_CYC_FILL && d.state.textures[0].is_none();
                // Only before the backdrop starts: a fill after it is
                // part of the scene.
                if fill && !started {
                    continue;
                }
                break;
            }
            started = true;
            let range = vertex_range(d);
            let (mut x0, mut x1) = (1.0f32, -1.0f32);
            for v in &frame.vertices[range.clone()] {
                let p = at(v);
                if p[3] > 0.0 {
                    x0 = x0.min(p[0] / p[3]);
                    x1 = x1.max(p[0] / p[3]);
                }
            }
            if x1.min(1.0) - x0.max(-1.0) < BACKDROP_MIN_SPAN {
                break;
            }
            backdrop[range].fill(true);
        }
    }
    // A flat overlay facing the camera and covering the whole view (a
    // fade or a flash drawn as triangles) covers the wide view too.
    if wide > 1.0 {
        for d in frame
            .draws()
            .filter(|d| !d.state.rect && d.state.color_image == frame.color_image)
        {
            let range = vertex_range(d);
            let Some(w) = frame.vertices[range.clone()].first().map(|v| at(v)[3]) else {
                continue;
            };
            // Stop at the first vertex off the plane: most 3D draws are
            // not overlays, and this keeps their vertices off the CPU.
            let mut ps = Vec::with_capacity(range.len());
            let flat = w > 0.0
                && frame.vertices[range.clone()].iter().all(|v| {
                    let p = at(v);
                    ps.push(p);
                    (p[3] - w).abs() <= w * OVERLAY_FLAT_W
                });
            let ext = |k: usize, f: fn(f32, f32) -> f32, s: f32| {
                ps.iter().map(|p| p[k] / p[3]).fold(s, f)
            };
            let covers = |k| {
                ext(k, f32::min, f32::MAX) <= -OVERLAY_EDGE
                    && ext(k, f32::max, f32::MIN) >= OVERLAY_EDGE
            };
            if flat && covers(0) && covers(1) {
                backdrop[range].fill(true);
            }
        }
    }
    backdrop
}

/// The fog multiplier and offset of `d` when fog is on: its vertices'
/// shade alpha becomes their fog factor, from their depth, as the
/// microcode computes it.
fn draw_fog(d: &crate::Draw) -> Option<[f32; 2]> {
    (!d.state.rect && d.state.geometry & G_FOG != 0).then_some(d.state.fog_factor)
}

/// The vertex stage's inputs for one picture.
struct VertexData {
    /// The vertex buffer (see [`vertex_bytes`]).
    vertices: Vec<u8>,
    /// The transform window's buffer (see [`transform_uniforms`]).
    transforms: Vec<u8>,
    /// Per draw, the first transform of its window; `None` for a draw
    /// moved on the CPU.
    bases: Vec<Option<u32>>,
    /// Per vertex, its HUD shift (see [`hud_shift`]), which also moves
    /// the draw's scissor box; empty when nothing moves.
    shift: Vec<f32>,
}

/// Everything the vertex stage needs to draw `draws` of `frame` with
/// `transforms`, `wide` times wider than the frame. With `gpu` the vertex
/// shader moves the vertices of every draw whose transforms fit in one
/// `window`; the rest (all of them without `gpu`) are moved here. The
/// vertex buffer is written into `scratch`, kept from the last picture.
///
/// Backdrops, overlays, stretched backgrounds and HUD pieces are found
/// here either way: each needs whole draws or triangles.
///
/// The shader's clip positions may differ from the CPU's in the last bit
/// (fused multiply-adds), so the same geometry drawn once by a draw moved
/// here and once by a draw moved in the shader can disagree in depth by
/// that much. Draws moved the same way agree (`@invariant` position).
fn vertex_data(
    frame: &Frame,
    transforms: &[crate::Transform],
    draws: &[&crate::Draw],
    wide: f32,
    (window, gpu): (TransformWindow, bool),
    scratch: Vec<u8>,
) -> VertexData {
    let clip = clip_matrices(frame, transforms);
    let flat = flat_transforms(transforms);
    let at = |v: &crate::Vertex| clip_position(&clip, &flat, v);
    let shift = hud_shift(frame, transforms, &clip, wide);
    let place = Placement {
        stretch: stretched_backgrounds(frame, wide),
        backdrop: backdrops(frame, wide, at),
        shift: &shift,
        wide,
    };
    let bases = if gpu {
        window_bases(frame, draws, transforms.len(), window)
    } else {
        vec![None; draws.len()]
    };
    let mut vertices = scratch;
    let raw = |d: usize, v: &crate::Vertex| {
        bases[d].is_some() && (v.transform as usize) < transforms.len()
    };
    vertex_bytes(frame, draws, raw, at, &place, &mut vertices);
    VertexData {
        vertices,
        transforms: transform_uniforms(&clip, &flat, window.len, &bases),
        bases,
        shift,
    }
}

/// Per transform, whether it is flat (orthographic). Flat pieces get their
/// depth clamped, not clipped: a HUD piece in front of the near plane
/// still shows. Perspective geometry there stays clipped.
fn flat_transforms(transforms: &[crate::Transform]) -> Vec<bool> {
    transforms
        .iter()
        .map(|t| {
            t.projection[0][3] == 0.0 && t.projection[1][3] == 0.0 && t.projection[2][3] == 0.0
        })
        .collect()
}

/// The clip position of `v` with the frame's `clip` matrices, depth
/// clamped for `flat` ones; zero for a vertex without a transform.
fn clip_position(clip: &[crate::Mat4], flat: &[bool], v: &crate::Vertex) -> [f32; 4] {
    let mut p = clip
        .get(v.transform as usize)
        .map_or([0.0; 4], |m| transform(m, v.pos));
    if flat.get(v.transform as usize) == Some(&true) && p[3] > 0.0 {
        p[2] = p[2].clamp(0.0, p[3]);
    }
    p
}

/// What moves each vertex after its transform in widescreen, decided on
/// the CPU from whole draws or triangles. Each list is per vertex, or
/// empty when it moves none.
struct Placement<'a> {
    /// Background rectangles stretched across a pixel span.
    stretch: Vec<Option<(f32, f32)>>,
    /// Backdrops and overlays, kept at full width.
    backdrop: Vec<bool>,
    /// HUD shifts (see [`hud_shift`]).
    shift: &'a [f32],
    /// How many times wider than the frame the target is.
    wide: f32,
}

impl Placement<'_> {
    fn stretch(&self, i: usize) -> Option<(f32, f32)> {
        self.stretch.get(i).copied().flatten()
    }

    fn backdrop(&self, i: usize) -> bool {
        self.backdrop.get(i) == Some(&true)
    }

    fn shift(&self, i: usize) -> f32 {
        self.shift.get(i).copied().unwrap_or(0.0)
    }
}

/// One vertex as the vertex buffer holds it: position, colour, two
/// texture coordinates, extra, then transform index and flags.
type VertexWords = ([f32; VERTEX_FLOATS], [u32; VERTEX_WORDS]);

/// Vertex `i` (`v`, at clip position `p`, with the draw's `fog`) moved on
/// the CPU, as the vertex shader moves a raw one.
fn cpu_vertex(
    v: &crate::Vertex,
    mut p: [f32; 4],
    fog: Option<[f32; 2]>,
    place: &Placement,
    i: usize,
) -> VertexWords {
    let mut color = v.color;
    if let Some([mul, off]) = fog {
        if p[3] > 0.0 {
            // Back from wgpu's 0 to 1 depth to the console's -1 to 1.
            let z = 2.0 * p[2] / p[3] - 1.0;
            color[3] = (z * mul + off).clamp(0.0, 255.0) / 255.0;
        }
    }
    match place.stretch(i) {
        // Rectangles are in pixels with w = 1.
        Some((x0, x1)) => p[0] = 2.0 * (v.pos[0] - x0) / (x1 - x0) - 1.0,
        None if place.backdrop(i) => {}
        None => p[0] = p[0] / place.wide + place.shift(i) * p[3],
    }
    let [r, g, b, a] = color;
    let ([s0, t0], [s1, t1]) = (v.uv[0], v.uv[1]);
    let floats = [
        p[0], p[1], p[2], p[3], r, g, b, a, s0, t0, s1, t1, 0.0, 0.0, 0.0, 0.0,
    ];
    (floats, [0, 0])
}

/// Vertex `i` (`v`, with the draw's `fog`) for the vertex shader to move:
/// its object position, and the fog and widescreen step in `extra`.
fn raw_vertex(
    v: &crate::Vertex,
    fog: Option<[f32; 2]>,
    place: &Placement,
    i: usize,
) -> VertexWords {
    let mut flags = VF_RAW;
    let [mul, off] = fog.unwrap_or_default();
    if fog.is_some() {
        flags |= VF_FOG;
    }
    let [x_a, x_b] = match place.stretch(i) {
        Some((x0, x1)) => {
            flags |= VF_STRETCH;
            [x0, x1]
        }
        None => {
            if place.backdrop(i) {
                flags |= VF_BACKDROP;
            }
            [place.shift(i), 0.0]
        }
    };
    let ([x, y, z], [r, g, b, a]) = (v.pos, v.color);
    let ([s0, t0], [s1, t1]) = (v.uv[0], v.uv[1]);
    let floats = [x, y, z, 1.0, r, g, b, a, s0, t0, s1, t1, mul, off, x_a, x_b];
    (floats, [v.transform, flags])
}

/// Per draw, the first transform of its window when the vertex shader can
/// move its vertices: when every transform they use (of the `transforms`
/// there are) fits in one `window`.
fn window_bases(
    frame: &Frame,
    draws: &[&crate::Draw],
    transforms: usize,
    window: TransformWindow,
) -> Vec<Option<u32>> {
    draws
        .iter()
        .map(|d| {
            let (lo, hi) = frame.vertices[vertex_range(d)]
                .iter()
                .map(|v| v.transform)
                .filter(|&t| (t as usize) < transforms)
                .fold((u32::MAX, 0), |(lo, hi), t| (lo.min(t), hi.max(t)));
            window.base(lo, hi)
        })
        .collect()
}

/// Writes the vertex buffer into `out`: each vertex of `draws` as
/// [`VERTEX_BYTES`] bytes at its index, the others zero. Those that
/// `raw(draw, vertex)` picks carry their object position and what the
/// vertex shader needs to move them; the others are moved here, `at`
/// giving their clip position.
fn vertex_bytes(
    frame: &Frame,
    draws: &[&crate::Draw],
    raw: impl Fn(usize, &crate::Vertex) -> bool,
    at: impl Fn(&crate::Vertex) -> [f32; 4],
    place: &Placement,
    out: &mut Vec<u8>,
) {
    let size = VERTEX_BYTES as usize;
    out.clear();
    out.resize(frame.vertices.len() * size, 0);
    for (k, d) in draws.iter().enumerate() {
        let fog = draw_fog(d);
        for i in vertex_range(d) {
            let v = &frame.vertices[i];
            let (floats, words) = if raw(k, v) {
                raw_vertex(v, fog, place, i)
            } else {
                cpu_vertex(v, at(v), fog, place, i)
            };
            let (fs, ws) = out[i * size..(i + 1) * size].split_at_mut(VERTEX_FLOATS * 4);
            put_le(fs, floats.map(f32::to_le_bytes));
            put_le(ws, words.map(u32::to_le_bytes));
        }
    }
}

/// Writes `words` (little-endian bytes) into `dst` in turn.
fn put_le(dst: &mut [u8], words: impl IntoIterator<Item = [u8; 4]>) {
    for (d, w) in dst.as_chunks_mut::<4>().0.iter_mut().zip(words) {
        *d = w;
    }
}

/// The transform window's buffer: each clip matrix (rows as the shader's
/// columns) and whether it is flat, [`TRANSFORM_BYTES`] apart, up to the
/// end of the last window that `bases` (per draw) starts, and at least one
/// whole window of `window`. Only that one window, zero, when no draw
/// reads it.
fn transform_uniforms(
    clip: &[crate::Mat4],
    flat: &[bool],
    window: u32,
    bases: &[Option<u32>],
) -> Vec<u8> {
    let end = bases.iter().flatten().max().map_or(0, |&b| b + window);
    let rows = end.max(window) as usize;
    let mut out = vec![0u8; rows * TRANSFORM_BYTES as usize];
    let chunks = out.as_chunks_mut::<{ TRANSFORM_BYTES as usize }>().0;
    for ((m, &flat), dst) in clip.iter().zip(flat).take(end as usize).zip(chunks) {
        let flat = [f32::from(u8::from(flat)), 0.0, 0.0, 0.0];
        let vals = m.iter().chain([&flat]).flatten();
        put_le(dst, vals.map(|f| f.to_le_bytes()));
    }
    out
}

/// The uniforms of each of `draws`, one [`UNIFORM_STRIDE`] apart, with
/// the widescreen narrowing `wide` and each draw's first transform in
/// its window (`bases`) for the vertex stage.
fn draw_uniforms(draws: &[&crate::Draw], wide: f32, bases: &[Option<u32>]) -> Vec<u8> {
    let mut uniforms = vec![0u8; (draws.len().max(1) as u64 * UNIFORM_STRIDE) as usize];
    for (i, d) in draws.iter().enumerate() {
        let s = &d.state;
        // Exact as a float: transform counts stay far below 2^24.
        let base = bases.get(i).copied().flatten().unwrap_or(0) as f32;
        let misc = [s.prim_lod, s.k5, wide, base];
        let vals: Vec<f32> = [s.prim, s.env, s.blend, misc, s.fog].concat();
        let at = i * UNIFORM_STRIDE as usize;
        uniforms[at..at + UNIFORM_SIZE as usize].copy_from_slice(&floats(&vals));
    }
    uniforms
}

/// The vertices of `d` in [`Frame::vertices`].
fn vertex_range(d: &crate::Draw) -> std::ops::Range<usize> {
    d.first as usize..(d.first + d.count) as usize
}

/// A texture binding's layout entry, at `binding`.
fn texture_layout_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Float { filterable: true },
            view_dimension: wgpu::TextureViewDimension::D2,
            multisampled: false,
        },
        count: None,
    }
}

/// A filtering sampler's layout entry, at `binding`.
fn sampler_layout_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
        count: None,
    }
}

/// Clip-space x shift of each vertex for the frame's HUD areas (see
/// [`Frame::hud`]): `side * (1 - 1 / wide)` for the triangles moved.
/// Empty when nothing moves.
///
/// A 3D piece drawn in a small viewport of its own (a spinning button)
/// counts by its viewport. Pieces on a panel stay: a flat quad at least
/// 100 pixels wide that no area holds (a dialog box) keeps the text drawn
/// on it together, even where the text runs into an area.
fn hud_shift(
    frame: &Frame,
    transforms: &[crate::Transform],
    clip: &[crate::Mat4],
    wide: f32,
) -> Vec<f32> {
    if wide <= 1.0 || frame.hud.is_empty() {
        return Vec::new(); // nothing moves
    }
    let mut shift = vec![0.0f32; frame.vertices.len()];
    let (fw, fh) = (frame.width as f32, frame.height() as f32);
    // Flat (screen-space) triangles with their pixel bounds.
    let mut tris = Vec::new();
    for d in frame
        .draws()
        .filter(|d| d.state.color_image == frame.color_image)
    {
        let end = (d.first + d.count) as usize;
        'tri: for tri in (d.first as usize..end).step_by(3) {
            let range = tri..(tri + 3).min(end);
            let mut b = [f32::MAX, f32::MAX, f32::MIN, f32::MIN];
            for v in &frame.vertices[range.clone()] {
                let (Some(m), Some(t)) = (
                    clip.get(v.transform as usize),
                    transforms.get(v.transform as usize),
                ) else {
                    continue 'tri;
                };
                if m[0][3] != 0.0 || m[1][3] != 0.0 || m[2][3] != 0.0 {
                    let ([sx, sy], [tx, ty]) = (t.viewport.scale, t.viewport.trans);
                    if sx.abs() * 2.0 >= fw * HUD_3D_VIEWPORT {
                        continue 'tri; // the 3D scene
                    }
                    let vb = [tx - sx.abs(), ty - sy.abs(), tx + sx.abs(), ty + sy.abs()];
                    b = [
                        b[0].min(vb[0]),
                        b[1].min(vb[1]),
                        b[2].max(vb[2]),
                        b[3].max(vb[3]),
                    ];
                    continue;
                }
                let p = transform(m, v.pos);
                let (x, y) = (
                    (p[0] / p[3] + 1.0) / 2.0 * fw,
                    (1.0 - p[1] / p[3]) / 2.0 * fh,
                );
                b = [b[0].min(x), b[1].min(y), b[2].max(x), b[3].max(y)];
            }
            tris.push((range, b));
        }
    }
    let within = |b: &[f32; 4], r: &[f32; 4]| {
        b[0] >= r[0] - 0.5 && b[1] >= r[1] - 0.5 && b[2] <= r[2] + 0.5 && b[3] <= r[3] + 0.5
    };
    let area = |r: &[f32; 5]| [r[0], r[1], r[2], r[3]];
    let panels: Vec<[f32; 4]> = tris
        .iter()
        .map(|t| t.1)
        .filter(|b| b[2] - b[0] >= PANEL_MIN_PX && b[2] - b[0] < fw * PANEL_MAX_WIDTH)
        .filter(|b| !frame.hud.iter().any(|r| within(b, &area(r))))
        .collect();
    for (range, b) in tris {
        if panels.iter().any(|p| within(&b, p)) {
            continue;
        }
        if let Some(r) = frame.hud.iter().find(|r| within(&b, &area(r))) {
            shift[range].fill(r[4] * (1.0 - 1.0 / wide));
        }
    }
    shift
}

fn transform(m: &crate::Mat4, p: [f32; 3]) -> [f32; 4] {
    let v = [p[0], p[1], p[2], 1.0];
    [0, 1, 2, 3].map(|j| (0..4).map(|k| v[k] * m[k][j]).sum())
}

/// The largest rectangle with the frame's shape that fits a `win_w` x
/// `win_h` window, centred: `(x, y, width, height)`.
pub fn fit(win_w: u32, win_h: u32, frame_w: u32, frame_h: u32) -> (u32, u32, u32, u32) {
    let (fw, fh) = (frame_w.max(1) as u64, frame_h.max(1) as u64);
    let (w, h) = if win_w as u64 * fh <= win_h as u64 * fw {
        (win_w as u64, win_w as u64 * fh / fw)
    } else {
        (win_h as u64 * fw / fh, win_h as u64)
    };
    let (w, h) = (w.max(1) as u32, h.max(1) as u32);
    ((win_w - w.min(win_w)) / 2, (win_h - h.min(win_h)) / 2, w, h)
}

/// Copies a drawn frame into a part of another texture.
const BLIT_SHADER: &str = r#"
@group(0) @binding(0) var tex: texture_2d<f32>;
@group(0) @binding(1) var smp: sampler;
struct Out { @builtin(position) pos: vec4<f32>, @location(0) uv: vec2<f32> };
@vertex fn vs(@builtin(vertex_index) i: u32) -> Out {
    // One triangle covering the viewport.
    let uv = vec2<f32>(f32((i << 1u) & 2u), f32(i & 2u));
    var o: Out;
    o.pos = vec4<f32>(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0, 0.0, 1.0);
    o.uv = uv;
    return o;
}
@fragment fn fs(o: Out) -> @location(0) vec4<f32> {
    return vec4<f32>(textureSample(tex, smp, o.uv).rgb, 1.0);
}
"#;

/// Draws frames into a texture of another format, letterboxed.
struct Blit {
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
}

impl Blit {
    fn new(device: &wgpu::Device, format: wgpu::TextureFormat) -> Blit {
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("blit"),
            entries: &[texture_layout_entry(0), sampler_layout_entry(1)],
        });
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("blit"),
            source: wgpu::ShaderSource::Wgsl(BLIT_SHADER.into()),
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("blit"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("blit"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vs"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some("fs"),
                compilation_options: Default::default(),
                targets: &[Some(format.into())],
            }),
            primitive: Default::default(),
            depth_stencil: None,
            multisample: Default::default(),
            multiview_mask: None,
            cache: None,
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor::default());
        Blit {
            pipeline,
            layout,
            sampler,
        }
    }

    /// Records a copy of `src` into `dst`, sized to fit it with the
    /// source's shape and centred, black elsewhere.
    fn record(
        &self,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        src: &wgpu::Texture,
        dst: &wgpu::TextureView,
        (x, y, w, h): (u32, u32, u32, u32),
    ) {
        let view = src.create_view(&Default::default());
        let bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("blit"),
            layout: &self.layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
            ],
        });
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("blit"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: dst,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        pass.set_viewport(x as f32, y as f32, w as f32, h as f32, 0.0, 1.0);
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &bind, &[]);
        pass.draw(0..3, 0..1);
    }
}

/// A window the renderer draws finished frames into, at the window's own
/// resolution, keeping the frame's shape with black bars.
pub struct WindowTarget {
    surface: wgpu::Surface<'static>,
    format: wgpu::TextureFormat,
    /// The surface format without sRGB encoding: frame colours are already
    /// display values.
    view_format: wgpu::TextureFormat,
    present_mode: wgpu::PresentMode,
    /// The present modes the window offers.
    offers: Vec<wgpu::PresentMode>,
    alpha: wgpu::CompositeAlphaMode,
    size: Option<(u32, u32)>,
    blit: Option<Blit>,
}

/// How a window shows its pictures, from the modes it `offers`. Mailbox
/// (when `mailbox` is wanted and offered) shows the newest picture without
/// waiting for the screen; Fifo, offered everywhere, shows each picture
/// in turn at the screen's pace, which lets a phone's display and GPU rest
/// between pictures. The game keeps its own pace either way.
fn present_mode(offers: &[wgpu::PresentMode], mailbox: bool) -> wgpu::PresentMode {
    if mailbox && offers.contains(&wgpu::PresentMode::Mailbox) {
        wgpu::PresentMode::Mailbox
    } else {
        wgpu::PresentMode::Fifo
    }
}

impl WindowTarget {
    /// Chooses whether pictures wait for the screen (Fifo) or the newest
    /// one replaces any still waiting (Mailbox, where offered). Waiting
    /// blocks the drawing thread when pictures come faster than the screen
    /// shows them, so ask for it only when they do not. Takes effect at the
    /// next picture.
    pub fn set_wait_for_screen(&mut self, wait: bool) {
        let mode = present_mode(&self.offers, !wait);
        if mode != self.present_mode {
            self.present_mode = mode;
            self.size = None; // configure again next picture
        }
    }
}

impl Renderer {
    /// Prepares to draw into a window (any window handle, for example an
    /// `Arc` of a winit window).
    pub fn window_target(
        &self,
        window: impl Into<wgpu::SurfaceTarget<'static>>,
    ) -> Result<WindowTarget, RenderError> {
        let (instance, adapter) = gpu()?;
        let surface = instance
            .create_surface(window)
            .map_err(|e| RenderError::Device(e.to_string()))?;
        let caps = surface.get_capabilities(adapter);
        let format = *caps.formats.first().ok_or_else(|| {
            RenderError::Device("the graphics adapter cannot draw into this window".into())
        })?;
        // Mailbox where offered, phones too: the newest picture wins, and
        // pictures are only drawn when they change, so it draws no more often.
        let present_mode = present_mode(&caps.present_modes, true);
        Ok(WindowTarget {
            surface,
            format,
            view_format: format.remove_srgb_suffix(),
            present_mode,
            offers: caps.present_modes.clone(),
            alpha: caps
                .alpha_modes
                .first()
                .copied()
                .unwrap_or(wgpu::CompositeAlphaMode::Auto),
            size: None,
            blit: None,
        })
    }

    /// Draws `frame` into a window whose inner size is `width` x `height`.
    /// A frame the window cannot take right now (minimised, being resized)
    /// is skipped.
    pub fn present(
        &mut self,
        frame: &Frame,
        win: &mut WindowTarget,
        width: u32,
        height: u32,
    ) -> Result<(), RenderError> {
        self.present_with(frame, &frame.transforms, win, width, height)
    }

    /// Draws a picture `t` of the way (0 to 1) from game frame `prev` to
    /// game frame `cur`, for refresh rates above the game's: `cur` with
    /// its matrices blended from `prev` (see [`crate::interpolate`]).
    pub fn present_between(
        &mut self,
        prev: &Frame,
        cur: &Frame,
        t: f32,
        win: &mut WindowTarget,
        width: u32,
        height: u32,
    ) -> Result<(), RenderError> {
        let transforms = crate::interpolate(prev, cur, t);
        self.present_with(cur, &transforms, win, width, height)
    }

    fn present_with(
        &mut self,
        frame: &Frame,
        transforms: &[crate::Transform],
        win: &mut WindowTarget,
        width: u32,
        height: u32,
    ) -> Result<(), RenderError> {
        let Some(texture) = self.surface_texture(win, width, height) else {
            return Ok(());
        };
        let blit = win
            .blit
            .get_or_insert_with(|| Blit::new(&self.device, win.view_format));
        let view = texture.texture.create_view(&wgpu::TextureViewDescriptor {
            format: Some(win.view_format),
            ..Default::default()
        });
        let rect = self.place(frame, width, height);
        let (w, h) = capped(rect.2, rect.3, self.max_height);
        let (src, mut encoder) = self.draw(frame, transforms, w, h);
        blit.record(&self.device, &mut encoder, &src, &view, rect);
        let t0 = std::time::Instant::now();
        self.queue.submit([encoder.finish()]);
        self.queue.present(texture);
        self.stats.submit_us += t0.elapsed().as_micros() as u64;
        Ok(())
    }

    /// Shows `w` x `h` pixels (RGBA bytes) in a window of `width` x
    /// `height` at `rect` (x, y, width, height), black elsewhere: a screen
    /// drawn on the CPU, where the window must stay the GPU's (Android).
    #[allow(clippy::too_many_arguments)]
    pub fn present_pixels(
        &mut self,
        rgba: &[u8],
        (w, h): (u32, u32),
        win: &mut WindowTarget,
        width: u32,
        height: u32,
        rect: (u32, u32, u32, u32),
    ) -> Result<(), RenderError> {
        self.check_pixels(rgba, (w, h), rect)?;
        let Some(texture) = self.surface_texture(win, width, height) else {
            return Ok(());
        };
        let size = wgpu::Extent3d {
            width: w,
            height: h,
            depth_or_array_layers: 1,
        };
        let src = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("pixels"),
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            ..target_desc(size)
        });
        self.queue.write_texture(
            src.as_image_copy(),
            rgba,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(4 * w),
                rows_per_image: Some(h),
            },
            size,
        );
        let blit = win
            .blit
            .get_or_insert_with(|| Blit::new(&self.device, win.view_format));
        let view = texture.texture.create_view(&wgpu::TextureViewDescriptor {
            format: Some(win.view_format),
            ..Default::default()
        });
        let mut encoder = self.device.create_command_encoder(&Default::default());
        blit.record(&self.device, &mut encoder, &src, &view, rect);
        self.queue.submit([encoder.finish()]);
        self.queue.present(texture);
        Ok(())
    }

    /// An error unless `rgba` holds exactly `w` x `h` pixels the device can
    /// take, and `rect` is a place wgpu accepts to show them.
    fn check_pixels(
        &self,
        rgba: &[u8],
        (w, h): (u32, u32),
        (x, y, rw, rh): (u32, u32, u32, u32),
    ) -> Result<(), RenderError> {
        self.check_size("picture", w, h)?;
        let want = u64::from(w) * u64::from(h) * BYTES_PER_PIXEL;
        if rgba.len() as u64 != want {
            return Err(RenderError::Size(format!(
                "a {w} x {h} picture has {} bytes, not {want}",
                rgba.len()
            )));
        }
        let max = u64::from(self.device.limits().max_texture_dimension_2d);
        // wgpu's set_viewport: each side at most the largest texture, and
        // the far edge below twice that.
        let far = |at: u32, len: u32| u64::from(at) + u64::from(len) >= 2 * max;
        if u64::from(rw) > max || u64::from(rh) > max || far(x, rw) || far(y, rh) {
            return Err(RenderError::Size(format!(
                "place {rw} x {rh} at ({x}, {y}): sides must be at most {max}, edges below {}",
                2 * max
            )));
        }
        Ok(())
    }

    /// The window's next picture to draw into, at `width` x `height`; none
    /// while the window cannot take one (minimised, being resized).
    fn surface_texture(
        &self,
        win: &mut WindowTarget,
        width: u32,
        height: u32,
    ) -> Option<wgpu::SurfaceTexture> {
        let max = self.device.limits().max_texture_dimension_2d;
        if width == 0 || height == 0 || width > max || height > max {
            return None;
        }
        if win.size != Some((width, height)) {
            win.surface.configure(
                &self.device,
                &wgpu::SurfaceConfiguration {
                    usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                    format: win.format,
                    color_space: Default::default(),
                    width,
                    height,
                    present_mode: win.present_mode,
                    desired_maximum_frame_latency: 2,
                    alpha_mode: win.alpha,
                    view_formats: vec![win.view_format],
                },
            );
            win.size = Some((width, height));
        }
        match win.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(t) => Some(t),
            wgpu::CurrentSurfaceTexture::Suboptimal(t) => {
                win.size = None; // configure again next frame
                Some(t)
            }
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                win.size = None;
                None
            }
            _ => None,
        }
    }

    /// Like [`Renderer::present`], into an off-screen image of `width` x
    /// `height`, for tests of the window path.
    pub fn render_letterboxed(
        &mut self,
        frame: &Frame,
        width: u32,
        height: u32,
    ) -> Result<Image, RenderError> {
        self.letterboxed_with(frame, &frame.transforms, width, height)
    }

    /// Like [`Renderer::present_between`], into an off-screen image, for
    /// tests.
    pub fn render_between(
        &mut self,
        prev: &Frame,
        cur: &Frame,
        t: f32,
        width: u32,
        height: u32,
    ) -> Result<Image, RenderError> {
        let transforms = crate::interpolate(prev, cur, t);
        self.letterboxed_with(cur, &transforms, width, height)
    }

    fn letterboxed_with(
        &mut self,
        frame: &Frame,
        transforms: &[crate::Transform],
        width: u32,
        height: u32,
    ) -> Result<Image, RenderError> {
        self.check_size("image", width, height)?;
        let dst = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("letterboxed"),
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            ..target_desc(wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            })
        });
        let blit = Blit::new(&self.device, COLOR_FORMAT);
        let rect = self.place(frame, width, height);
        let (w, h) = capped(rect.2, rect.3, self.max_height);
        let (src, mut encoder) = self.draw(frame, transforms, w, h);
        blit.record(
            &self.device,
            &mut encoder,
            &src,
            &dst.create_view(&Default::default()),
            rect,
        );
        self.read_back(encoder, &dst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn present_mode_is_mailbox_only_when_wanted_and_offered() {
        use wgpu::PresentMode::{Fifo, Immediate, Mailbox};
        assert_eq!(present_mode(&[Fifo, Mailbox], true), Mailbox);
        assert_eq!(present_mode(&[Fifo, Mailbox], false), Fifo);
        assert_eq!(present_mode(&[Fifo, Immediate], true), Fifo);
        assert_eq!(present_mode(&[], true), Fifo);
    }

    #[test]
    fn capped_keeps_shape_and_leaves_small_pictures_alone() {
        assert_eq!(capped(1920, 1440, None), (1920, 1440));
        assert_eq!(capped(1920, 1440, Some(1440)), (1920, 1440));
        assert_eq!(capped(640, 480, Some(720)), (640, 480));
        assert_eq!(capped(1920, 1440, Some(720)), (960, 720));
        // Rounded to the nearest pixel, never below one.
        assert_eq!(capped(2560, 1080, Some(480)), (1138, 480));
        assert_eq!(capped(1, 1000, Some(10)), (1, 10));
        assert_eq!(capped(800, 600, Some(0)), (800, 600));
    }

    fn state(other_l: u32) -> RenderState {
        RenderState {
            combine: 0,
            other_h: 0,
            other_l,
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
            color_width: 0,
            scissor: None,
        }
    }

    #[test]
    fn alpha_compare_keeps_texture_edge_test() {
        // Alpha compare on its own, texture edge on its own, then both.
        assert_eq!(pipe_key(&state(0x0001)).shader.alpha_test, 1);
        assert_eq!(pipe_key(&state(0x3000)).shader.alpha_test, 2);
        assert_eq!(pipe_key(&state(0x3049)).shader.alpha_test, 3);
    }

    #[test]
    fn pixels_to_show_must_match_their_size() {
        let r = match Renderer::new() {
            Ok(r) => r,
            Err(e) if std::env::var_os("N64RUSTY_REQUIRE_GPU").is_none() => {
                eprintln!("skipping: {e}");
                return;
            }
            Err(e) => panic!("{e}"),
        };
        let max = r.device.limits().max_texture_dimension_2d;
        let rgba = vec![0u8; 4 * 3 * BYTES_PER_PIXEL as usize];
        let ok = |px: &[u8], size, rect| r.check_pixels(px, size, rect).is_ok();
        assert!(ok(&rgba, (4, 3), (0, 0, 8, 6)));
        // Larger than the window, as a start screen in a small one is.
        assert!(ok(&rgba, (4, 3), (10, 10, max, max)));
        assert!(!ok(&rgba[1..], (4, 3), (0, 0, 8, 6)), "short");
        assert!(
            !ok(&[rgba.as_slice(), &[0; 4]].concat(), (4, 3), (0, 0, 8, 6)),
            "long"
        );
        assert!(!ok(&[], (0, 0), (0, 0, 8, 6)), "empty");
        assert!(!ok(&rgba, (4, 3), (0, 0, max + 1, 6)), "too wide");
        assert!(!ok(&rgba, (4, 3), (max, 0, max, 6)), "too far");
    }

    /// RGBA8 pixels of one grey level each, all four channels alike.
    fn greys(levels: &[u8]) -> Vec<u8> {
        levels.iter().flat_map(|&g| [g; 4]).collect()
    }

    #[test]
    fn half_of_one_pixel_is_that_pixel() {
        const PX: [u8; 4] = [10, 20, 30, 40];
        assert_eq!(half(&PX, 1, 1), (PX.to_vec(), 1, 1));
    }

    #[test]
    fn half_averages_each_channel_of_a_two_by_two_block_rounding_to_nearest() {
        let src = [[255, 0, 0, 255], [0, 255, 0, 255], [0, 0, 255, 0], [0; 4]].concat();
        // (255 + 2) / 4 = 64, (510 + 2) / 4 = 128.
        assert_eq!(half(&src, 2, 2), (vec![64, 64, 64, 128], 1, 1));
        // A quarter rounds down, a half rounds up.
        assert_eq!(half(&greys(&[0, 0, 0, 1]), 2, 2).0, greys(&[0]));
        assert_eq!(half(&greys(&[0, 0, 1, 1]), 2, 2).0, greys(&[1]));
    }

    #[test]
    fn half_of_an_odd_size_rounds_down_and_drops_the_last_row_and_column() {
        // Mip sizes are floor(size / 2), at least 1 (as OpenGL and wgpu
        // number them); each texel is the box average of its 2x2 block.
        #[rustfmt::skip]
        let src = greys(&[
            0, 40, 200,
            80, 120, 200,
            200, 200, 200,
        ]);
        assert_eq!(half(&src, 3, 3), (greys(&[60]), 1, 1));
    }

    #[test]
    fn half_of_a_one_pixel_wide_or_tall_image_averages_along_the_long_side() {
        let pairs = greys(&[20, 60]);
        let tall = greys(&[10, 30, 50, 70]);
        assert_eq!(half(&tall, 1, 4), (pairs.clone(), 1, 2));
        let wide = greys(&[10, 30, 50, 70, 250]);
        assert_eq!(half(&wide, 5, 1), (pairs, 2, 1));
    }

    #[test]
    fn halving_an_odd_image_down_to_one_pixel_never_panics() {
        let (mut level, mut w, mut h) = (vec![0x80u8; 7 * 3 * 4], 7, 3);
        let mut sizes = vec![(w, h)];
        while (w, h) != (1, 1) {
            (level, w, h) = half(&level, w, h);
            assert_eq!(level.len(), (w * h * 4) as usize);
            sizes.push((w, h));
        }
        assert_eq!(sizes, [(7, 3), (3, 1), (1, 1)]);
        assert_eq!(level, vec![0x80; 4]);
    }

    #[test]
    fn transform_window_fits_the_uniform_limits() {
        // The smallest limits wgpu allows (GLES): 204 transforms, first
        // ones 16 apart (16 * 80 bytes is a multiple of 256).
        let w = TransformWindow::from_limits(16384, 256).unwrap();
        assert_eq!(w, TransformWindow { len: 204, step: 16 });
        assert_eq!(
            TransformWindow::from_limits(65536, 256),
            Some(TransformWindow { len: 819, step: 16 })
        );
        // Capped, and a finer alignment gives a finer step.
        assert_eq!(
            TransformWindow::from_limits(1 << 20, 64),
            Some(TransformWindow { len: 1024, step: 4 })
        );
        assert_eq!(TransformWindow::from_limits(65536, 0).unwrap().step, 1);
        // Too small to be of use: every vertex moves on the CPU.
        assert_eq!(TransformWindow::from_limits(2000, 256), None);
        assert_eq!(TransformWindow::from_limits(0, 256), None);
        // A window must hold at least two steps.
        assert_eq!(TransformWindow::from_limits(40 * 80, 2048), None);
    }

    #[test]
    fn a_window_starts_at_a_step_and_holds_the_whole_draw() {
        let w = TransformWindow { len: 204, step: 16 };
        assert_eq!(w.base(17, 30), Some(16));
        assert_eq!(w.base(0, 0), Some(0));
        assert_eq!(w.base(16, 16 + 203), Some(16));
        assert_eq!(w.base(15, 204), None, "one past the window");
        assert_eq!(w.base(u32::MAX, 0), None, "no transforms");
        assert_eq!(
            w.base(u32::MAX - 1, u32::MAX - 1),
            Some((u32::MAX - 1) / 16 * 16)
        );
    }

    fn vertex(transform: u32) -> crate::Vertex {
        crate::Vertex {
            pos: [0.0; 3],
            transform,
            color: [1.0; 4],
            uv: [[0.0; 2]; 2],
        }
    }

    fn draw(first: u32, count: u32) -> crate::Draw {
        crate::Draw {
            first,
            count,
            state: state(0),
        }
    }

    #[test]
    fn window_bases_skip_missing_transforms_and_draws_too_wide() {
        let w = TransformWindow { len: 32, step: 16 };
        let f = Frame {
            width: 320,
            // Draw 0: transforms 3 and 40, too far apart for one window.
            // Draw 1: 20 and a missing one. Draw 2: only missing ones.
            vertices: [3, 40, 40, 20, 99, 20, 99, 99, 99].map(vertex).to_vec(),
            transforms: vec![transform_with(IDENTITY_M, ortho()); 50],
            ..Default::default()
        };
        let draws = [draw(0, 3), draw(3, 3), draw(6, 3)];
        let refs: Vec<_> = draws.iter().collect();
        let data = vertex_data(&f, &f.transforms, &refs, 1.0, (w, true), Vec::new());
        assert_eq!(data.bases, [None, Some(16), None]);
        // Only draw 1's vertices with a transform go to the shader.
        let flags: Vec<u32> = data
            .vertices
            .as_chunks::<{ VERTEX_BYTES as usize }>()
            .0
            .iter()
            .map(|v| u32::from_le_bytes(v[v.len() - 4..].try_into().unwrap()))
            .collect();
        assert_eq!(flags, [0, 0, 0, VF_RAW, 0, VF_RAW, 0, 0, 0]);
        // Without the shader none do.
        let cpu = vertex_data(&f, &f.transforms, &refs, 1.0, (w, false), Vec::new());
        assert_eq!(cpu.bases, [None; 3]);
        // The window buffer ends with the last window read; without the
        // shader it is one window of zeros, there only to bind.
        let row = TRANSFORM_BYTES as usize;
        assert_eq!(data.transforms.len(), (16 + 32) * row);
        assert_eq!(cpu.transforms.len(), 32 * row);
        assert!(cpu.transforms.iter().all(|&b| b == 0));
    }

    #[test]
    fn vertex_bytes_carry_raw_or_moved_vertices() {
        let mut f = Frame {
            width: 320,
            vertices: vec![vertex(0); 2],
            transforms: vec![transform_with(IDENTITY_M, ortho())],
            ..Default::default()
        };
        f.vertices[0].pos = [16.0, 12.0, 0.0];
        f.vertices[1].pos = [16.0, 12.0, 0.0];
        let clip = clip_matrices(&f, &f.transforms);
        let flat = flat_transforms(&f.transforms);
        let shift = vec![0.25, 0.25];
        let place = Placement {
            stretch: Vec::new(),
            backdrop: Vec::new(),
            shift: &shift,
            wide: 2.0,
        };
        // One vertex a draw, both fogged; the first for the shader.
        let mut fogged = [draw(0, 1), draw(1, 1)];
        for d in &mut fogged {
            d.state.geometry = G_FOG;
            d.state.fog_factor = [640.0, 128.0];
        }
        let at = |v: &crate::Vertex| clip_position(&clip, &flat, v);
        let mut bytes = vec![7; 3]; // a kept buffer is written anew
        let draws: Vec<_> = fogged.iter().collect();
        vertex_bytes(&f, &draws, |k, _| k == 0, at, &place, &mut bytes);
        assert_eq!(bytes.len(), 2 * VERTEX_BYTES as usize);
        let word = |v: usize, k: usize| {
            let at = v * VERTEX_BYTES as usize + k * 4;
            u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
        };
        let float = |v, k| f32::from_bits(word(v, k));
        // Raw: object position with w 1, fog and shift for the shader.
        assert_eq!([0, 1, 2, 3].map(|k| float(0, k)), [16.0, 12.0, 0.0, 1.0]);
        assert_eq!([12, 13, 14].map(|k| float(0, k)), [640.0, 128.0, 0.25]);
        assert_eq!([word(0, 16), word(0, 17)], [0, VF_RAW | VF_FOG]);
        // Moved here: x narrowed by 2 and shifted, fog alpha 128 / 255.
        let p = at(&f.vertices[1]);
        assert_eq!(float(1, 0), p[0] / 2.0 + 0.25 * p[3]);
        assert_eq!(float(1, 7), 128.0 / 255.0);
        assert_eq!([word(1, 16), word(1, 17)], [0, 0]);
    }

    // Parity of the vertex shader with the CPU path.

    const IDENTITY_M: crate::Mat4 = crate::IDENTITY;
    /// The combiner passing the shade colour.
    const SHADE: u64 = crate::Combiner::both([15, 15, 31, 4], [7, 7, 7, 4]).to_raw();
    const DEPTH: u32 = Z_CMP | Z_UPD;
    /// "IN * A + MEM * (1 - A)", forced: a see-through overlay.
    const TRANSLUCENT: u32 = FORCE_BL | (BL_MEM << 22);
    /// Two-cycle mode, and the fog blender's first cycle.
    const TWO_CYCLE: u32 = G_CYC_2CYCLE << 20;
    const FOG_SHADE_A: u32 = (BL_FOG << 30) | (BL_A_SHADE << 26);

    /// Deterministic numbers for synthetic scenes.
    struct Lcg(u32);

    impl Lcg {
        /// The next number from `lo` to `hi`.
        fn next(&mut self, lo: f32, hi: f32) -> f32 {
            self.0 = self.0.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            lo + (hi - lo) * ((self.0 >> 8) as f32 / (1u32 << 24) as f32)
        }
    }

    /// guPerspective-like, 4:3: w is minus the eye depth, near 10, far 1000.
    fn perspective() -> crate::Mat4 {
        let (n, f) = (10.0f32, 1000.0f32);
        [
            [0.75, 0.0, 0.0, 0.0],
            [0.0, 1.0, 0.0, 0.0],
            [0.0, 0.0, (n + f) / (n - f), -1.0],
            [0.0, 0.0, 2.0 * n * f / (n - f), 0.0],
        ]
    }

    /// Orthographic: x -160 to 160, y -120 to 120, z 100 (near) to -100.
    fn ortho() -> crate::Mat4 {
        [
            [1.0 / 160.0, 0.0, 0.0, 0.0],
            [0.0, 1.0 / 120.0, 0.0, 0.0],
            [0.0, 0.0, -1.0 / 100.0, 0.0],
            [0.0, 0.0, 0.0, 1.0],
        ]
    }

    /// A turn of `angle` about y, then a move to `at`.
    fn placed(at: [f32; 3], angle: f32) -> crate::Mat4 {
        let (s, c) = angle.sin_cos();
        [
            [c, 0.0, -s, 0.0],
            [0.0, 1.0, 0.0, 0.0],
            [s, 0.0, c, 0.0],
            [at[0], at[1], at[2], 1.0],
        ]
    }

    fn transform_with(modelview: crate::Mat4, projection: crate::Mat4) -> crate::Transform {
        crate::Transform {
            modelview,
            projection,
            viewport: crate::Viewport {
                scale: [160.0, 120.0],
                trans: [160.0, 120.0],
            },
            tag: 0,
        }
    }

    fn shaded(geometry: u32, other_h: u32, other_l: u32) -> RenderState {
        RenderState {
            combine: SHADE,
            other_h,
            geometry: geometry | G_SHADING_SMOOTH,
            ..state(other_l)
        }
    }

    /// Appends a draw of `n` random triangles `size` across about the
    /// origin, triangle `k` with transform `transform(k)`.
    fn add_tris(
        f: &mut Frame,
        rng: &mut Lcg,
        n: usize,
        size: f32,
        transform: impl Fn(usize) -> u32,
        state: RenderState,
    ) {
        let first = f.vertices.len() as u32;
        for k in 0..n {
            let (cx, cy, cz) = (
                rng.next(-size, size),
                rng.next(-size, size),
                rng.next(-size, size),
            );
            for _ in 0..3 {
                f.vertices.push(crate::Vertex {
                    pos: [
                        cx + rng.next(-size, size) / 2.0,
                        cy + rng.next(-size, size) / 2.0,
                        cz + rng.next(-size, size) / 2.0,
                    ],
                    transform: transform(k),
                    color: [
                        rng.next(0.0, 1.0),
                        rng.next(0.0, 1.0),
                        rng.next(0.0, 1.0),
                        0.6,
                    ],
                    uv: [[0.0; 2]; 2],
                });
            }
        }
        f.commands.push(Command::Draw(crate::Draw {
            first,
            count: n as u32 * 3,
            state,
        }));
    }

    /// Appends a screen rectangle in pixels, as the interpreter records one.
    fn add_rect(f: &mut Frame, xy: [f32; 4], color: [f32; 4]) {
        let (w, h) = (f.width as f32, f.height() as f32);
        let t = f.transforms.len() as u32;
        f.transforms.push(crate::Transform {
            viewport: crate::Viewport {
                scale: [w / 2.0, h / 2.0],
                trans: [w / 2.0, h / 2.0],
            },
            ..transform_with(
                IDENTITY_M,
                [
                    [2.0 / w, 0.0, 0.0, 0.0],
                    [0.0, -2.0 / h, 0.0, 0.0],
                    [0.0, 0.0, 1.0, 0.0],
                    [-1.0, 1.0, 0.0, 1.0],
                ],
            )
        });
        let first = f.vertices.len() as u32;
        let corner = |x: usize, y: usize| crate::Vertex {
            pos: [xy[x * 2], xy[1 + y * 2], 1.0],
            transform: t,
            color: [0.0; 4],
            uv: [[0.0; 2]; 2],
        };
        let (a, b, c, d) = (corner(0, 0), corner(1, 0), corner(0, 1), corner(1, 1));
        f.vertices.extend([a, c, b, b, c, d]);
        // A fill: what comes before a backdrop.
        let mut s = state(0);
        s.other_h = G_CYC_FILL << 20;
        s.rect = true;
        s.combine = crate::combiner::PRIM_ONLY;
        s.prim = color;
        f.commands.push(Command::Draw(crate::Draw {
            first,
            count: 6,
            state: s,
        }));
    }

    /// Appends a draw of the quad `(x0, y0)` to `(x1, y1)` at depth `z`
    /// with `transform`, in one colour.
    fn add_quad(
        f: &mut Frame,
        [x0, y0, x1, y1, z]: [f32; 5],
        transform: u32,
        color: [f32; 4],
        state: RenderState,
    ) {
        let first = f.vertices.len() as u32;
        for (x, y) in [(x0, y0), (x1, y0), (x1, y1), (x0, y0), (x1, y1), (x0, y1)] {
            f.vertices.push(crate::Vertex {
                pos: [x, y, z],
                transform,
                color,
                uv: [[0.0; 2]; 2],
            });
        }
        f.commands.push(Command::Draw(crate::Draw {
            first,
            count: 6,
            state,
        }));
    }

    fn empty_frame() -> Frame {
        Frame {
            width: 320,
            ..Default::default()
        }
    }

    /// Objects at many depths, some crossing the near plane, with a depth
    /// buffer, one transform each.
    fn perspective_scene(f: &mut Frame, rng: &mut Lcg, other_h: u32, other_l: u32, geometry: u32) {
        for k in 0..24 {
            let z = if k % 8 == 0 {
                -12.0
            } else {
                rng.next(-700.0, -40.0)
            };
            let at = [rng.next(-0.5, 0.5) * -z, rng.next(-0.4, 0.4) * -z, z];
            let t = f.transforms.len() as u32;
            f.transforms.push(transform_with(
                placed(at, rng.next(0.0, 6.3)),
                perspective(),
            ));
            let s = shaded(G_ZBUFFER | geometry, other_h, DEPTH | other_l);
            add_tris(f, rng, 12, 30.0, |_| t, s);
        }
    }

    /// A renderer on the GPU, or `None` (skipping) without one unless
    /// `N64RUSTY_REQUIRE_GPU` is set.
    /// The vertex shader path is on even under `N64RUSTY_GPU_VERTICES=off`;
    /// a device whose uniform limits allow no window skips.
    fn gpu_renderer() -> Option<Renderer> {
        match Renderer::new() {
            Ok(mut r) => {
                let l = r.device.limits();
                let max = l.max_uniform_buffer_binding_size;
                if TransformWindow::from_limits(max, l.min_uniform_buffer_offset_alignment)
                    .is_none()
                {
                    eprintln!("skipping: uniform buffers too small for the vertex shader");
                    return None;
                }
                r.gpu_vertices = true;
                Some(r)
            }
            Err(e) if std::env::var_os("N64RUSTY_REQUIRE_GPU").is_none() => {
                eprintln!("skipping: {e}");
                None
            }
            Err(e) => panic!("{e}"),
        }
    }

    /// How many of the frame's draws the vertex shader moves.
    fn shader_draws(r: &Renderer, f: &Frame) -> usize {
        let draws: Vec<_> = f.draws().collect();
        window_bases(f, &draws, f.transforms.len(), r.window)
            .iter()
            .filter(|b| b.is_some())
            .count()
    }

    /// Draws `f` at `w` x `h` with vertices moved in the vertex shader,
    /// then on the CPU, and asserts the same pixels: all of them within
    /// one level, and nearly all exact. The shader compiler may fuse a
    /// multiply and an add (FMA), rounding a clip position in its last
    /// bit differently from the CPU; a colour interpolated across the
    /// triangle can then land one level of 255 off in a rare pixel.
    #[track_caller]
    fn assert_same_both_ways(r: &mut Renderer, f: &Frame, wide: bool, (w, h): (u32, u32)) {
        assert_drawn_same_both_ways(r, wide, (w, h), |r| r.render_letterboxed(f, w, h));
    }

    /// [`assert_same_both_ways`] for any way of drawing a picture.
    #[track_caller]
    fn assert_drawn_same_both_ways(
        r: &mut Renderer,
        wide: bool,
        (w, h): (u32, u32),
        render: impl Fn(&mut Renderer) -> Result<Image, RenderError>,
    ) {
        r.set_widescreen(wide);
        r.gpu_vertices = true;
        let gpu = render(r).unwrap();
        r.gpu_vertices = false;
        let cpu = render(r).unwrap();
        r.gpu_vertices = true;
        r.set_widescreen(false);
        let differ: Vec<_> = (0..(w * h) as usize)
            .filter(|i| gpu.rgba[i * 4..i * 4 + 4] != cpu.rgba[i * 4..i * 4 + 4])
            .collect();
        let most = differ
            .iter()
            .flat_map(|i| (0..4).map(move |c| i * 4 + c))
            .map(|k| gpu.rgba[k].abs_diff(cpu.rgba[k]))
            .max()
            .unwrap_or(0);
        eprintln!(
            "{w} x {h}, wide {wide}: {} pixels differ, by up to {most}",
            differ.len()
        );
        assert!(
            most <= PARITY_LEVELS && differ.len() as u32 <= w * h / PARITY_SHARE,
            "{} of {} pixels differ (by up to {most}) at {w} x {h}, wide {wide}; first {:?}",
            differ.len(),
            w * h,
            differ.first().map(|&i| (i as u32 % w, i as u32 / w)),
        );
        let drawn = gpu.rgba.chunks(4).filter(|p| p[..3] != [0, 0, 0]).count();
        assert!(
            drawn > (w * h / 20) as usize,
            "the scene shows: {drawn} pixels"
        );
    }

    /// How far a pixel may differ between the two paths, in levels of 255.
    const PARITY_LEVELS: u8 = 1;
    /// At most one pixel in this many may differ at all.
    const PARITY_SHARE: u32 = 10_000;

    const SIZES: [(u32, u32); 3] = [(320, 240), (640, 480), (1280, 960)];

    #[test]
    fn vertex_shader_matches_the_cpu_with_perspective() {
        let Some(mut r) = gpu_renderer() else { return };
        let mut f = empty_frame();
        perspective_scene(&mut f, &mut Lcg(1), 0, 0, 0);
        // Back faces culled too: the winding must agree.
        perspective_scene(&mut f, &mut Lcg(2), 0, 0, G_CULL_BACK);
        assert_eq!(shader_draws(&r, &f), f.draws().count());
        for size in SIZES {
            assert_same_both_ways(&mut r, &f, false, size);
        }
    }

    #[test]
    fn vertex_shader_matches_the_cpu_with_fog() {
        let Some(mut r) = gpu_renderer() else { return };
        let mut f = empty_frame();
        perspective_scene(&mut f, &mut Lcg(3), TWO_CYCLE, FOG_SHADE_A, G_FOG);
        for (k, d) in f.commands.iter_mut().enumerate() {
            if let Command::Draw(d) = d {
                d.state.fog = [0.0, 1.0, 0.0, 1.0];
                // gSPFogPosition(400, 600), (900, 1000) and (0, 1000).
                d.state.fog_factor = [[640.0, 128.0], [2560.0, -2304.0], [128.0, 0.0]][k % 3];
            }
        }
        assert_eq!(shader_draws(&r, &f), f.draws().count());
        for size in SIZES {
            assert_same_both_ways(&mut r, &f, false, size);
        }
    }

    #[test]
    fn vertex_shader_matches_the_cpu_with_flat_depth() {
        let Some(mut r) = gpu_renderer() else { return };
        let mut f = empty_frame();
        f.transforms.push(transform_with(IDENTITY_M, ortho()));
        // Pieces past the near and the far plane: clamped, not clipped.
        let s = shaded(G_ZBUFFER, 0, DEPTH);
        add_tris(&mut f, &mut Lcg(4), 40, 150.0, |_| 0, s);
        for v in &mut f.vertices {
            v.pos[2] *= 2.0;
        }
        assert!(f.vertices.iter().any(|v| v.pos[2] > 100.0));
        assert!(f.vertices.iter().any(|v| v.pos[2] < -100.0));
        assert_eq!(shader_draws(&r, &f), 1);
        for size in SIZES {
            assert_same_both_ways(&mut r, &f, false, size);
        }
    }

    /// A background, a backdrop, a scene, HUD pieces in their areas and on
    /// a panel, and a see-through overlay over all.
    fn widescreen_scene() -> Frame {
        let mut f = empty_frame();
        let mut rng = Lcg(5);
        add_rect(&mut f, [0.0, 0.0, 320.0, 240.0], [0.0, 0.0, 1.0, 1.0]);
        // The backdrop: a wall far away covering the whole view.
        let wall = f.transforms.len() as u32;
        f.transforms.push(transform_with(
            placed([0.0, 0.0, -900.0], 0.0),
            perspective(),
        ));
        let sky = [0.3, 0.7, 0.9, 1.0];
        add_quad(
            &mut f,
            [-1300.0, -800.0, 1300.0, 800.0, 0.0],
            wall,
            sky,
            shaded(0, 0, 0),
        );
        perspective_scene(&mut f, &mut rng, 0, 0, 0);
        // HUD: flat pieces at the left and right, a panel with text on it.
        let hud = f.transforms.len() as u32;
        f.transforms.push(transform_with(IDENTITY_M, ortho()));
        for xy in [
            [-150.0, 100.0, -120.0, 70.0, 50.0],
            [120.0, 100.0, 150.0, 70.0, 50.0],
            [-140.0, -60.0, 140.0, -110.0, 50.0],
            [-130.0, -70.0, -120.0, -80.0, 60.0],
        ] {
            let color = [rng.next(0.0, 1.0), 1.0, rng.next(0.0, 1.0), 1.0];
            add_quad(&mut f, xy, hud, color, shaded(0, 0, 0));
        }
        f.hud = vec![
            [0.0, 0.0, 100.0, 240.0, -1.0],
            [220.0, 0.0, 320.0, 60.0, 1.0],
        ];
        // A fade over the whole view, made of triangles.
        let fade = [1.0, 0.5, 0.0, 0.25];
        let s = shaded(0, 0, TRANSLUCENT);
        add_quad(&mut f, [-160.0, -120.0, 160.0, 120.0, 90.0], hud, fade, s);
        f
    }

    #[test]
    fn vertex_shader_matches_the_cpu_in_widescreen_and_out() {
        let Some(mut r) = gpu_renderer() else { return };
        let f = widescreen_scene();
        // The scene has what widescreen moves: check each is found.
        let draws: Vec<_> = f.draws().collect();
        let data = vertex_data(&f, &f.transforms, &draws, 2.0, (r.window, true), Vec::new());
        assert!(data.shift.iter().any(|&s| s < 0.0));
        assert!(data.shift.iter().any(|&s| s > 0.0));
        let clip = clip_matrices(&f, &f.transforms);
        let flat = flat_transforms(&f.transforms);
        let backdrop = backdrops(&f, 2.0, |v| clip_position(&clip, &flat, v));
        assert!(backdrop[6], "the wall is a backdrop");
        assert_eq!(backdrop.last(), Some(&true), "the fade is an overlay");
        assert!(stretched_backgrounds(&f, 2.0)[0].is_some());
        assert_eq!(shader_draws(&r, &f), f.draws().count());
        for wide in [false, true] {
            for size in [(640, 360), (1280, 720), (320, 240)] {
                assert_same_both_ways(&mut r, &f, wide, size);
            }
        }
    }

    /// Far more transforms than a window holds: draws of a few, and one
    /// spanning them all, which moves on the CPU.
    fn many_transforms_scene(transforms: usize, rng: &mut Lcg) -> Frame {
        let mut f = empty_frame();
        for _ in 0..transforms {
            let z = rng.next(-800.0, -60.0);
            let at = [rng.next(-0.5, 0.5) * -z, rng.next(-0.4, 0.4) * -z, z];
            f.transforms.push(transform_with(
                placed(at, rng.next(0.0, 6.3)),
                perspective(),
            ));
        }
        let s = shaded(G_ZBUFFER, 0, DEPTH);
        // Draws of 30 triangles, 25 transforms on: windows start anywhere.
        for d in 0..transforms / 25 - 1 {
            add_tris(&mut f, rng, 30, 20.0, |k| (d * 25 + k) as u32, s.clone());
        }
        // One of them also with vertices whose transform is missing: those
        // move on the CPU (to zero) while the rest of the draw is raw.
        let first = f.vertices.len();
        add_tris(&mut f, rng, 30, 20.0, |k| k as u32, s.clone());
        for v in f.vertices[first..].iter_mut().step_by(4) {
            v.transform = transforms as u32 + 7;
        }
        add_tris(&mut f, rng, transforms / 5, 20.0, |k| (k * 5) as u32, s);
        f
    }

    /// The frame `f` with each transform tagged and moved a little, as the
    /// next game frame would have it.
    fn moved_on(f: &mut Frame) -> Frame {
        for (k, t) in f.transforms.iter_mut().enumerate() {
            t.tag = k as u32;
        }
        let mut next = f.clone();
        for t in &mut next.transforms {
            t.modelview[3][0] += 3.0;
            t.modelview[3][2] -= 2.0;
        }
        next
    }

    #[test]
    fn vertex_shader_matches_the_cpu_between_frames() {
        let Some(mut r) = gpu_renderer() else { return };
        let mut prev = empty_frame();
        perspective_scene(&mut prev, &mut Lcg(8), 0, 0, 0);
        let cur = moved_on(&mut prev);
        // Mid-way transforms are neither frame's: the blended ones are used.
        let mid = crate::interpolate(&prev, &cur, 0.5);
        assert!(mid
            .iter()
            .zip(&cur.transforms)
            .all(|(m, c)| m.modelview != c.modelview));
        assert_eq!(shader_draws(&r, &cur), cur.draws().count());
        for wide in [false, true] {
            for (w, h) in [(640, 480), (1280, 720)] {
                let render = |r: &mut Renderer| r.render_between(&prev, &cur, 0.5, w, h);
                assert_drawn_same_both_ways(&mut r, wide, (w, h), render);
            }
        }
    }

    /// The same triangles drawn again by a draw with another combiner (its
    /// own pipeline and vertex shader) land at the same depth: with "less
    /// or equal" and no decal bias, the second pass covers all the first.
    #[test]
    fn a_second_pass_over_the_same_geometry_passes_the_depth_test() {
        let Some(mut r) = gpu_renderer() else { return };
        let mut first = empty_frame();
        perspective_scene(&mut first, &mut Lcg(9), 0, 0, 0);
        for v in &mut first.vertices {
            v.color = [0.2, 0.8, 0.3, 1.0];
        }
        let magenta = [1.0, 0.0, 1.0, 1.0];
        let mut both = first.clone();
        let again: Vec<_> = first
            .draws()
            .map(|d| {
                let mut d = d.clone();
                d.state.combine = crate::combiner::PRIM_ONLY;
                d.state.prim = magenta;
                Command::Draw(d)
            })
            .collect();
        both.commands.extend(again);
        assert_eq!(shader_draws(&r, &both), both.draws().count());
        for wide in [false, true] {
            r.set_widescreen(wide);
            for (w, h) in SIZES {
                let once = r.render_letterboxed(&first, w, h).unwrap();
                let twice = r.render_letterboxed(&both, w, h).unwrap();
                let covered = once.rgba.chunks(4).filter(|p| p[..3] != [0, 0, 0]);
                let pairs = covered.count();
                let missed = once
                    .rgba
                    .chunks(4)
                    .zip(twice.rgba.chunks(4))
                    .filter(|(a, b)| a[..3] != [0, 0, 0] && b[..3] != [255, 0, 255])
                    .count();
                assert!(pairs > (w * h / 20) as usize, "the scene shows");
                assert_eq!(
                    missed, 0,
                    "{w} x {h}, wide {wide}: pixels the second pass missed"
                );
            }
        }
        r.set_widescreen(false);
    }

    #[test]
    fn vertex_shader_matches_the_cpu_with_many_transforms() {
        let Some(mut r) = gpu_renderer() else { return };
        let n = r.window.len as usize * 3 / 2;
        let f = many_transforms_scene(n, &mut Lcg(6));
        assert_eq!(
            shader_draws(&r, &f),
            f.draws().count() - 1,
            "all but the widest draw"
        );
        for size in SIZES {
            assert_same_both_ways(&mut r, &f, false, size);
        }
    }

    /// CPU time to prepare the vertices of a large frame, both ways. Run
    /// with `cargo test --release -p n64rusty-gfx --lib -- --ignored
    /// --nocapture vertex_preparation_time`.
    #[test]
    #[ignore = "a measurement, not a check"]
    fn vertex_preparation_time() {
        const ROUNDS: u32 = 200;
        let window = TransformWindow::from_limits(65536, 256).unwrap();
        // About a busy game frame: 312 objects of 12 triangles each.
        let mut f = empty_frame();
        let mut rng = Lcg(7);
        for _ in 0..13 {
            perspective_scene(&mut f, &mut rng, 0, 0, 0);
        }
        let draws: Vec<_> = f.draws().collect();
        for (name, wide) in [("4:3", 1.0), ("widescreen", 1.5)] {
            for gpu in [false, true] {
                // The renderer keeps the vertex bytes from picture to picture.
                let mut scratch = Vec::new();
                let t0 = std::time::Instant::now();
                for _ in 0..ROUNDS {
                    let way = (window, gpu);
                    let data = vertex_data(&f, &f.transforms, &draws, wide, way, scratch);
                    scratch = std::hint::black_box(data).vertices;
                }
                let us = t0.elapsed().as_secs_f64() * 1e6 / f64::from(ROUNDS);
                eprintln!(
                    "{name}, {}: {us:.0} us a frame of {} vertices, {:.2} us a draw ({} draws)",
                    if gpu { "vertex shader" } else { "CPU" },
                    f.vertices.len(),
                    us / draws.len() as f64,
                    draws.len(),
                );
            }
        }
    }
}
