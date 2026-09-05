//! Slim wgpu renderer, distilled from switchblade's: sRGB surface,
//! instanced quads in logical pixels, per-video textures with a small
//! blit-generated mip chain (shimmer-free minification of 4K sources —
//! the same reason switchblade mips its hires texture), plus a glyph
//! atlas for the new text stack.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use winit::window::Window;

use crate::text::{ATLAS, TextCtx};

const MIP_LEVELS: u32 = 4;

/// The wordmark, baked in like the Dock icon (`main.rs`) so the launch
/// window works from a bare binary with no resource directory.
const LOGO_PNG: &[u8] = include_bytes!("../assets/logo.png");

/// The decoded wordmark: pixels, size, and the sub-rect the ink actually
/// occupies.
struct Logo {
    rgba: Vec<u8>,
    w: u32,
    h: u32,
    /// UV box of the ink, transparent margin excluded.
    uv: [f32; 4],
    /// Aspect of that box — what the layout wants, never the file's.
    aspect: f32,
}

/// Decode the wordmark to straight-alpha RGBA8 and measure it. The Dock
/// icon goes through AppKit, but a texture needs the pixels in-process —
/// hence the one image-format dependency in the tree.
///
/// The measuring matters: an exported logo carries whatever margin the
/// exporter left (assets/logo.png is padded ~12% at the top and ~18% at
/// the bottom, so drawing it whole would sit the mark visibly high in its
/// own box). Trimming to the ink here keeps the launch layout independent
/// of the file's framing — the same reason the renderer measures text
/// rather than letting callers estimate it.
fn decode_logo() -> Logo {
    let mut dec = png::Decoder::new(std::io::Cursor::new(LOGO_PNG));
    // Widen whatever the file happens to be (palette, 16-bit, no alpha)
    // to the RGBA8 the texture wants.
    dec.set_transformations(png::Transformations::EXPAND | png::Transformations::ALPHA);
    let mut reader = dec.read_info().expect("logo png header");
    let mut buf = vec![0u8; reader.output_buffer_size().expect("logo png size")];
    let info = reader.next_frame(&mut buf).expect("logo png pixels");
    buf.truncate(info.buffer_size());
    let (w, h) = (info.width, info.height);

    // Alpha bounding box. The cutoff ignores the near-zero fringe a soft
    // export leaves outside the artwork.
    const INK: u8 = 8;
    let (mut x0, mut y0, mut x1, mut y1) = (w, h, 0u32, 0u32);
    for y in 0..h {
        let row = &buf[(y * w * 4) as usize..((y + 1) * w * 4) as usize];
        for x in 0..w {
            if row[(x * 4 + 3) as usize] > INK {
                x0 = x0.min(x);
                x1 = x1.max(x);
                y0 = y0.min(y);
                y1 = y1.max(y);
            }
        }
    }
    // A fully transparent image would leave the box inverted; fall back
    // to the whole texture rather than divide by a negative.
    let (x0, y0, x1, y1) = if x1 >= x0 && y1 >= y0 { (x0, y0, x1, y1) } else { (0, 0, w - 1, h - 1) };
    let (bw, bh) = ((x1 - x0 + 1) as f32, (y1 - y0 + 1) as f32);
    Logo {
        rgba: buf,
        w,
        h,
        uv: [
            x0 as f32 / w as f32,
            y0 as f32 / h as f32,
            (x1 + 1) as f32 / w as f32,
            (y1 + 1) as f32 / h as f32,
        ],
        aspect: bw / bh,
    }
}

/// sRGB → linear for values headed to an `*UnormSrgb` render target that
/// re-encodes on write (the shader's `ui_color`, CPU side).
fn srgb_to_linear(c: f32) -> f64 {
    let c = c as f64;
    if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
}

/// Fullscreen-triangle blit used to fill each video mip from the previous.
const BLIT_WGSL: &str = r#"
@group(0) @binding(0) var src: texture_2d<f32>;
@group(0) @binding(1) var samp: sampler;

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) vi: u32) -> VsOut {
    var out: VsOut;
    let xy = vec2<f32>(f32((vi << 1u) & 2u), f32(vi & 2u));
    out.pos = vec4<f32>(xy * 2.0 - 1.0, 0.0, 1.0);
    out.uv = vec2<f32>(xy.x, 1.0 - xy.y);
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    return textureSample(src, samp, in.uv);
}
"#;

/// Compare-shader modes (keep in sync with shader.wgsl).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoMode {
    Tex = 1,
    Delta = 2,
    Split = 3,
    Checker = 4,
    Blend = 5,
}

/// One decoded frame to upload into video texture `idx` this frame.
pub struct Upload {
    pub idx: usize,
    pub w: u32,
    pub h: u32,
    pub buf: Vec<u8>,
}

#[derive(Debug, Clone, Copy)]
pub struct RectPx {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// A filled rect: optionally rounded, optionally outlined, optionally
/// faded out toward its top edge (the transport scrim).
#[derive(Debug, Clone, Copy)]
pub struct RectItem {
    pub r: RectPx,
    pub color: [f32; 4],
    pub radius: f32,
    pub border_w: f32,
    pub border_color: [f32; 4],
    /// Alpha ramps to zero at the top edge — a bottom-anchored gradient.
    pub fade_up: bool,
}

impl RectItem {
    pub fn new(r: RectPx, color: [f32; 4]) -> Self {
        Self { r, color, ..Default::default() }
    }
}

impl Default for RectItem {
    fn default() -> Self {
        Self {
            r: RectPx { x: 0.0, y: 0.0, w: 0.0, h: 0.0 },
            color: [0.0; 4],
            radius: 0.0,
            border_w: 0.0,
            border_color: [0.0; 4],
            fade_up: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Align {
    Left,
    Center,
    Right,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VAlign {
    /// `y` is the top of the line box.
    Top,
    /// `y` is the line box's vertical centre.
    Middle,
}

/// Chip drawn behind a text run. Sized from the run's real measured
/// width, so keycaps and pills fit their label exactly.
#[derive(Debug, Clone, Copy)]
pub struct TextBg {
    pub color: [f32; 4],
    pub radius: f32,
    pub pad_x: f32,
    pub pad_y: f32,
    /// Solid offset shadow beneath the chip (keycap depth); alpha 0 = none.
    pub shadow: [f32; 4],
    pub shadow_dy: f32,
}

impl TextBg {
    pub fn new(color: [f32; 4]) -> Self {
        Self {
            color,
            radius: 0.0,
            pad_x: 6.0,
            pad_y: 3.0,
            shadow: [0.0; 4],
            shadow_dy: 0.0,
        }
    }
}

#[derive(Debug, Clone)]
pub struct TextItem {
    /// Anchor point; `align` decides which edge of the run sits here.
    pub x: f32,
    pub y: f32,
    /// Logical px size.
    pub px: f32,
    pub color: [f32; 4],
    pub text: String,
    pub align: Align,
    pub valign: VAlign,
    /// Extra advance per glyph, logical px (CSS letter-spacing).
    pub tracking: f32,
    pub bg: Option<TextBg>,
}

impl TextItem {
    pub fn new(x: f32, y: f32, px: f32, color: [f32; 4], text: impl Into<String>) -> Self {
        Self {
            x,
            y,
            px,
            color,
            text: text.into(),
            align: Align::Left,
            valign: VAlign::Top,
            tracking: 0.0,
            bg: None,
        }
    }
}

pub enum Item {
    Rect(RectItem),
    Video {
        a: usize,
        b: usize,
        r: RectPx,
        uv: [f32; 4],
        mode: VideoMode,
        p0: f32,
        p1: f32,
    },
    Text(TextItem),
    /// The wordmark, drawn from the renderer's own texture. `alpha` fades
    /// it; the rect must already carry the logo's aspect (`Gpu::logo_aspect`).
    Logo { r: RectPx, alpha: f32 },
}

/// Everything the renderer needs for one frame.
pub struct FrameDesc {
    pub clear: [f32; 4],
    pub uploads: Vec<Upload>,
    pub items: Vec<Item>,
    pub animating: bool,
    pub redraw_at: Option<Instant>,
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Uniforms {
    viewport: [f32; 2],
    _pad: [f32; 2],
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Instance {
    pos: [f32; 2],
    size: [f32; 2],
    uv: [f32; 4],
    color: [f32; 4],
    mode: f32,
    p0: f32,
    p1: f32,
    pad: f32,
}

struct VideoTex {
    tex: wgpu::Texture,
    view: wgpu::TextureView,
    mips: u32,
    mip_views: Vec<wgpu::TextureView>,
    mip_bgs: Vec<wgpu::BindGroup>,
    dirty: bool,
}

pub struct Gpu {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    pipeline: wgpu::RenderPipeline,
    blit_pipeline: wgpu::RenderPipeline,
    uniforms: wgpu::Buffer,
    instances: wgpu::Buffer,
    instance_capacity: usize,
    sampler: wgpu::Sampler,
    bgl: wgpu::BindGroupLayout,
    /// Kept so `set_video_dims` can build mip bind groups for textures
    /// created after startup (a drop loads clips at runtime).
    blit_bgl: wgpu::BindGroupLayout,
    videos: Vec<VideoTex>,
    /// The launch wordmark: same texture shape as a video (mip chain and
    /// all — it is drawn at a fraction of its native size), uploaded once
    /// and bound in every group so mode 7 needs no batch key.
    logo: VideoTex,
    logo_uv: [f32; 4],
    logo_aspect: f32,
    /// Bind groups per (a, b) texture pair, created lazily.
    pair_bgs: HashMap<(usize, usize), wgpu::BindGroup>,
    glyph_tex: wgpu::Texture,
    glyph_view: wgpu::TextureView,
    pub text: TextCtx,
    pub scale: f32,
}

impl Gpu {
    pub async fn new(
        window: Arc<Window>,
        video_dims: &[(u32, u32)],
        text: TextCtx,
    ) -> anyhow::Result<Self> {
        let size = window.inner_size();
        let scale = window.scale_factor() as f32;
        // Launch state has no videos, but the flat/glyph pipeline still
        // needs a valid (0,0) texture pair to bind. Stand up one tiny
        // never-sampled texture so `pair_bg(0, 0)` is well-formed.
        let dummy_dims: [(u32, u32); 1] = [(2, 2)];
        let video_dims: &[(u32, u32)] = if video_dims.is_empty() { &dummy_dims } else { video_dims };
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let surface = instance.create_surface(window)?;
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                compatible_surface: Some(&surface),
                ..Default::default()
            })
            .await?;
        // 4K+ sources need texture room; ask for what the adapter has
        // (wgpu's default caps at 8192 — switchblade learning).
        let mut limits = wgpu::Limits::default();
        limits.max_texture_dimension_2d =
            adapter.limits().max_texture_dimension_2d.clamp(8192, 16384);
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                required_limits: limits,
                ..Default::default()
            })
            .await?;

        let caps = surface.get_capabilities(&adapter);
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| f.is_srgb())
            .unwrap_or(caps.formats[0]);
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode: wgpu::PresentMode::AutoVsync,
            // The window is transparent (see main.rs): the clear colour
            // carries an alpha, so the desktop shows through the app's
            // background while opaque video quads stay solid. Premultiplied
            // is what `BlendState::ALPHA_BLENDING` accumulates; fall back to
            // whatever the surface does offer, opaque included.
            alpha_mode: [
                wgpu::CompositeAlphaMode::PreMultiplied,
                wgpu::CompositeAlphaMode::PostMultiplied,
            ]
            .into_iter()
            .find(|m| caps.alpha_modes.contains(m))
            .unwrap_or(caps.alpha_modes[0]),
            color_space: wgpu::SurfaceColorSpace::Auto,
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &config);

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("abner"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shader.wgsl").into()),
        });
        let uniforms = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("uniforms"),
            size: std::mem::size_of::<Uniforms>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Linear,
            ..Default::default()
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
        let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("abner"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                tex_entry(1),
                tex_entry(2),
                tex_entry(3),
                wgpu::BindGroupLayoutEntry {
                    binding: 4,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                tex_entry(5),
            ],
        });

        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("abner"),
            bind_group_layouts: &[Some(&bgl)],
            immediate_size: 0,
        });
        let instance_layout = wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<Instance>() as u64,
            step_mode: wgpu::VertexStepMode::Instance,
            attributes: &wgpu::vertex_attr_array![
                0 => Float32x2,
                1 => Float32x2,
                2 => Float32x4,
                3 => Float32x4,
                4 => Float32,
                5 => Float32,
                6 => Float32,
                7 => Float32,
            ],
        };
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("abner"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[Some(instance_layout)],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: config.format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

        // Mip-downsample blit (per video texture, after each frame upload).
        let blit_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("mip blit"),
            source: wgpu::ShaderSource::Wgsl(BLIT_WGSL.into()),
        });
        let blit_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("blit"),
            entries: &[
                tex_entry(0),
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let blit_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("blit"),
            bind_group_layouts: &[Some(&blit_bgl)],
            immediate_size: 0,
        });
        let blit_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("mip blit"),
            layout: Some(&blit_layout),
            vertex: wgpu::VertexState {
                module: &blit_shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            fragment: Some(wgpu::FragmentState {
                module: &blit_shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: wgpu::TextureFormat::Rgba8UnormSrgb,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

        let videos = video_dims
            .iter()
            .map(|&(w, h)| Self::make_video_tex(&device, &blit_bgl, &sampler, w, h))
            .collect();

        let logo_img = decode_logo();
        let (logo_w, logo_h) = (logo_img.w, logo_img.h);
        let logo = Self::make_video_tex(&device, &blit_bgl, &sampler, logo_w, logo_h);
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &logo.tex,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &logo_img.rgba,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(logo_w * 4),
                rows_per_image: Some(logo_h),
            },
            wgpu::Extent3d { width: logo_w, height: logo_h, depth_or_array_layers: 1 },
        );

        let glyph_tex = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("glyphs"),
            size: wgpu::Extent3d { width: ATLAS, height: ATLAS, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::R8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let glyph_view = glyph_tex.create_view(&wgpu::TextureViewDescriptor::default());

        let instance_capacity = 1024;
        let instances = Self::make_instance_buffer(&device, instance_capacity);

        let mut gpu = Self {
            surface,
            device,
            queue,
            config,
            pipeline,
            blit_pipeline,
            uniforms,
            instances,
            instance_capacity,
            sampler,
            bgl,
            blit_bgl,
            videos,
            // Mips are blitted on the first frame, like a video upload.
            logo: VideoTex { dirty: true, ..logo },
            logo_uv: logo_img.uv,
            logo_aspect: logo_img.aspect,
            pair_bgs: HashMap::new(),
            glyph_tex,
            glyph_view,
            text,
            scale,
        };
        // Keyless batches (text/rects before any video item) fall back to
        // the (0, 0) pair — make sure it exists.
        gpu.pair_bg(0, 0);
        Ok(gpu)
    }

    /// One video's texture + its blit-filled mip chain. Called at startup
    /// and again whenever the video set changes (a drop loads clips at
    /// runtime), so it takes only borrowed device state.
    fn make_video_tex(
        device: &wgpu::Device,
        blit_bgl: &wgpu::BindGroupLayout,
        sampler: &wgpu::Sampler,
        w: u32,
        h: u32,
    ) -> VideoTex {
        let (w, h) = (w.max(2), h.max(2));
        let mips = (32 - w.max(h).leading_zeros()).min(MIP_LEVELS);
        let tex = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("video"),
            size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
            mip_level_count: mips,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_DST
                | wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        let view = tex.create_view(&wgpu::TextureViewDescriptor::default());
        let mip_views: Vec<_> = (0..mips)
            .map(|i| {
                tex.create_view(&wgpu::TextureViewDescriptor {
                    base_mip_level: i,
                    mip_level_count: Some(1),
                    ..Default::default()
                })
            })
            .collect();
        let mip_bgs: Vec<_> = (1..mips as usize)
            .map(|i| {
                device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("video mip"),
                    layout: blit_bgl,
                    entries: &[
                        wgpu::BindGroupEntry {
                            binding: 0,
                            resource: wgpu::BindingResource::TextureView(&mip_views[i - 1]),
                        },
                        wgpu::BindGroupEntry {
                            binding: 1,
                            resource: wgpu::BindingResource::Sampler(sampler),
                        },
                    ],
                })
            })
            .collect();
        VideoTex { tex, view, mips, mip_views, mip_bgs, dirty: false }
    }

    /// Re-point the video textures at a new set of streams (a drop loaded
    /// clips at runtime). Slots whose dimensions are unchanged KEEP their
    /// texture — an append (A, B gain a C) must not blank the two streams
    /// already on screen, which would otherwise show black until their
    /// next frame lands. Every cached pair bind group holds views into
    /// these textures, so any real change drops the whole cache.
    pub fn set_video_dims(&mut self, dims: &[(u32, u32)]) {
        // Same reason as `new`: with no streams loaded the flat/glyph
        // pipeline still needs a well-formed (0, 0) pair to bind.
        let dummy: [(u32, u32); 1] = [(2, 2)];
        let dims: &[(u32, u32)] = if dims.is_empty() { &dummy } else { dims };
        let mut old = std::mem::take(&mut self.videos).into_iter();
        let mut changed = false;
        let mut videos = Vec::with_capacity(dims.len());
        for &(w, h) in dims {
            let (w, h) = (w.max(2), h.max(2));
            match old.next() {
                Some(v) if v.tex.size().width == w && v.tex.size().height == h => videos.push(v),
                _ => {
                    changed = true;
                    videos.push(Self::make_video_tex(
                        &self.device,
                        &self.blit_bgl,
                        &self.sampler,
                        w,
                        h,
                    ));
                }
            }
        }
        // Anything left over is a stream that went away.
        changed |= old.next().is_some();
        self.videos = videos;
        if changed {
            self.pair_bgs.clear();
            self.pair_bg(0, 0);
        }
    }

    fn make_instance_buffer(device: &wgpu::Device, capacity: usize) -> wgpu::Buffer {
        device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("instances"),
            size: (std::mem::size_of::<Instance>() * capacity) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        })
    }

    fn pair_bg(&mut self, a: usize, b: usize) -> (usize, usize) {
        let key = (a.min(self.videos.len() - 1), b.min(self.videos.len() - 1));
        if !self.pair_bgs.contains_key(&key) {
            let bg = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("pair"),
                layout: &self.bgl,
                entries: &[
                    wgpu::BindGroupEntry { binding: 0, resource: self.uniforms.as_entire_binding() },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::TextureView(&self.videos[key.0].view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: wgpu::BindingResource::TextureView(&self.videos[key.1].view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 3,
                        resource: wgpu::BindingResource::TextureView(&self.glyph_view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 4,
                        resource: wgpu::BindingResource::Sampler(&self.sampler),
                    },
                    wgpu::BindGroupEntry {
                        binding: 5,
                        resource: wgpu::BindingResource::TextureView(&self.logo.view),
                    },
                ],
            });
            self.pair_bgs.insert(key, bg);
        }
        key
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        self.config.width = width;
        self.config.height = height;
        self.surface.configure(&self.device, &self.config);
    }

    pub fn set_scale(&mut self, scale: f32) {
        self.scale = scale;
    }

    /// Width / height of the wordmark's INK, margin excluded. The
    /// renderer owns the image, so it owns its proportions too — same
    /// rule as text.
    pub fn logo_aspect(&self) -> f32 {
        self.logo_aspect
    }

    fn upload_video(&mut self, up: &Upload) {
        let Some(v) = self.videos.get_mut(up.idx) else { return };
        let sz = v.tex.size();
        if up.w != sz.width || up.h != sz.height || up.buf.len() != (up.w * up.h * 4) as usize {
            log::warn!("bad video upload: idx {} {}x{} {}B", up.idx, up.w, up.h, up.buf.len());
            return;
        }
        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &v.tex,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &up.buf,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(up.w * 4),
                rows_per_image: Some(up.h),
            },
            wgpu::Extent3d { width: up.w, height: up.h, depth_or_array_layers: 1 },
        );
        v.dirty = true;
    }

    /// Upload one rect of the CPU glyph atlas. The source slice starts at
    /// the rect's first byte with the atlas width as its row pitch, so no
    /// copy-out is needed.
    fn upload_glyph_rect(&self, x: u32, y: u32, w: u32, h: u32) {
        let offset = (y * ATLAS + x) as usize;
        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &self.glyph_tex,
                mip_level: 0,
                origin: wgpu::Origin3d { x, y, z: 0 },
                aspect: wgpu::TextureAspect::All,
            },
            &self.text.atlas[offset..],
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(ATLAS),
                rows_per_image: Some(h),
            },
            wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        );
    }

    pub fn render(&mut self, desc: &FrameDesc, viewport: (f32, f32)) {
        for up in &desc.uploads {
            self.upload_video(up);
        }
        // A deferred atlas reset lands here, before any layout in this
        // frame hands out a UV; the wiped texture goes up whole.
        if self.text.begin_frame() {
            self.upload_glyph_rect(0, 0, ATLAS, ATLAS);
        }

        // Build instances + draw batches (bind-group key per range).
        let mut data: Vec<Instance> = Vec::new();
        let mut batches: Vec<((usize, usize), std::ops::Range<u32>)> = Vec::new();
        let push = |data: &mut Vec<Instance>,
                        batches: &mut Vec<((usize, usize), std::ops::Range<u32>)>,
                        key: Option<(usize, usize)>,
                        inst: Instance| {
            let idx = data.len() as u32;
            data.push(inst);
            match (batches.last_mut(), key) {
                // Keyless items (text/rects) ride the current batch.
                (Some((_, range)), None) => range.end = idx + 1,
                (Some((k, range)), Some(key)) if *k == key => range.end = idx + 1,
                (_, key) => batches.push((key.unwrap_or((0, 0)), idx..idx + 1)),
            }
        };

        let scale = self.scale;
        // Mode 0 carries the border colour in the uv slot (see shader.wgsl).
        let rect_inst = |ri: &RectItem| Instance {
            pos: [ri.r.x, ri.r.y],
            size: [ri.r.w, ri.r.h],
            uv: ri.border_color,
            color: ri.color,
            mode: 0.0,
            p0: ri.radius,
            p1: ri.border_w,
            pad: if ri.fade_up { 1.0 } else { 0.0 },
        };
        for item in &desc.items {
            match item {
                Item::Rect(ri) => push(&mut data, &mut batches, None, rect_inst(ri)),
                Item::Video { a, b, r, uv, mode, p0, p1 } => {
                    let key = self.pair_bg(*a, *b);
                    push(&mut data, &mut batches, Some(key), Instance {
                        pos: [r.x, r.y],
                        size: [r.w, r.h],
                        uv: *uv,
                        color: [0.0; 4],
                        mode: *mode as u8 as f32,
                        p0: *p0,
                        p1: *p1,
                        pad: 0.0,
                    });
                }
                Item::Logo { r, alpha } => push(&mut data, &mut batches, None, Instance {
                    pos: [r.x, r.y],
                    size: [r.w, r.h],
                    uv: self.logo_uv,
                    color: [0.0, 0.0, 0.0, *alpha],
                    mode: 7.0,
                    p0: 0.0,
                    p1: 0.0,
                    pad: 0.0,
                }),
                Item::Text(t) => {
                    let laid = self.text.layout(&t.text, t.px * scale, t.tracking * scale);
                    let (tw, th) = (laid.w / scale, laid.h / scale);
                    // The anchor names an edge; the run is measured, so
                    // centring is exact rather than estimated.
                    let x0 = match t.align {
                        Align::Left => t.x,
                        Align::Center => t.x - tw / 2.0,
                        Align::Right => t.x - tw,
                    };
                    let y0 = match t.valign {
                        VAlign::Top => t.y,
                        VAlign::Middle => t.y - th / 2.0,
                    };
                    if let Some(bg) = &t.bg {
                        let chip = RectPx {
                            x: x0 - bg.pad_x,
                            y: y0 - bg.pad_y,
                            w: tw + bg.pad_x * 2.0,
                            h: th + bg.pad_y * 2.0,
                        };
                        if bg.shadow[3] > 0.0 {
                            let mut sh = chip;
                            sh.y += bg.shadow_dy;
                            push(&mut data, &mut batches, None, rect_inst(&RectItem {
                                r: sh,
                                color: bg.shadow,
                                radius: bg.radius,
                                ..Default::default()
                            }));
                        }
                        push(&mut data, &mut batches, None, rect_inst(&RectItem {
                            r: chip,
                            color: bg.color,
                            radius: bg.radius,
                            ..Default::default()
                        }));
                    }
                    for q in &laid.quads {
                        push(&mut data, &mut batches, None, Instance {
                            pos: [x0 + q.x / scale, y0 + q.y / scale],
                            size: [q.w / scale, q.h / scale],
                            uv: q.uv,
                            color: t.color,
                            mode: 6.0,
                            p0: 0.0,
                            p1: 0.0,
                            pad: 0.0,
                        });
                    }
                }
            }
        }
        // Glyphs rasterized during layout go up as their own rects, not
        // as a whole-atlas re-upload (4 MiB per new glyph otherwise).
        let pending = std::mem::take(&mut self.text.pending);
        for u in &pending {
            self.upload_glyph_rect(u.x, u.y, u.w, u.h);
        }

        if data.len() > self.instance_capacity {
            self.instance_capacity = data.len().next_power_of_two();
            self.instances = Self::make_instance_buffer(&self.device, self.instance_capacity);
        }
        if !data.is_empty() {
            self.queue.write_buffer(&self.instances, 0, bytemuck::cast_slice(&data));
        }
        self.queue.write_buffer(
            &self.uniforms,
            0,
            bytemuck::bytes_of(&Uniforms { viewport: [viewport.0, viewport.1], _pad: [0.0; 2] }),
        );

        let surface_tex = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(t) | wgpu::CurrentSurfaceTexture::Suboptimal(t) => t,
            wgpu::CurrentSurfaceTexture::Lost | wgpu::CurrentSurfaceTexture::Outdated => {
                self.surface.configure(&self.device, &self.config);
                return;
            }
            wgpu::CurrentSurfaceTexture::Occluded => return,
            wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Validation => {
                log::warn!("no surface texture this frame");
                return;
            }
        };
        let view = surface_tex.texture.create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("frame") });

        // Refresh dirty video mip chains (queued writes land first).
        for v in self.videos.iter_mut().chain(std::iter::once(&mut self.logo)) {
            if !v.dirty {
                continue;
            }
            v.dirty = false;
            for i in 1..v.mips as usize {
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("video mip"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &v.mip_views[i],
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    ..Default::default()
                });
                pass.set_pipeline(&self.blit_pipeline);
                pass.set_bind_group(0, &v.mip_bgs[i - 1], &[]);
                pass.draw(0..3, 0..1);
            }
        }

        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("main"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        // sRGB surface: the clear value is linear, but
                        // `clear` is authored as an sRGB colour like the
                        // rest of the palette (see ui_color in the shader).
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            // The window is transparent, and both the
                            // blend state and the surface's alpha mode
                            // are PREmultiplied, so the clear colour is
                            // scaled by its own alpha here.
                            r: srgb_to_linear(desc.clear[0]) * desc.clear[3] as f64,
                            g: srgb_to_linear(desc.clear[1]) * desc.clear[3] as f64,
                            b: srgb_to_linear(desc.clear[2]) * desc.clear[3] as f64,
                            a: desc.clear[3] as f64,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                ..Default::default()
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_vertex_buffer(0, self.instances.slice(..));
            for (key, range) in &batches {
                pass.set_bind_group(0, &self.pair_bgs[key], &[]);
                pass.draw(0..6, range.clone());
            }
        }
        self.queue.submit([encoder.finish()]);
        self.queue.present(surface_tex);
    }
}
