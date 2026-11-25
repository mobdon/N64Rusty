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
use crate::{mat_mul, Command, Frame, RenderState, Wrap};

/// Pixels read back from the GPU.
#[derive(Clone, Debug, PartialEq)]
pub struct Image {
    pub width: u32,
    pub height: u32,
    /// RGBA8, row-major, top row first.
    pub rgba: Vec<u8>,
}

impl Image {
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

#[derive(Debug)]
pub enum RenderError {
    /// No GPU or software adapter was found.
    NoAdapter(String),
    Device(String),
    Readback(String),
}

impl fmt::Display for RenderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RenderError::NoAdapter(e) => write!(f, "no graphics adapter: {e}"),
            RenderError::Device(e) => write!(f, "graphics device: {e}"),
            RenderError::Readback(e) => write!(f, "reading the image back: {e}"),
        }
    }
}

impl std::error::Error for RenderError {}

const COLOR_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;
const DEPTH_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Depth32Float;
/// Bytes per draw in the uniform buffer (four vec4s, padded to the
/// guaranteed dynamic-offset alignment).
const UNIFORM_STRIDE: u64 = 256;
/// Floats per vertex: clip position, colour, two texture coordinates.
const VERTEX_FLOATS: usize = 12;

// Blender inputs: P and M pick a colour, A and B a factor.
const BL_IN: u32 = 0;
const BL_MEM: u32 = 1;

/// Everything that needs its own pipeline.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct PipeKey {
    shader: ShaderKey,
    /// Source and destination factors, or opaque.
    blend: Option<(u8, u8)>,
    depth_test: bool,
    depth_write: bool,
    decal: bool,
    /// 0 none, 1 front, 2 back.
    cull: u8,
}

fn pipe_key(s: &RenderState) -> PipeKey {
    let cycle = (s.other_h >> 20) & 3;
    let two_cycle = cycle == 1;
    let copy_or_fill = cycle >= 2;
    // Coverage times alpha ("texture edge" modes) behaves like a test at
    // one half. With alpha compare also on, both apply: a compare against a
    // zero blend alpha alone would draw the texture's clear parts solid.
    let alpha_test = match (cycle == 3, s.other_l & 3 == 1, s.other_l & 0x3000 == 0x3000) {
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
    let force_blend = s.other_l & 0x4000 != 0;
    // shortcut: only "IN * A + MEM * B" blends; fog and blend-colour mixes
    // are drawn opaque. Add them when a game needs them.
    let blend = (!copy_or_fill && force_blend && p == BL_IN && m == BL_MEM).then(|| {
        let src = if a == 3 { 0 } else { 1 }; // zero, or source alpha
        let dst = match b {
            0 if a == 3 => 2, // one
            0 => 3,           // one minus source alpha
            1 | 2 => 2,       // memory alpha, one
            _ => 0,           // zero
        };
        (src, dst)
    });
    let zbuf = if s.rect {
        s.other_l & 4 != 0 // rectangles compare only with the primitive depth
    } else {
        s.geometry & 1 != 0
    };
    let depth = zbuf && !copy_or_fill;
    let cull = if s.rect || copy_or_fill {
        0
    } else if s.geometry & 0x0400 != 0 {
        2
    } else if s.geometry & 0x0200 != 0 {
        1
    } else {
        0
    };
    PipeKey {
        shader: ShaderKey {
            combine: s.combine,
            two_cycle,
            alpha_test,
            flat: !s.rect && s.geometry & 0x0020_0000 == 0,
        },
        blend,
        depth_test: depth && s.other_l & 0x10 != 0,
        depth_write: depth && s.other_l & 0x20 != 0,
        decal: (s.other_l >> 10) & 3 == 3,
        cull,
    }
}

fn factor(f: u8) -> wgpu::BlendFactor {
    match f {
        0 => wgpu::BlendFactor::Zero,
        1 => wgpu::BlendFactor::SrcAlpha,
        2 => wgpu::BlendFactor::One,
        _ => wgpu::BlendFactor::OneMinusSrcAlpha,
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
    /// Colour and depth targets of the last size drawn.
    targets: Option<(u32, u32, wgpu::Texture, wgpu::Texture)>,
}

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
                    min_binding_size: wgpu::BufferSize::new(64),
                },
                count: None,
            }],
        });
        let tex_entry = |binding| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        };
        let smp_entry = |binding| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
            count: None,
        };
        let texture_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("textures"),
            entries: &[tex_entry(0), smp_entry(1), tex_entry(2), smp_entry(3)],
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
            targets: None,
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

    /// How many times wider than the frame's own shape a `width` x
    /// `height` target is drawn: 1 unless widescreen is on and the target
    /// is wider.
    fn wide(&self, frame: &Frame, width: u32, height: u32) -> f32 {
        if !self.widescreen {
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
                    src_factor: factor(src),
                    dst_factor: factor(dst),
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
                    cull_mode: match key.cull {
                        1 => Some(wgpu::Face::Front),
                        2 => Some(wgpu::Face::Back),
                        _ => None,
                    },
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
                    ..Default::default()
                })
            })
            .clone()
    }

    /// Draw `frame` at `width` x `height` pixels and read it back. The
    /// frame's native framebuffer is scaled to fill the image.
    pub fn render(&mut self, frame: &Frame, width: u32, height: u32) -> Result<Image, RenderError> {
        let (target, encoder) = self.draw(frame, &frame.transforms, width, height);
        self.read_back(encoder, &target)
    }

    /// Submits `encoder` and reads `target` back.
    fn read_back(
        &self,
        mut encoder: wgpu::CommandEncoder,
        target: &wgpu::Texture,
    ) -> Result<Image, RenderError> {
        let (width, height, size) = (target.width(), target.height(), target.size());
        // Read back, with rows padded to the copy alignment.
        let row = width * 4;
        let padded =
            row.div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let readback = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("readback"),
            size: (padded * height) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
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
        let mut rgba = Vec::with_capacity((row * height) as usize);
        for y in 0..height {
            let start = (y * padded) as usize;
            rgba.extend_from_slice(&data[start..start + row as usize]);
        }
        Ok(Image {
            width,
            height,
            rgba,
        })
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
        let size = wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        };
        // The window draws at one size frame after frame: keep the targets.
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
        let color_view = target.create_view(&Default::default());
        let depth_view = depth.create_view(&Default::default());

        // Vertices, transformed to clip space. In widescreen everything is
        // narrowed about the centre to keep its shape, except rectangles
        // spanning the whole frame (backgrounds), which stretch from edge
        // to edge of the target, over the border the game left.
        // A background may also be a row of tiles: rectangles on the same
        // lines, edge to edge, that together span the frame.
        let wide = self.wide(frame, width, height);
        let mut stretch = vec![None; frame.vertices.len()];
        let mut rows = HashMap::<(i32, i32), Vec<_>>::new();
        // Neighbouring rectangles with one state share a draw: look at each
        // one (two triangles, six vertices) on its own.
        let quads = frame
            .draws()
            .filter(|d| d.state.rect && wide > 1.0)
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
            if edge_to_edge && x0 <= fw * 0.05 && x1 >= fw * 0.95 {
                for (_, _, range) in row {
                    stretch[range].fill(Some((x0, x1)));
                }
            }
        }
        let clip = clip_matrices(frame, transforms);
        let at = |v: &crate::Vertex| {
            clip.get(v.transform as usize)
                .map_or([0.0; 4], |m| transform(m, v.pos))
        };
        // A backdrop drawn first (a sky made for the original view's width)
        // ends at the 4:3 edges: stretch it like a background. It is the
        // leading run of 3D draws that each cover nine tenths of the width.
        let mut backdrop = vec![false; frame.vertices.len()];
        if wide > 1.0 {
            for d in frame
                .draws()
                .filter(|d| d.state.color_image == frame.color_image)
            {
                if d.state.rect {
                    continue;
                }
                let range = d.first as usize..(d.first + d.count) as usize;
                let (mut x0, mut x1) = (1.0f32, -1.0f32);
                for v in &frame.vertices[range.clone()] {
                    let p = at(v);
                    if p[3] > 0.0 {
                        x0 = x0.min(p[0] / p[3]);
                        x1 = x1.max(p[0] / p[3]);
                    }
                }
                if x1.min(1.0) - x0.max(-1.0) < 1.8 {
                    break;
                }
                backdrop[range].fill(true);
            }
        }
        let mut verts = Vec::with_capacity(frame.vertices.len() * VERTEX_FLOATS);
        for ((v, stretch), backdrop) in frame.vertices.iter().zip(stretch).zip(backdrop) {
            let mut p = at(v);
            match stretch {
                // Rectangles are in pixels with w = 1.
                Some((x0, x1)) => p[0] = 2.0 * (v.pos[0] - x0) / (x1 - x0) - 1.0,
                None if backdrop => {}
                None => p[0] /= wide,
            }
            verts.extend_from_slice(&p);
            verts.extend_from_slice(&v.color);
            verts.extend_from_slice(&v.uv[0]);
            verts.extend_from_slice(&v.uv[1]);
        }
        let vbuf = self.buffer(&floats(&verts), wgpu::BufferUsages::VERTEX);

        // Per-draw uniforms.
        let draws: Vec<_> = frame
            .draws()
            // shortcut: draws into other colour images (render-to-texture)
            // are skipped; give them their own targets when a game needs it.
            .filter(|d| d.state.color_image == frame.color_image)
            .collect();
        let mut uniforms = vec![0u8; (draws.len().max(1) as u64 * UNIFORM_STRIDE) as usize];
        for (i, d) in draws.iter().enumerate() {
            let s = &d.state;
            let misc = [s.prim_lod, 0.0, 0.0, 0.0];
            let vals: Vec<f32> = [s.prim, s.env, s.blend, misc].concat();
            let at = i * UNIFORM_STRIDE as usize;
            uniforms[at..at + 64].copy_from_slice(&floats(&vals));
        }
        let ubuf = self.buffer(&uniforms, wgpu::BufferUsages::UNIFORM);
        let ubind = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("uniforms"),
            layout: &self.uniform_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                    buffer: &ubuf,
                    offset: 0,
                    size: wgpu::BufferSize::new(64),
                }),
            }],
        });

        let views: Vec<_> = frame
            .textures
            .iter()
            .map(|t| upload(&self.device, &self.queue, t.width, t.height, &t.rgba))
            .collect();

        // Prepare pipelines and texture bindings before recording passes.
        let mut prepared = Vec::with_capacity(draws.len());
        for d in &draws {
            let key = pipe_key(&d.state);
            self.pipeline(key);
            let linear = (d.state.other_h >> 20) & 3 != 2
                && (d.state.other_h >> 12) & 3 != 0
                && !d.state.texel_exact;
            let mut entries = Vec::new();
            for (k, slot) in d.state.textures.iter().enumerate() {
                let (view, wrap) = match slot.and_then(|i| Some((views.get(i as usize)?, i))) {
                    Some((v, i)) => (v.clone(), frame.textures[i as usize].wrap),
                    None => (self.white.clone(), [Wrap::Repeat; 2]),
                };
                entries.push((view, self.sampler(wrap, linear), k as u32 * 2));
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
            prepared.push((key, bind));
        }

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
                let [x, y, w, h] = scissor(d.state.scissor, frame, width, height, wide);
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

        (target, encoder)
    }

    fn buffer(&self, data: &[u8], usage: wgpu::BufferUsages) -> wgpu::Buffer {
        let buf = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: (data.len() as u64).max(64).next_multiple_of(4),
            usage: usage | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        self.queue.write_buffer(&buf, 0, data);
        buf
    }
}

/// Whether pixels `x0..x1` span a frame `fw` wide, give or take the border
/// games often leave (a scissor of 10 to 310 in a 320-wide frame).
fn spans_width(x0: f32, x1: f32, fw: f32) -> bool {
    x0 <= fw * 0.1 && x1 >= fw * 0.9
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
    let size = wgpu::Extent3d {
        width,
        height,
        depth_or_array_layers: 1,
    };
    let tex = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("n64 texture"),
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        ..target_desc(size)
    });
    queue.write_texture(
        tex.as_image_copy(),
        rgba,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(width * 4),
            rows_per_image: Some(height),
        },
        size,
    );
    tex.create_view(&Default::default())
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
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
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
        let max = self.device.limits().max_texture_dimension_2d;
        if width == 0 || height == 0 || width > max || height > max {
            return Ok(());
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
        let texture = match win.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(t) => t,
            wgpu::CurrentSurfaceTexture::Suboptimal(t) => {
                win.size = None; // configure again next frame
                t
            }
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                win.size = None;
                return Ok(());
            }
            _ => return Ok(()),
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
        self.queue.submit([encoder.finish()]);
        self.queue.present(texture);
        Ok(())
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
            env: [1.0; 4],
            blend: [0.0; 4],
            fog: [0.0; 4],
            textures: [None; 2],
            rect: false,
            texel_exact: false,
            color_image: 0,
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
}
