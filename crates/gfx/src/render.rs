//! Draws a [`Frame`] with wgpu into an offscreen image.
//!
//! Headless by design: no window or surface is ever created. Vertices are
//! transformed on the CPU at draw time from object space, so the transforms
//! in a frame can still be changed (widescreen, interpolation) right up to
//! the call to [`Renderer::render`].

use std::collections::HashMap;
use std::fmt;
use std::sync::OnceLock;

use crate::combiner::{shader, ShaderKey};
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
/// Floats per vertex: clip position, colour, two texture coordinates.
const VERTEX_FLOATS: usize = 12;

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
    /// Pictures drawn so far, to know which textures are still in use.
    pictures: u64,
    /// Vertex, uniform and read-back buffers kept from the last picture.
    /// Each draw is submitted before the next one writes them again.
    vertex_buf: Option<wgpu::Buffer>,
    uniform_buf: Option<wgpu::Buffer>,
    readback_buf: Option<wgpu::Buffer>,
    /// Texture bindings by what they bind, kept with the uploaded textures.
    bindings: HashMap<BindKey, wgpu::BindGroup>,
    /// Keep the vertex and uniform buffers across pictures. Off with
    /// `N64RUSTY_BUFFER_REUSE=off`, read when the renderer is made.
    buffer_reuse: bool,
}

/// Content key and wrap of each texture slot, and linear filtering.
type BindKey = ([Option<(u64, [Wrap; 2])>; 2], bool);

struct Uploaded {
    view: wgpu::TextureView,
    bytes: u64,
    last_used: u64,
}

/// Uploaded textures kept, by count and by bytes: thousands of small
/// textures cost a view each even under the byte limit.
const UPLOADED_MAX: usize = 2048;
const UPLOADED_MAX_BYTES: u64 = 256 << 20;
/// Texture bindings kept; all are dropped past this.
const BINDINGS_MAX: usize = 2048;

impl Renderer {
    /// Open the first GPU or software adapter, with no window.
    pub fn new() -> Result<Self, RenderError> {
        pollster::block_on(Self::new_async())
    }

    async fn new_async() -> Result<Self, RenderError> {
        let adapter = adapter()?;
        let info = adapter.get_info();
        let adapter_name = format!("{} ({:?})", info.name, info.backend);
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("n64rusty-gfx"),
                // All the adapter offers: the lowest limits cap textures at
                // 2048 pixels, smaller than a full-screen window.
                required_limits: adapter.limits(),
                ..Default::default()
            })
            .await
            .map_err(|e| RenderError::Device(e.to_string()))?;

        let uniform_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("uniforms"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: true,
                    min_binding_size: wgpu::BufferSize::new(UNIFORM_SIZE),
                },
                count: None,
            }],
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
        Ok(Self {
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
            readback_buf: None,
            bindings: HashMap::new(),
            buffer_reuse: std::env::var_os("N64RUSTY_BUFFER_REUSE").is_none_or(|v| v != "off"),
        })
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
            let module = self.shaders.entry(key.shader).or_insert_with(|| {
                device.create_shader_module(wgpu::ShaderModuleDescriptor {
                    label: Some("combiner"),
                    source: wgpu::ShaderSource::Wgsl(shader(key.shader).into()),
                })
            });
            let f = std::mem::size_of::<f32>() as u64;
            let attrs = [
                wgpu::VertexAttribute {
                    format: wgpu::VertexFormat::Float32x4,
                    offset: 0,
                    shader_location: 0,
                },
                wgpu::VertexAttribute {
                    format: wgpu::VertexFormat::Float32x4,
                    offset: 4 * f,
                    shader_location: 1,
                },
                wgpu::VertexAttribute {
                    format: wgpu::VertexFormat::Float32x2,
                    offset: 8 * f,
                    shader_location: 2,
                },
                wgpu::VertexAttribute {
                    format: wgpu::VertexFormat::Float32x2,
                    offset: 10 * f,
                    shader_location: 3,
                },
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
                        array_stride: VERTEX_FLOATS as u64 * f,
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
        let stretch = stretched_backgrounds(frame, wide);
        let clip = clip_matrices(frame, transforms);
        // Flat (orthographic) pieces get their depth clamped, not clipped:
        // a HUD piece in front of the near plane still shows. Perspective
        // geometry there stays clipped.
        let flat: Vec<bool> = transforms
            .iter()
            .map(|t| {
                t.projection[0][3] == 0.0 && t.projection[1][3] == 0.0 && t.projection[2][3] == 0.0
            })
            .collect();
        let at = |v: &crate::Vertex| {
            let mut p = clip
                .get(v.transform as usize)
                .map_or([0.0; 4], |m| transform(m, v.pos));
            if flat.get(v.transform as usize) == Some(&true) && p[3] > 0.0 {
                p[2] = p[2].clamp(0.0, p[3]);
            }
            p
        };
        let backdrop = backdrops(frame, wide, at);
        let shift = hud_shift(frame, transforms, &clip, wide);
        let fog = fog_factors(frame);
        let mut verts = Vec::with_capacity(frame.vertices.len() * VERTEX_FLOATS);
        for ((((v, stretch), backdrop), shift), fog) in frame
            .vertices
            .iter()
            .zip(stretch)
            .zip(backdrop)
            .zip(shift.iter().copied())
            .zip(fog)
        {
            let mut p = at(v);
            let mut color = v.color;
            if let Some([mul, off]) = fog {
                if p[3] > 0.0 {
                    // Back from wgpu's 0 to 1 depth to the console's -1 to 1.
                    let z = 2.0 * p[2] / p[3] - 1.0;
                    color[3] = (z * mul + off).clamp(0.0, 255.0) / 255.0;
                }
            }
            match stretch {
                // Rectangles are in pixels with w = 1.
                Some((x0, x1)) => p[0] = 2.0 * (v.pos[0] - x0) / (x1 - x0) - 1.0,
                None if backdrop => {}
                None => p[0] = p[0] / wide + shift * p[3],
            }
            verts.extend_from_slice(&p);
            verts.extend_from_slice(&color);
            verts.extend_from_slice(&v.uv[0]);
            verts.extend_from_slice(&v.uv[1]);
        }
        let vbuf = kept_buffer(
            &self.device,
            &self.queue,
            &mut self.vertex_buf,
            &floats(&verts),
            wgpu::BufferUsages::VERTEX,
            self.buffer_reuse,
        );

        // Per-draw uniforms.
        let draws: Vec<_> = frame
            .draws()
            // shortcut: draws into other colour images (render-to-texture)
            // are skipped; give them their own targets when a game needs it.
            .filter(|d| d.state.color_image == frame.color_image)
            .collect();
        let uniforms = draw_uniforms(&draws);
        let ubuf = kept_buffer(
            &self.device,
            &self.queue,
            &mut self.uniform_buf,
            &uniforms,
            wgpu::BufferUsages::UNIFORM,
            self.buffer_reuse,
        );
        let ubind = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("uniforms"),
            layout: &self.uniform_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                    buffer: &ubuf,
                    offset: 0,
                    size: wgpu::BufferSize::new(UNIFORM_SIZE),
                }),
            }],
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
                pass.set_bind_group(0, &ubind, &[(i as u64 * UNIFORM_STRIDE) as u32]);
                pass.set_bind_group(1, bind, &[]);
                pass.draw(d.first..d.first + d.count, 0..1);
            }
        }

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
        use std::hash::Hasher;
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
                let mut h = crate::tmem::Fx(0);
                h.write_u32(t.width);
                h.write_u32(t.height);
                h.write(&t.rgba);
                let key = h.finish();
                let k = kept.entry(key).or_insert_with(|| Uploaded {
                    view: fresh(&mut self.stats, t),
                    bytes: t.rgba.len() as u64,
                    last_used: 0,
                });
                k.last_used = self.pictures;
                (Some(key), k.view.clone())
            })
            .collect();
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
/// edge to edge, that together span the frame.
fn stretched_backgrounds(frame: &Frame, wide: f32) -> Vec<Option<(f32, f32)>> {
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
/// stretches like a background; `at` gives a vertex's clip position.
fn backdrops(frame: &Frame, wide: f32, at: impl Fn(&crate::Vertex) -> [f32; 4]) -> Vec<bool> {
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
            let ps: Vec<_> = frame.vertices[range.clone()].iter().map(&at).collect();
            let Some(w) = ps.first().map(|p| p[3]) else {
                continue;
            };
            let flat = w > 0.0 && ps.iter().all(|p| (p[3] - w).abs() <= w * OVERLAY_FLAT_W);
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

/// Per vertex, the fog multiplier and offset when fog is on: the vertex's
/// shade alpha becomes its fog factor, from its depth, as the microcode
/// computes it.
fn fog_factors(frame: &Frame) -> Vec<Option<[f32; 2]>> {
    let mut fog = vec![None; frame.vertices.len()];
    for d in frame.draws().filter(|d| !d.state.rect && d.state.geometry & G_FOG != 0) {
        fog[vertex_range(d)].fill(Some(d.state.fog_factor));
    }
    fog
}

/// The uniforms of each of `draws`, one [`UNIFORM_STRIDE`] apart.
fn draw_uniforms(draws: &[&crate::Draw]) -> Vec<u8> {
    let mut uniforms = vec![0u8; (draws.len().max(1) as u64 * UNIFORM_STRIDE) as usize];
    for (i, d) in draws.iter().enumerate() {
        let s = &d.state;
        let misc = [s.prim_lod, s.k5, 0.0, 0.0];
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
    let mut shift = vec![0.0f32; frame.vertices.len()];
    if wide <= 1.0 || frame.hud.is_empty() {
        return shift;
    }
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
    alpha: wgpu::CompositeAlphaMode,
    size: Option<(u32, u32)>,
    blit: Option<Blit>,
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
        // Mailbox shows the newest frame without waiting for the screen;
        // the game keeps its own pace either way.
        let present_mode = if caps.present_modes.contains(&wgpu::PresentMode::Mailbox) {
            wgpu::PresentMode::Mailbox
        } else {
            wgpu::PresentMode::Fifo
        };
        Ok(WindowTarget {
            surface,
            format,
            view_format: format.remove_srgb_suffix(),
            present_mode,
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
        let (src, mut encoder) = self.draw(frame, transforms, rect.2, rect.3);
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
        let (src, mut encoder) = self.draw(frame, transforms, rect.2, rect.3);
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
}
