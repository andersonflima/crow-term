//! wgpu backend. Draws the same [`Scene`] as the software framebuffer with one
//! instanced quad per fill, glyph or decoration:
//!
//! * glyph images come from the shared [`GridShaper`] (same cache keys, same
//!   integer origins) and live in two `etagere` atlases: an R8 coverage mask
//!   and an RGBA page for color glyphs (emoji); texels are fetched exactly;
//! * the surface uses a non-sRGB 8-bit format so blending happens on the same
//!   gamma-encoded values as the software path (output matches within the
//!   documented golden tolerance);
//! * terminal rows keep their shaped instances per (pane, row) and are only
//!   reshaped when their cells change; the frame is rebuilt from those slices;
//! * the block cursor is a blend state that writes `1 - destination`.
//!
//! The surface is configured only when its size changes, presents with FIFO
//! (vsync) and is only drawn on request: no idle frames.
use super::{GridShaper, Layer, Op, PlacedGlyph, Rect, RowCache, Scene, ShapedRow};
use crate::config::Config;
use anyhow::{Context as _, Result, anyhow, bail};
use cosmic_text::{CacheKey, SwashContent};
use std::collections::HashMap;
use std::fmt;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use winit::window::Window;

const SOLID: u32 = 0;
const MASK: u32 = 1;
const COLOR: u32 = 2;

/// One quad. `rect` and `uv` are integer pixel/texel coordinates stored as f32.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub(crate) struct Instance {
    rect: [f32; 4],
    uv: [f32; 2],
    color: [u8; 4],
    kind: u32,
}
impl Instance {
    fn solid(rect: Rect, color: u32) -> Self {
        Self {
            rect: [rect.x as f32, rect.y as f32, rect.w as f32, rect.h as f32],
            uv: [0.0; 2],
            color: [(color >> 16) as u8, (color >> 8) as u8, color as u8, 255],
            kind: SOLID,
        }
    }
    fn glyph(glyph: &PlacedGlyph, entry: &Entry) -> Self {
        Self {
            rect: [
                (glyph.x + entry.left) as f32,
                (glyph.y - entry.top) as f32,
                entry.w as f32,
                entry.h as f32,
            ],
            uv: [entry.x as f32, entry.y as f32],
            color: [glyph.color.r(), glyph.color.g(), glyph.color.b(), 255],
            kind: if entry.color { COLOR } else { MASK },
        }
    }
    fn bounds(&self) -> Rect {
        Rect {
            x: self.rect[0] as i32,
            y: self.rect[1] as i32,
            w: self.rect[2] as u32,
            h: self.rect[3] as u32,
        }
    }
    fn translated(mut self, dx: i32, dy: i32) -> Self {
        self.rect[0] += dx as f32;
        self.rect[1] += dy as f32;
        self
    }
    /// The part of the quad inside `clip`, with texture coordinates shifted
    /// by the same amount; `None` when nothing is left.
    fn clipped(mut self, clip: Rect) -> Option<Self> {
        let old = self.bounds();
        let new = old.intersection(clip);
        if new.is_empty() {
            return None;
        }
        self.uv[0] += (new.x - old.x) as f32;
        self.uv[1] += (new.y - old.y) as f32;
        self.rect = [new.x as f32, new.y as f32, new.w as f32, new.h as f32];
        Some(self)
    }
}

/// A glyph image placed in an atlas page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Entry {
    color: bool,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
    left: i32,
    top: i32,
}

/// The atlas ran out of space during a frame.
#[derive(Debug)]
struct Full;

struct Page {
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    alloc: etagere::AtlasAllocator,
    size: u32,
    format: wgpu::TextureFormat,
    bytes_per_pixel: u32,
}
impl Page {
    fn new(device: &wgpu::Device, size: u32, format: wgpu::TextureFormat, label: &str) -> Self {
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some(label),
            size: wgpu::Extent3d {
                width: size,
                height: size,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        Self {
            texture,
            view,
            alloc: etagere::AtlasAllocator::new(etagere::size2(size as i32, size as i32)),
            size,
            format,
            bytes_per_pixel: if format == wgpu::TextureFormat::R8Unorm {
                1
            } else {
                4
            },
        }
    }
}

/// Glyph atlases. Growing or resetting bumps `generation`, which invalidates
/// every cached instance that points into the old pages.
struct Atlas {
    mask: Page,
    color: Page,
    entries: HashMap<CacheKey, Option<Entry>>,
    generation: u64,
    max_size: u32,
}
impl Atlas {
    fn new(device: &wgpu::Device) -> Self {
        let max_size = device.limits().max_texture_dimension_2d.clamp(512, 8192);
        Self {
            mask: Page::new(
                device,
                1024.min(max_size),
                wgpu::TextureFormat::R8Unorm,
                "crow glyph mask atlas",
            ),
            color: Page::new(
                device,
                512.min(max_size),
                wgpu::TextureFormat::Rgba8Unorm,
                "crow color glyph atlas",
            ),
            entries: HashMap::new(),
            generation: 0,
            max_size,
        }
    }
    /// Where `key`'s image is, rasterizing and uploading it on first use.
    /// `Ok(None)` for glyphs without pixels (spaces) or unsupported content.
    fn get(
        &mut self,
        key: CacheKey,
        shaper: &mut GridShaper,
        queue: &wgpu::Queue,
    ) -> std::result::Result<Option<Entry>, Full> {
        if let Some(entry) = self.entries.get(&key) {
            return Ok(*entry);
        }
        let entry = match shaper.swash.get_image(&mut shaper.fonts, key) {
            Some(image) if image.placement.width > 0 && image.placement.height > 0 => {
                let color = match image.content {
                    SwashContent::Mask => false,
                    SwashContent::Color => true,
                    // The software path skips subpixel masks too.
                    SwashContent::SubpixelMask => {
                        self.entries.insert(key, None);
                        return Ok(None);
                    }
                };
                let page = if color {
                    &mut self.color
                } else {
                    &mut self.mask
                };
                let (w, h) = (image.placement.width, image.placement.height);
                let Some(allocation) = page.alloc.allocate(etagere::size2(w as i32, h as i32))
                else {
                    return Err(Full);
                };
                let (x, y) = (
                    allocation.rectangle.min.x as u32,
                    allocation.rectangle.min.y as u32,
                );
                queue.write_texture(
                    wgpu::TexelCopyTextureInfo {
                        texture: &page.texture,
                        mip_level: 0,
                        origin: wgpu::Origin3d { x, y, z: 0 },
                        aspect: wgpu::TextureAspect::All,
                    },
                    &image.data,
                    wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(w * page.bytes_per_pixel),
                        rows_per_image: Some(h),
                    },
                    wgpu::Extent3d {
                        width: w,
                        height: h,
                        depth_or_array_layers: 1,
                    },
                );
                Some(Entry {
                    color,
                    x,
                    y,
                    w,
                    h,
                    left: image.placement.left,
                    top: image.placement.top,
                })
            }
            _ => None,
        };
        self.entries.insert(key, entry);
        Ok(entry)
    }
    /// Double the pages up to the device limit; at the limit start over.
    fn grow(&mut self, device: &wgpu::Device) {
        let next = |page: &Page| (page.size * 2).min(self.max_size);
        let (mask, color) = (next(&self.mask), next(&self.color));
        self.mask = Page::new(device, mask, self.mask.format, "crow glyph mask atlas");
        self.color = Page::new(device, color, self.color.format, "crow color glyph atlas");
        self.entries.clear();
        self.generation += 1;
    }
}

/// Adapter identity for `doctor`, status messages and benchmarks.
#[derive(Debug, Clone)]
pub struct AdapterSummary {
    pub name: String,
    pub backend: String,
    pub device_type: String,
    pub driver: String,
    /// A CPU rasterizer (llvmpipe, lavapipe, WARP): `auto` prefers the
    /// built-in software renderer over it.
    pub cpu: bool,
}
impl fmt::Display for AdapterSummary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({}, {})", self.name, self.backend, self.device_type)?;
        if !self.driver.is_empty() {
            write!(f, ", driver {}", self.driver)?;
        }
        Ok(())
    }
}
impl From<wgpu::AdapterInfo> for AdapterSummary {
    fn from(info: wgpu::AdapterInfo) -> Self {
        Self {
            name: info.name,
            backend: format!("{:?}", info.backend).to_lowercase(),
            device_type: format!("{:?}", info.device_type).to_lowercase(),
            driver: [info.driver, info.driver_info]
                .into_iter()
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join(" "),
            cpu: info.device_type == wgpu::DeviceType::Cpu,
        }
    }
}

fn instance_for(window: Option<&Arc<Window>>) -> wgpu::Instance {
    let descriptor = match window {
        // GLES (Linux/Wayland) needs the display connection at instance creation.
        Some(window) => {
            wgpu::InstanceDescriptor::new_with_display_handle_from_env(Box::new(window.clone()))
        }
        None => wgpu::InstanceDescriptor::new_without_display_handle_from_env(),
    };
    wgpu::Instance::new(descriptor)
}
fn request_adapter(
    instance: &wgpu::Instance,
    surface: Option<&wgpu::Surface<'_>>,
) -> Result<wgpu::Adapter> {
    pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference:
            wgpu::PowerPreference::from_env().unwrap_or(wgpu::PowerPreference::LowPower),
        force_fallback_adapter: false,
        compatible_surface: surface,
        ..Default::default()
    }))
    .map_err(|error| anyhow!("no compatible GPU adapter: {error}"))
}

/// Adapter that a window would get, without opening one (`crow-term doctor`).
pub fn probe() -> Result<AdapterSummary> {
    let instance = instance_for(None);
    Ok(request_adapter(&instance, None)?.get_info().into())
}

struct Target {
    window: Arc<Window>,
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    configured: bool,
    /// The surface texture can be copied out (frame dumps).
    capture: bool,
    readback: Option<(u32, u32, wgpu::Buffer)>,
}

#[derive(Default)]
struct Health {
    failed: AtomicBool,
    reason: Mutex<Option<String>>,
}
impl Health {
    fn fail(&self, reason: String) {
        if let Ok(mut slot) = self.reason.lock() {
            slot.get_or_insert(reason);
        }
        self.failed.store(true, Ordering::SeqCst);
    }
    fn check(&self) -> Result<()> {
        if self.failed.load(Ordering::SeqCst) {
            let reason = self
                .reason
                .lock()
                .ok()
                .and_then(|slot| slot.clone())
                .unwrap_or_else(|| "unknown GPU error".into());
            bail!("GPU device failed: {reason}");
        }
        Ok(())
    }
}

struct GpuRow {
    shaped: ShapedRow,
    instances: Vec<Instance>,
    generation: u64,
}

struct CachedLayer {
    key: u64,
    generation: u64,
    instances: Vec<Instance>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Pass {
    Normal,
    Invert,
}

pub struct GpuRenderer {
    _instance: wgpu::Instance,
    adapter: wgpu::Adapter,
    device: wgpu::Device,
    queue: wgpu::Queue,
    summary: AdapterSummary,
    health: Arc<Health>,
    target: Option<Target>,
    shader: wgpu::ShaderModule,
    layout: wgpu::PipelineLayout,
    bind_layout: wgpu::BindGroupLayout,
    pipelines: HashMap<wgpu::TextureFormat, (wgpu::RenderPipeline, wgpu::RenderPipeline)>,
    globals: wgpu::Buffer,
    bind_group: Option<(u64, wgpu::BindGroup)>,
    atlas: Atlas,
    instance_buffer: Option<wgpu::Buffer>,
    rows: RowCache<GpuRow>,
    layer: Option<CachedLayer>,
    frame: Vec<Instance>,
    draws: Vec<(Pass, std::ops::Range<u32>)>,
    offscreen: Option<(u32, u32, wgpu::Texture, wgpu::Buffer)>,
}

impl GpuRenderer {
    /// Renderer presenting to `window`. `allow_cpu` accepts software
    /// rasterizer adapters (tests, `--renderer gpu`); `auto` refuses them.
    pub fn for_window(window: Arc<Window>, allow_cpu: bool) -> Result<Self> {
        let instance = instance_for(Some(&window));
        let surface = instance
            .create_surface(window.clone())
            .context("create GPU surface")?;
        let adapter = request_adapter(&instance, Some(&surface))?;
        let mut renderer = Self::with_adapter(instance, adapter, allow_cpu)?;
        let caps = surface.get_capabilities(&renderer.adapter);
        // Non-sRGB: blend on gamma-encoded values like the software framebuffer.
        let format = [
            wgpu::TextureFormat::Bgra8Unorm,
            wgpu::TextureFormat::Rgba8Unorm,
        ]
        .into_iter()
        .find(|format| caps.formats.contains(format))
        .ok_or_else(|| {
            anyhow!(
                "surface offers no non-sRGB 8-bit format (offers {:?})",
                caps.formats
            )
        })?;
        let size = window.inner_size();
        let mut config = surface
            .get_default_config(&renderer.adapter, size.width.max(1), size.height.max(1))
            .context("surface is not supported by the GPU adapter")?;
        config.format = format;
        config.present_mode = wgpu::PresentMode::Fifo;
        config.alpha_mode = if caps.alpha_modes.contains(&wgpu::CompositeAlphaMode::Opaque) {
            wgpu::CompositeAlphaMode::Opaque
        } else {
            caps.alpha_modes
                .first()
                .copied()
                .unwrap_or(wgpu::CompositeAlphaMode::Auto)
        };
        config.desired_maximum_frame_latency = 2;
        config.view_formats = Vec::new();
        // Frame dumps (test builds only) read the presented texture back.
        let capture =
            cfg!(feature = "frame-dump") && caps.usages.contains(wgpu::TextureUsages::COPY_SRC);
        if capture {
            config.usage |= wgpu::TextureUsages::COPY_SRC;
        }
        renderer.pipelines_for(format);
        renderer.target = Some(Target {
            window,
            surface,
            config,
            configured: false,
            capture,
            readback: None,
        });
        Ok(renderer)
    }
    /// Renderer without a window, drawing offscreen (tests, benchmarks).
    pub fn headless(allow_cpu: bool) -> Result<Self> {
        let instance = instance_for(None);
        let adapter = request_adapter(&instance, None)?;
        Self::with_adapter(instance, adapter, allow_cpu)
    }
    fn with_adapter(
        instance: wgpu::Instance,
        adapter: wgpu::Adapter,
        allow_cpu: bool,
    ) -> Result<Self> {
        let summary: AdapterSummary = adapter.get_info().into();
        if summary.cpu && !allow_cpu {
            bail!(
                "adapter {summary} is a CPU rasterizer; the built-in software renderer is preferred"
            );
        }
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("crow-term"),
            required_features: wgpu::Features::empty(),
            // Downlevel defaults: runs on GLES 3 / WARP / lavapipe as well.
            required_limits:
                wgpu::Limits::downlevel_webgl2_defaults().using_resolution(adapter.limits()),
            memory_hints: wgpu::MemoryHints::MemoryUsage,
            ..Default::default()
        }))
        .with_context(|| format!("open GPU device on {summary}"))?;
        let health = Arc::new(Health::default());
        let lost = health.clone();
        device.set_device_lost_callback(move |reason, message| {
            lost.fail(format!("device lost ({reason:?}): {message}"));
        });
        let errors = health.clone();
        device.on_uncaptured_error(Arc::new(move |error: wgpu::Error| {
            errors.fail(error.to_string());
        }));
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("crow-term renderer"),
            source: wgpu::ShaderSource::Wgsl(include_str!("gpu.wgsl").into()),
        });
        let texture_entry = |binding| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: false },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        };
        let bind_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("crow-term globals and atlases"),
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
                texture_entry(1),
                texture_entry(2),
            ],
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("crow-term renderer"),
            bind_group_layouts: &[Some(&bind_layout)],
            immediate_size: 0,
        });
        let globals = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("crow-term globals"),
            size: 16,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let atlas = Atlas::new(&device);
        Ok(Self {
            _instance: instance,
            adapter,
            device,
            queue,
            summary,
            health,
            target: None,
            shader,
            layout,
            bind_layout,
            pipelines: HashMap::new(),
            globals,
            bind_group: None,
            atlas,
            instance_buffer: None,
            rows: RowCache::default(),
            layer: None,
            frame: Vec::new(),
            draws: Vec::new(),
            offscreen: None,
        })
    }
    pub fn adapter(&self) -> &AdapterSummary {
        &self.summary
    }
    /// Forget shaped rows (metrics, scale or layout changed).
    pub fn invalidate(&mut self) {
        self.rows.clear();
    }
    fn pipelines_for(&mut self, format: wgpu::TextureFormat) {
        if self.pipelines.contains_key(&format) {
            return;
        }
        let attributes = wgpu::vertex_attr_array![
            0 => Float32x4,
            1 => Float32x2,
            2 => Unorm8x4,
            3 => Uint32,
        ];
        let make = |entry: &str, blend: wgpu::BlendState| {
            self.device
                .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                    label: Some(entry),
                    layout: Some(&self.layout),
                    vertex: wgpu::VertexState {
                        module: &self.shader,
                        entry_point: Some("vs_main"),
                        compilation_options: Default::default(),
                        buffers: &[Some(wgpu::VertexBufferLayout {
                            array_stride: std::mem::size_of::<Instance>() as u64,
                            step_mode: wgpu::VertexStepMode::Instance,
                            attributes: &attributes,
                        })],
                    },
                    primitive: wgpu::PrimitiveState {
                        topology: wgpu::PrimitiveTopology::TriangleStrip,
                        ..Default::default()
                    },
                    depth_stencil: None,
                    multisample: wgpu::MultisampleState::default(),
                    fragment: Some(wgpu::FragmentState {
                        module: &self.shader,
                        entry_point: Some(entry),
                        compilation_options: Default::default(),
                        targets: &[Some(wgpu::ColorTargetState {
                            format,
                            blend: Some(blend),
                            write_mask: wgpu::ColorWrites::ALL,
                        })],
                    }),
                    multiview_mask: None,
                    cache: None,
                })
        };
        // Straight alpha, like the software blend; destination alpha is kept
        // opaque so a compositor never sees a translucent window.
        let keep_alpha = wgpu::BlendComponent {
            src_factor: wgpu::BlendFactor::Zero,
            dst_factor: wgpu::BlendFactor::One,
            operation: wgpu::BlendOperation::Add,
        };
        let normal = make(
            "fs_main",
            wgpu::BlendState {
                color: wgpu::BlendComponent {
                    src_factor: wgpu::BlendFactor::SrcAlpha,
                    dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                    operation: wgpu::BlendOperation::Add,
                },
                alpha: keep_alpha,
            },
        );
        let invert = make(
            "fs_invert",
            wgpu::BlendState {
                color: wgpu::BlendComponent {
                    src_factor: wgpu::BlendFactor::OneMinusDst,
                    dst_factor: wgpu::BlendFactor::Zero,
                    operation: wgpu::BlendOperation::Add,
                },
                alpha: keep_alpha,
            },
        );
        self.pipelines.insert(format, (normal, invert));
    }

    fn push(&mut self, instance: Instance, clip: Rect) {
        push(&mut self.frame, &mut self.draws, instance, clip);
    }

    /// Turn the scene into instances and draw ranges. Fails only when the
    /// glyph atlas filled up mid-frame (the caller grows it and retries).
    fn build(
        &mut self,
        scene: &Scene<'_>,
        shaper: &mut GridShaper,
        config: &Config,
    ) -> std::result::Result<(), Full> {
        self.frame.clear();
        self.draws.clear();
        self.rows.begin_frame();
        let bounds = Rect {
            x: 0,
            y: 0,
            w: scene.width,
            h: scene.height,
        };
        for op in &scene.ops {
            match op {
                Op::Fill { rect, color } => self.push(Instance::solid(*rect, *color), bounds),
                Op::Glyphs { clip, glyphs } => {
                    let clip = clip.intersection(bounds);
                    for glyph in glyphs {
                        if let Some(entry) = self.atlas.get(glyph.key, shaper, &self.queue)? {
                            self.push(Instance::glyph(glyph, &entry), clip);
                        }
                    }
                }
                Op::Row {
                    pane,
                    row,
                    cells,
                    x,
                    y,
                    clip,
                } => {
                    let generation = self.atlas.generation;
                    let cached = self.rows.get_or_insert_with(*pane, *row, cells, || GpuRow {
                        shaped: shaper.shape_row(config, cells),
                        instances: Vec::new(),
                        generation: u64::MAX,
                    });
                    if cached.generation != generation {
                        cached.instances =
                            row_instances(&cached.shaped, &mut self.atlas, shaper, &self.queue)?;
                        cached.generation = generation;
                    }
                    let row_clip = Rect {
                        x: *x,
                        y: *y,
                        w: cached.shaped.width as u32,
                        h: cached.shaped.height as u32,
                    }
                    .intersection(*clip)
                    .intersection(bounds);
                    for instance in &cached.instances {
                        push(
                            &mut self.frame,
                            &mut self.draws,
                            instance.translated(*x, *y),
                            row_clip,
                        );
                    }
                }
                Op::Invert { rect } => {
                    if let Some(instance) = Instance::solid(*rect, 0).clipped(bounds) {
                        let at = self.frame.len() as u32;
                        self.draws.push((Pass::Invert, at..at + 1));
                        self.frame.push(instance);
                    }
                }
                Op::Layer { rect, clip, layer } => {
                    let generation = self.atlas.generation;
                    if self.layer.as_ref().is_none_or(|cached| {
                        cached.key != layer.key || cached.generation != generation
                    }) {
                        self.layer = Some(CachedLayer {
                            key: layer.key,
                            generation,
                            instances: layer_instances(
                                layer,
                                &mut self.atlas,
                                shaper,
                                &self.queue,
                            )?,
                        });
                    }
                    let layer_clip = Rect {
                        x: rect.x,
                        y: rect.y,
                        w: layer.width,
                        h: layer.height,
                    }
                    .intersection(*clip)
                    .intersection(bounds);
                    let instances = self
                        .layer
                        .as_mut()
                        .map(|cached| std::mem::take(&mut cached.instances))
                        .unwrap_or_default();
                    for instance in &instances {
                        self.push(instance.translated(rect.x, rect.y), layer_clip);
                    }
                    if let Some(cached) = &mut self.layer {
                        cached.instances = instances;
                    }
                }
            }
        }
        self.rows.end_frame();
        Ok(())
    }

    fn prepare(&mut self, scene: &Scene<'_>, shaper: &mut GridShaper, config: &Config) {
        // A full atlas restarts the frame with bigger pages (all cached glyph
        // positions are invalidated by the new generation).
        for _ in 0..3 {
            match self.build(scene, shaper, config) {
                Ok(()) => break,
                Err(Full) => self.atlas.grow(&self.device),
            }
        }
        shaper.trim();
        let size = [scene.width as f32, scene.height as f32, 0.0, 0.0];
        self.queue
            .write_buffer(&self.globals, 0, bytemuck::cast_slice(&size));
        let bytes = std::mem::size_of_val(self.frame.as_slice()) as u64;
        if self
            .instance_buffer
            .as_ref()
            .is_none_or(|buffer| buffer.size() < bytes)
        {
            self.instance_buffer = Some(self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("crow-term instances"),
                size: bytes.max(4096).next_power_of_two(),
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }));
        }
        if let Some(buffer) = &self.instance_buffer
            && bytes > 0
        {
            self.queue
                .write_buffer(buffer, 0, bytemuck::cast_slice(&self.frame));
        }
        if self
            .bind_group
            .as_ref()
            .is_none_or(|(generation, _)| *generation != self.atlas.generation)
        {
            let group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("crow-term globals and atlases"),
                layout: &self.bind_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: self.globals.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::TextureView(&self.atlas.mask.view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: wgpu::BindingResource::TextureView(&self.atlas.color.view),
                    },
                ],
            });
            self.bind_group = Some((self.atlas.generation, group));
        }
    }

    fn encode(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        view: &wgpu::TextureView,
        format: wgpu::TextureFormat,
        background: u32,
    ) {
        let channel = |shift: u32| ((background >> shift) & 255) as f64 / 255.0;
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("crow-term frame"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color {
                        r: channel(16),
                        g: channel(8),
                        b: channel(0),
                        a: 1.0,
                    }),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        let (Some((_, group)), Some(buffer), Some((normal, invert))) = (
            &self.bind_group,
            &self.instance_buffer,
            self.pipelines.get(&format),
        ) else {
            return;
        };
        if self.frame.is_empty() {
            return;
        }
        pass.set_bind_group(0, group, &[]);
        pass.set_vertex_buffer(0, buffer.slice(..));
        for (kind, range) in &self.draws {
            pass.set_pipeline(match kind {
                Pass::Normal => normal,
                Pass::Invert => invert,
            });
            pass.draw(0..4, range.clone());
        }
    }

    /// Draw `scene` to the window surface and present it. `Ok(false)` when
    /// the frame was skipped (occluded/timeout); the caller may redraw later.
    pub fn render(
        &mut self,
        scene: &Scene<'_>,
        shaper: &mut GridShaper,
        config: &Config,
    ) -> Result<bool> {
        Ok(self.render_surface(scene, shaper, config, false)?.0)
    }
    /// Like [`Self::render`], also returning the presented pixels when the
    /// surface allows copies (frame-dump builds); otherwise `None`.
    pub fn render_and_capture(
        &mut self,
        scene: &Scene<'_>,
        shaper: &mut GridShaper,
        config: &Config,
    ) -> Result<(bool, Option<Vec<u32>>)> {
        self.render_surface(scene, shaper, config, true)
    }
    fn render_surface(
        &mut self,
        scene: &Scene<'_>,
        shaper: &mut GridShaper,
        config: &Config,
        capture: bool,
    ) -> Result<(bool, Option<Vec<u32>>)> {
        self.health.check()?;
        if scene.width == 0 || scene.height == 0 {
            return Ok((false, None));
        }
        self.prepare(scene, shaper, config);
        let Some(target) = &mut self.target else {
            bail!("GPU renderer has no window surface");
        };
        if !target.configured
            || target.config.width != scene.width
            || target.config.height != scene.height
        {
            target.config.width = scene.width;
            target.config.height = scene.height;
            target.surface.configure(&self.device, &target.config);
            target.configured = true;
        }
        let mut texture = None;
        for _ in 0..2 {
            match target.surface.get_current_texture() {
                wgpu::CurrentSurfaceTexture::Success(frame) => {
                    texture = Some(frame);
                    break;
                }
                wgpu::CurrentSurfaceTexture::Suboptimal(frame) => {
                    // Use it, and reconfigure before the next frame.
                    target.configured = false;
                    texture = Some(frame);
                    break;
                }
                wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => {
                    return Ok((false, None));
                }
                wgpu::CurrentSurfaceTexture::Outdated => {
                    target.surface.configure(&self.device, &target.config);
                }
                wgpu::CurrentSurfaceTexture::Lost => {
                    let instance = &self._instance;
                    target.surface = instance
                        .create_surface(target.window.clone())
                        .context("recreate lost GPU surface")?;
                    target.surface.configure(&self.device, &target.config);
                }
                wgpu::CurrentSurfaceTexture::Validation => {
                    self.health.check()?;
                    bail!("GPU surface validation error");
                }
            }
        }
        let Some(frame) = texture else {
            return Ok((false, None));
        };
        let format = target.config.format;
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("crow-term frame"),
            });
        self.encode(&mut encoder, &view, format, scene.background);
        let (width, height) = (frame.texture.width(), frame.texture.height());
        let padded = (width * 4).div_ceil(256) * 256;
        let copy = capture && self.target.as_ref().is_some_and(|target| target.capture);
        if copy && let Some(target) = &mut self.target {
            if target
                .readback
                .as_ref()
                .is_none_or(|(w, h, _)| (*w, *h) != (width, height))
            {
                let buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("crow-term surface readback"),
                    size: padded as u64 * height as u64,
                    usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                    mapped_at_creation: false,
                });
                target.readback = Some((width, height, buffer));
            }
            if let Some((_, _, buffer)) = &target.readback {
                encoder.copy_texture_to_buffer(
                    wgpu::TexelCopyTextureInfo {
                        texture: &frame.texture,
                        mip_level: 0,
                        origin: wgpu::Origin3d::ZERO,
                        aspect: wgpu::TextureAspect::All,
                    },
                    wgpu::TexelCopyBufferInfo {
                        buffer,
                        layout: wgpu::TexelCopyBufferLayout {
                            offset: 0,
                            bytes_per_row: Some(padded),
                            rows_per_image: Some(height),
                        },
                    },
                    wgpu::Extent3d {
                        width,
                        height,
                        depth_or_array_layers: 1,
                    },
                );
            }
        }
        self.queue.submit([encoder.finish()]);
        if let Some(target) = &self.target {
            target.window.pre_present_notify();
        }
        self.queue.present(frame);
        self.health.check()?;
        let pixels = match &self.target {
            Some(Target {
                readback: Some((_, _, buffer)),
                ..
            }) if copy => Some(self.map_pixels(
                buffer,
                width,
                height,
                format == wgpu::TextureFormat::Bgra8Unorm,
            )?),
            _ => None,
        };
        Ok((true, pixels))
    }

    /// Draw `scene` into an offscreen texture and read it back as
    /// `0x00RRGGBB` rows (tests, frame dumps, benchmarks).
    pub fn render_offscreen(
        &mut self,
        scene: &Scene<'_>,
        shaper: &mut GridShaper,
        config: &Config,
    ) -> Result<Vec<u32>> {
        let readback = self.render_offscreen_submit(scene, shaper, config)?;
        self.read_back(readback)
    }

    /// Submit an offscreen frame and wait for the GPU, without reading pixels
    /// back (benchmarks measure the same work a presented frame does).
    pub fn render_offscreen_wait(
        &mut self,
        scene: &Scene<'_>,
        shaper: &mut GridShaper,
        config: &Config,
    ) -> Result<()> {
        self.render_offscreen_submit_inner(scene, shaper, config, false)?;
        self.device
            .poll(wgpu::PollType::wait_indefinitely())
            .map_err(|error| anyhow!("GPU poll: {error}"))?;
        self.health.check()
    }

    fn render_offscreen_submit(
        &mut self,
        scene: &Scene<'_>,
        shaper: &mut GridShaper,
        config: &Config,
    ) -> Result<(u32, u32)> {
        self.render_offscreen_submit_inner(scene, shaper, config, true)
    }

    fn render_offscreen_submit_inner(
        &mut self,
        scene: &Scene<'_>,
        shaper: &mut GridShaper,
        config: &Config,
        copy: bool,
    ) -> Result<(u32, u32)> {
        self.health.check()?;
        let (width, height) = (scene.width.max(1), scene.height.max(1));
        let format = wgpu::TextureFormat::Rgba8Unorm;
        self.pipelines_for(format);
        self.prepare(scene, shaper, config);
        let padded = (width * 4).div_ceil(256) * 256;
        if self
            .offscreen
            .as_ref()
            .is_none_or(|(w, h, _, _)| (*w, *h) != (width, height))
        {
            let texture = self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("crow-term offscreen"),
                size: wgpu::Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
                view_formats: &[],
            });
            let buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("crow-term readback"),
                size: padded as u64 * height as u64,
                usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                mapped_at_creation: false,
            });
            self.offscreen = Some((width, height, texture, buffer));
        }
        let (_, _, texture, buffer) = self.offscreen.as_ref().expect("offscreen created");
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("crow-term offscreen"),
            });
        self.encode(&mut encoder, &view, format, scene.background);
        if copy {
            encoder.copy_texture_to_buffer(
                wgpu::TexelCopyTextureInfo {
                    texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                wgpu::TexelCopyBufferInfo {
                    buffer,
                    layout: wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(padded),
                        rows_per_image: Some(height),
                    },
                },
                wgpu::Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
            );
        }
        self.queue.submit([encoder.finish()]);
        Ok((width, height))
    }

    fn read_back(&mut self, (width, height): (u32, u32)) -> Result<Vec<u32>> {
        let (_, _, _, buffer) = self.offscreen.as_ref().expect("offscreen created");
        self.map_pixels(buffer, width, height, false)
    }
    /// Map a readback buffer of `width` x `height` RGBA (or BGRA) rows,
    /// padded to 256 bytes, into `0x00RRGGBB` pixels.
    fn map_pixels(
        &self,
        buffer: &wgpu::Buffer,
        width: u32,
        height: u32,
        bgra: bool,
    ) -> Result<Vec<u32>> {
        let padded = (width * 4).div_ceil(256) * 256;
        let slice = buffer.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |result| {
            let _ = tx.send(result);
        });
        self.device
            .poll(wgpu::PollType::wait_indefinitely())
            .map_err(|error| anyhow!("GPU poll: {error}"))?;
        rx.recv()
            .context("GPU readback callback dropped")?
            .map_err(|error| anyhow!("map GPU readback: {error}"))?;
        let mut pixels = Vec::with_capacity((width * height) as usize);
        {
            let data = slice
                .get_mapped_range()
                .map_err(|error| anyhow!("read GPU readback: {error}"))?;
            for row in 0..height as usize {
                let line = &data[row * padded as usize..][..width as usize * 4];
                pixels.extend(line.as_chunks::<4>().0.iter().map(|p| {
                    let (r, b) = if bgra { (p[2], p[0]) } else { (p[0], p[2]) };
                    (r as u32) << 16 | (p[1] as u32) << 8 | b as u32
                }));
            }
        }
        buffer.unmap();
        self.health.check()?;
        Ok(pixels)
    }
}

/// Append `instance` clipped to `clip` to the current normal draw range.
fn push(
    frame: &mut Vec<Instance>,
    draws: &mut Vec<(Pass, std::ops::Range<u32>)>,
    instance: Instance,
    clip: Rect,
) {
    if let Some(instance) = instance.clipped(clip) {
        match draws.last_mut() {
            Some((Pass::Normal, range)) => range.end += 1,
            _ => {
                let at = frame.len() as u32;
                draws.push((Pass::Normal, at..at + 1));
            }
        }
        frame.push(instance);
    }
}

/// One quad per run of equal neighbouring cell backgrounds (row-local).
fn background_runs(shaped: &ShapedRow) -> Vec<Instance> {
    let cw = shaped.cell_width as u32;
    let mut out = Vec::new();
    let mut start = 0;
    for run in shaped.cell_bg.chunk_by(|a, b| a == b) {
        out.push(Instance::solid(
            Rect {
                x: (start as u32 * cw) as i32,
                y: 0,
                w: run.len() as u32 * cw,
                h: shaped.height as u32,
            },
            run[0],
        ));
        start += run.len();
    }
    out
}

/// Row-local instances: background runs, glyphs, then decorations.
fn row_instances(
    shaped: &ShapedRow,
    atlas: &mut Atlas,
    shaper: &mut GridShaper,
    queue: &wgpu::Queue,
) -> std::result::Result<Vec<Instance>, Full> {
    let mut out = background_runs(shaped);
    for glyph in &shaped.glyphs {
        if let Some(entry) = atlas.get(glyph.key, shaper, queue)? {
            out.push(Instance::glyph(glyph, &entry));
        }
    }
    for &(rect, color) in &shaped.decorations {
        out.push(Instance::solid(rect, color));
    }
    Ok(out)
}

/// Layer-local instances, each already clipped to its op's clip and the
/// layer bounds (the software path rasterizes the layer into its own buffer).
fn layer_instances(
    layer: &Layer,
    atlas: &mut Atlas,
    shaper: &mut GridShaper,
    queue: &wgpu::Queue,
) -> std::result::Result<Vec<Instance>, Full> {
    let local = Rect {
        x: 0,
        y: 0,
        w: layer.width,
        h: layer.height,
    };
    let mut out = vec![Instance::solid(local, layer.background)];
    for op in &layer.ops {
        match op {
            Op::Fill { rect, color } => out.extend(Instance::solid(*rect, *color).clipped(local)),
            Op::Glyphs { clip, glyphs } => {
                let clip = clip.intersection(local);
                for glyph in glyphs {
                    if let Some(entry) = atlas.get(glyph.key, shaper, queue)? {
                        out.extend(Instance::glyph(glyph, &entry).clipped(clip));
                    }
                }
            }
            // Layers hold chrome only (see the software backend).
            Op::Row { .. } | Op::Invert { .. } | Op::Layer { .. } => {}
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clipping_shifts_texture_coordinates_with_the_quad() {
        let glyph = Instance {
            rect: [10.0, 20.0, 8.0, 12.0],
            uv: [100.0, 200.0],
            color: [1, 2, 3, 255],
            kind: MASK,
        };
        let clipped = glyph
            .clipped(Rect {
                x: 13,
                y: 0,
                w: 100,
                h: 25,
            })
            .unwrap();
        assert_eq!(clipped.rect, [13.0, 20.0, 5.0, 5.0]);
        assert_eq!(clipped.uv, [103.0, 200.0]);
        assert!(
            glyph
                .clipped(Rect {
                    x: 18,
                    y: 0,
                    w: 5,
                    h: 50
                })
                .is_none()
        );
        let moved = glyph.translated(-10, 5);
        assert_eq!(moved.rect[..2], [0.0, 25.0]);
        assert_eq!(moved.uv, glyph.uv);
    }

    #[test]
    fn row_backgrounds_merge_equal_neighbours() {
        let shaped = ShapedRow {
            width: 40,
            height: 20,
            cell_width: 10,
            background: 0,
            cell_bg: vec![1, 1, 2, 1],
            glyphs: Vec::new(),
            decorations: vec![(
                Rect {
                    x: 0,
                    y: 10,
                    w: 10,
                    h: 1,
                },
                9,
            )],
        };
        let runs = background_runs(&shaped);
        let rects: Vec<_> = runs.iter().map(|i| (i.rect, i.color[2])).collect();
        assert_eq!(
            rects,
            [
                ([0.0, 0.0, 20.0, 20.0], 1),
                ([20.0, 0.0, 10.0, 20.0], 2),
                ([30.0, 0.0, 10.0, 20.0], 1)
            ]
        );
        // Decorations come after backgrounds so they are drawn on top.
        let instance = Instance::solid(shaped.decorations[0].0, 9);
        assert_eq!(instance.color, [0, 0, 9, 255]);
        assert_eq!(instance.kind, SOLID);
    }
}
