// THE MATRIX Stage 4 — native renderer.
//
// Lifted from the measured bench-A harness (/srv/storage/work/the-matrix-bench/
// harness-a-wgpu): same wgpu 30 / winit 0.30 pipeline, same instanced-quad
// geometry, same 48x512 lattice, same Matrix rain, same activation envelope.
// What changed is the data source: bench A replayed a synthetic fixture, this
// app draws events that came off the collector's unix socket. Nothing here is
// simulated — when there is no session, no activation and no telemetry, the
// frame says exactly that.
//
// Rendering is present-on-demand: with rain off the renderer only draws when a
// collector line arrives, when the connection state changes, or while a lamp is
// still decaying. There is no idle redraw loop.

use std::collections::HashMap;
use std::sync::Arc;

use bytemuck::{Pod, Zeroable};
use winit::dpi::PhysicalSize;
use winit::window::Window;

use crate::state::{State, LAYER_SLOTS};
use crate::text::{self, Face};

/* ------------------------------------------------------------------ palette */

const BG: [f32; 4] = [0.0, 0.0, 0.0, 1.0];
const GREEN: [f32; 4] = [0.302, 1.0, 0.522, 1.0];
const BRIGHT: [f32; 4] = [0.85, 1.0, 0.88, 1.0];
const DIM: [f32; 4] = [0.36, 0.72, 0.46, 1.0];
const FAINT: [f32; 4] = [0.20, 0.44, 0.27, 1.0];
const RED: [f32; 4] = [1.0, 0.36, 0.34, 1.0];
const AMBER: [f32; 4] = [1.0, 0.74, 0.26, 1.0];
const PANEL_BG: [f32; 4] = [0.016, 0.045, 0.028, 1.0];
const PANEL_EDGE: [f32; 4] = [0.10, 0.32, 0.20, 1.0];
const CELL_OFF: [f32; 4] = [0.239, 0.278, 0.318, 1.0];

/* ------------------------------------------------------------------ constants */

pub const N_LAYERS: usize = LAYER_SLOTS;
pub const N_EXPERTS: usize = 512;

const SZ_SMALL: u32 = 13;
const SZ_BODY: u32 = 16;
const SZ_TITLE: u32 = 24;

const RAIN_FONT_PX: f32 = 13.0;
const RAIN_CHAR_W: f32 = 9.0;
const MAX_STREAMS: usize = 260;
const DENSITY: f32 = 0.55;
const IDLE_FACTOR: f32 = 0.45;
const STREAM_CHARS: usize = 26;
const RAIN_SPEED: f32 = 0.09;
const MAX_STREAM_GLYPHS: usize = 1500;
const WASH_BUDGET: usize = 1200;
const WASH_TAIL: usize = 6;
const WASH_ALPHA: f32 = 0.40;
const GREEN_MAX: f32 = 0.72;
const BLOOM_MIN: f32 = 0.5;
const BLOOM_CAP: usize = 2200;
const TRAIL_PAINT_LIMIT: usize = 9000;

const FLASH_IN: f64 = 80.0;
const HOLD: f64 = 220.0;
const FADE: f64 = 1200.0;
const FLOOR: f64 = 0.35;
const HISTORY_MS: f64 = 8000.0;

/* ------------------------------------------------------------------ gpu types */

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Globals {
    res: [f32; 2],
    scale: f32,
    _pad: f32,
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct Quad {
    pub rect: [f32; 4],
    pub color: [f32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable, Default)]
pub struct GInst {
    pub rect: [f32; 4],
    pub uv: [f32; 4],
    pub color: [f32; 4],
}

/* ------------------------------------------------------------------ overlay */

#[derive(Clone, Copy)]
struct Marker {
    layer: u16,
    expert: u16,
    t0: f64,
}

pub struct Overlay {
    cur: HashMap<u32, Marker>,
    hist: HashMap<u32, Marker>,
    pass: u32,
    pub activated: u64,
    pub expired: u64,
}

impl Overlay {
    pub fn new() -> Overlay {
        Overlay {
            cur: HashMap::with_capacity(4096),
            hist: HashMap::with_capacity(8192),
            pass: u32::MAX,
            activated: 0,
            expired: 0,
        }
    }

    fn alpha(&self, m: &Marker, now: f64) -> f32 {
        let age = now - m.t0;
        if age < 0.0 {
            return 0.0;
        }
        if age < FLASH_IN {
            return (age / FLASH_IN) as f32;
        }
        let h1 = FLASH_IN + HOLD;
        if age < h1 {
            return 1.0;
        }
        let h2 = h1 + FADE;
        if age < h2 {
            let t = (age - h1) / FADE;
            return (1.0 - (1.0 - FLOOR) * t) as f32;
        }
        let h3 = HISTORY_MS.max(h2 + 1.0);
        if age >= h3 {
            return 0.0;
        }
        let t = (age - h2) / (h3 - h2);
        (FLOOR * (1.0 - t)) as f32
    }

    pub fn begin_pass(&mut self, p: u32) {
        if self.pass != p {
            self.pass = p;
            let moved: Vec<(u32, Marker)> = self.cur.drain().collect();
            for (idx, m) in moved {
                self.hist.insert(idx, m);
            }
        }
    }

    pub fn activate(&mut self, layer: u16, expert: u16, now: f64) {
        let idx = layer as u32 * N_EXPERTS as u32 + expert as u32;
        if let Some(m) = self.cur.get(&idx).copied() {
            let a = self.alpha(&m, now);
            if let Some(mm) = self.cur.get_mut(&idx) {
                mm.t0 = now - FLASH_IN * a as f64;
            }
            self.activated += 1;
            return;
        }
        if let Some(mut m) = self.hist.remove(&idx) {
            let a = self.alpha(&m, now);
            m.t0 = now - FLASH_IN * a as f64;
            self.cur.insert(idx, m);
            self.activated += 1;
            return;
        }
        if self.cur.len() + self.hist.len() >= N_LAYERS * N_EXPERTS {
            if let Some(k) = self.hist.keys().next().copied() {
                self.hist.remove(&k);
                self.expired += 1;
            }
        }
        self.cur.insert(idx, Marker { layer, expert, t0: now });
        self.activated += 1;
    }

    pub fn tick(&mut self, now: f64) {
        let lim = HISTORY_MS.max(FLASH_IN + HOLD + FADE + 1.0);
        let dead: Vec<u32> = self
            .hist
            .iter()
            .filter(|(_, m)| now - m.t0 >= lim)
            .map(|(k, _)| *k)
            .collect();
        for k in dead {
            self.hist.remove(&k);
            self.expired += 1;
        }
    }

    pub fn live(&self) -> usize {
        self.cur.len() + self.hist.len()
    }

    /// True while any painted marker still has visible alpha at `now`.
    pub fn animating(&self, now: f64) -> bool {
        self.cur.values().any(|m| self.alpha(m, now) > 0.004)
            || self.hist.values().any(|m| self.alpha(m, now) > 0.004)
    }
}

/* ------------------------------------------------------------------ rain */

struct Stream {
    x: f32,
    y: f32,
    spd: f32,
    len: usize,
    codes: [u16; STREAM_CHARS],
    next: f32,
    mut_ms: f32,
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> f32 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        ((x >> 40) as f32) / 16777216.0
    }
    fn int(&mut self, n: usize) -> usize {
        if n == 0 { 0 } else { (self.next() * n as f32) as usize % n }
    }
}

pub struct Rain {
    streams: Vec<Stream>,
    rng: Rng,
    w: f32,
    h: f32,
    pub glyphs: Vec<char>,
}

impl Rain {
    pub fn new(w: f32, h: f32) -> Rain {
        Rain {
            streams: Vec::new(),
            rng: Rng(0x5eed1234),
            w,
            h,
            glyphs: matrix_alphabet(),
        }
    }

    fn target(&self, idle: bool) -> usize {
        let f = if idle { IDLE_FACTOR } else { 1.0 };
        ((MAX_STREAMS as f32) * DENSITY * f) as usize
    }

    fn new_stream(&mut self) -> Stream {
        let cols = ((self.w / RAIN_CHAR_W) as usize).max(1);
        let x = (self.rng.int(cols) as f32) * RAIN_CHAR_W + RAIN_CHAR_W * 0.5;
        let len = 8 + self.rng.int(STREAM_CHARS.saturating_sub(8).max(1));
        let spd = RAIN_FONT_PX * (5.0 + self.rng.next() * 22.0) * RAIN_SPEED;
        let y = -self.rng.next() * self.h * 0.6;
        let mut codes = [0u16; STREAM_CHARS];
        for c in codes.iter_mut() {
            *c = self.rng.int(self.glyphs.len()) as u16;
        }
        let mut_ms = 30.0 + self.rng.next() * 80.0;
        let next = self.rng.next() * mut_ms;
        Stream { x, y, spd, len, codes, next, mut_ms }
    }

    pub fn resize(&mut self, w: f32, h: f32) {
        self.w = w;
        self.h = h;
    }

    pub fn update(&mut self, dt: f32, idle: bool) {
        let dt = dt.clamp(0.0, 100.0);
        let target = self.target(idle);
        let mut budget = 4;
        while self.streams.len() < target && budget > 0 {
            let s = self.new_stream();
            self.streams.push(s);
            budget -= 1;
        }
        while self.streams.len() > target {
            self.streams.pop();
        }
        let n_glyphs = self.glyphs.len();
        for i in 0..self.streams.len() {
            let (mut y, spd, mut next, mut_ms, len) = {
                let s = &self.streams[i];
                (s.y, s.spd, s.next, s.mut_ms, s.len)
            };
            y += spd * dt / 1000.0;
            next -= dt;
            if next <= 0.0 {
                next = mut_ms * (0.7 + self.rng.next() * 0.6);
                let n = 1 + self.rng.int(3);
                self.streams[i].codes[0] = self.rng.int(n_glyphs) as u16;
                for _k in 1..n {
                    let slot = self.rng.int(len);
                    self.streams[i].codes[slot] = self.rng.int(n_glyphs) as u16;
                }
            }
            self.streams[i].y = y;
            self.streams[i].next = next;
            if y - (len as f32 - 1.0) * RAIN_FONT_PX > self.h {
                let s = self.new_stream();
                self.streams[i] = s;
            }
        }
    }
}

/// Katakana + digits + A-F + Latin (no L/l) + math, the same construction the
/// bench and the web page use.
pub fn matrix_alphabet() -> Vec<char> {
    let mut v: Vec<char> = Vec::new();
    for c in 0xff66u32..=0xff9d {
        if let Some(ch) = char::from_u32(c) {
            v.push(ch);
        }
    }
    for c in '0'..='9' {
        v.push(c);
    }
    for c in "ABCDEF".chars() {
        v.push(c);
    }
    for c in "GHIJKMNOPQRSTUVWXYZ".chars() {
        v.push(c);
    }
    for c in "abcdefghijkmnopqrstuvwxyz".chars() {
        v.push(c);
    }
    for c in "+-*/=<>%&|~^$!§±÷×≠≤≥∞≡⊕⊗∴∵¬∂∇{}[]()/:;·".chars() {
        v.push(c);
    }
    v
}

/* ------------------------------------------------------------------ layout */

#[derive(Clone, Copy, Default)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    fn right(&self) -> f32 {
        self.x + self.w
    }
    fn bottom(&self) -> f32 {
        self.y + self.h
    }
}

#[derive(Clone, Copy)]
pub struct Layout {
    pub w: f32,
    pub h: f32,
    pub header: Rect,
    pub left: Rect,
    pub center: Rect,
    pub right: Rect,
    pub footer: Rect,
    pub grid: Rect,
    pub cell: f32,
}

impl Layout {
    pub fn compute(w: f32, h: f32) -> Layout {
        let pad = 10.0;
        let header_h = 70.0;
        let footer_h = 22.0;
        let side = (w * 0.175).clamp(260.0, 380.0).min((w - 560.0).max(180.0));
        let top = header_h + 4.0;
        let body_h = (h - top - footer_h - 4.0).max(200.0);
        let header = Rect { x: pad, y: 6.0, w: (w - 2.0 * pad).max(100.0), h: header_h - 6.0 };
        let left = Rect { x: pad, y: top, w: side, h: body_h };
        let right = Rect { x: w - pad - side, y: top, w: side, h: body_h };
        let center = Rect {
            x: left.right() + 10.0,
            y: top,
            w: (right.x - left.right() - 20.0).max(200.0),
            h: body_h,
        };
        let footer = Rect { x: pad, y: h - footer_h, w: (w - 2.0 * pad).max(100.0), h: footer_h - 2.0 };

        // the lattice fits the centre panel, keeping a layer gutter on the left
        let gutter = 40.0;
        let avail_w = (center.w - gutter - 16.0).max(64.0);
        let avail_h = (center.h - 52.0).max(32.0);
        let cell = (avail_w / N_EXPERTS as f32).min(avail_h / N_LAYERS as f32).max(0.12);
        let gw = N_EXPERTS as f32 * cell;
        let gh = N_LAYERS as f32 * cell;
        let grid = Rect {
            x: center.x + gutter + ((avail_w - gw) / 2.0).max(0.0),
            y: center.y + 38.0 + ((avail_h - gh) / 2.0).max(0.0),
            w: gw,
            h: gh,
        };
        Layout { w, h, header, left, center, right, footer, grid, cell }
    }
}

/* ------------------------------------------------------------------ gpu text */

struct FaceGpu {
    face: Face,
    bind: wgpu::BindGroup,
    _tex: wgpu::Texture,
    _view: wgpu::TextureView,
}

/// Text quads, kept per glyph size: each size has its own atlas texture, so
/// each size gets its own buffer and its own draw call.
struct TextBuf {
    by_size: HashMap<u32, Vec<GInst>>,
}

impl TextBuf {
    fn new() -> TextBuf {
        let mut by_size = HashMap::new();
        for px in [SZ_SMALL, SZ_BODY, SZ_TITLE] {
            by_size.insert(px, Vec::with_capacity(1024));
        }
        TextBuf { by_size }
    }
}

/* ------------------------------------------------------------------ buffers */

struct DynBuf {
    buf: wgpu::Buffer,
    cap: usize,
    bind: wgpu::BindGroup,
}

impl DynBuf {
    fn new(device: &wgpu::Device, layout: &wgpu::BindGroupLayout, cap: usize, label: &str) -> DynBuf {
        let cap = cap.max(8);
        // Reserve `cap` slots of the LARGER instance type (GInst is 48 B, Quad
        // 32 B): both are uploaded into buffers created here, so sizing on the
        // smaller one would overrun the storage binding.
        let stride = std::mem::size_of::<Quad>().max(std::mem::size_of::<GInst>());
        let buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some(label),
            size: (cap * stride) as u64,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some(label),
            layout,
            entries: &[wgpu::BindGroupEntry { binding: 0, resource: buf.as_entire_binding() }],
        });
        DynBuf { buf, cap, bind }
    }

    fn upload<T: Pod>(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, data: &[T],
                      layout: &wgpu::BindGroupLayout, label: &str) -> u32 {
        if data.len() > self.cap {
            let cap = (data.len() * 2).max(64);
            let stride = std::mem::size_of::<Quad>().max(std::mem::size_of::<GInst>());
            self.buf = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size: (cap * stride) as u64,
                usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            self.bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some(label),
                layout,
                entries: &[wgpu::BindGroupEntry { binding: 0, resource: self.buf.as_entire_binding() }],
            });
            self.cap = cap;
        }
        if !data.is_empty() {
            queue.write_buffer(&self.buf, 0, bytemuck::cast_slice(data));
        }
        data.len() as u32
    }
}

/* ------------------------------------------------------------------ renderer */

#[derive(Default, Clone, Copy)]
pub struct FrameStats {
    pub quads: usize,
    pub glyphs: usize,
    pub lamps: usize,
    pub streams: usize,
}

pub struct Renderer {
    pub surface: wgpu::Surface<'static>,
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    pub config: wgpu::SurfaceConfiguration,
    globals_buf: wgpu::Buffer,
    globals_bind: wgpu::BindGroup,
    quad_pipe: wgpu::RenderPipeline,
    quad_add_pipe: wgpu::RenderPipeline,
    glyph_pipe: wgpu::RenderPipeline,
    cells: DynBuf,
    lattice: DynBuf,
    lamps: DynBuf,
    bloom: DynBuf,
    rain: DynBuf,
    wash: DynBuf,
    chrome: DynBuf,
    text_bufs: HashMap<u32, DynBuf>,
    text: HashMap<u32, FaceGpu>,
    store_layout: wgpu::BindGroupLayout,
    pub scale: f32,
    pub layout: Layout,
    pub adapter: String,
    pub format: String,
    pub present_mode: String,
}

impl Renderer {
    pub fn new(window: Arc<Window>, size: PhysicalSize<u32>, font: &str) -> Result<Renderer, String> {
        let w = size.width.max(1);
        let h = size.height.max(1);
        let scale = window.scale_factor() as f32;
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let surface = instance.create_surface(window.clone()).map_err(|e| format!("surface: {e}"))?;
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
            apply_limit_buckets: false,
        }))
        .map_err(|e| format!("no adapter: {e}"))?;
        let info = adapter.get_info();
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("matrix-app"),
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::default(),
            experimental_features: wgpu::ExperimentalFeatures::disabled(),
            memory_hints: wgpu::MemoryHints::Performance,
            trace: wgpu::Trace::Off,
        }))
        .map_err(|e| format!("device: {e}"))?;

        let caps = surface.get_capabilities(&adapter);
        let format = caps.formats.iter().copied().find(|f| f.is_srgb()).unwrap_or(caps.formats[0]);
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: w,
            height: h,
            present_mode: wgpu::PresentMode::Fifo,
            alpha_mode: caps.alpha_modes[0],
            color_space: wgpu::SurfaceColorSpace::Auto,
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &config);

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("matrix"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shader.wgsl").into()),
        });

        let bgl_globals = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("globals"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });
        let bgl_store = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("store"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Storage { read_only: true },
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });
        let bgl_tex = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("tex"),
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

        let globals_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("globals"),
            size: std::mem::size_of::<Globals>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let globals_bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("globals"),
            layout: &bgl_globals,
            entries: &[wgpu::BindGroupEntry { binding: 0, resource: globals_buf.as_entire_binding() }],
        });

        let quad_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("quad"),
            bind_group_layouts: &[Some(&bgl_globals), Some(&bgl_store)],
            immediate_size: 0,
        });
        let glyph_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("glyph"),
            bind_group_layouts: &[Some(&bgl_globals), Some(&bgl_store), Some(&bgl_tex)],
            immediate_size: 0,
        });

        let alpha_blend = wgpu::BlendState {
            color: wgpu::BlendComponent {
                src_factor: wgpu::BlendFactor::One,
                dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                operation: wgpu::BlendOperation::Add,
            },
            alpha: wgpu::BlendComponent {
                src_factor: wgpu::BlendFactor::One,
                dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                operation: wgpu::BlendOperation::Add,
            },
        };
        let add_blend = wgpu::BlendState {
            color: wgpu::BlendComponent {
                src_factor: wgpu::BlendFactor::One,
                dst_factor: wgpu::BlendFactor::One,
                operation: wgpu::BlendOperation::Add,
            },
            alpha: wgpu::BlendComponent {
                src_factor: wgpu::BlendFactor::One,
                dst_factor: wgpu::BlendFactor::One,
                operation: wgpu::BlendOperation::Add,
            },
        };
        let make_pipe = |pl: &wgpu::PipelineLayout, vs: &str, fs: &str, blend: wgpu::BlendState,
                         label: &str| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(label),
                layout: Some(pl),
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: Some(vs),
                    buffers: &[],
                    compilation_options: Default::default(),
                },
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: Some(fs),
                    targets: &[Some(wgpu::ColorTargetState {
                        format,
                        blend: Some(blend),
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                    compilation_options: Default::default(),
                }),
                primitive: wgpu::PrimitiveState::default(),
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                multiview_mask: None,
                cache: None,
            })
        };
        let quad_pipe = make_pipe(&quad_layout, "vs_quad", "fs_quad", alpha_blend, "quads");
        let quad_add_pipe = make_pipe(&quad_layout, "vs_quad", "fs_quad", add_blend, "quads-add");
        let glyph_pipe = make_pipe(&glyph_layout, "vs_glyph", "fs_glyph", alpha_blend, "glyphs");

        let cells = DynBuf::new(&device, &bgl_store, N_LAYERS * N_EXPERTS, "cells");
        let lattice = DynBuf::new(&device, &bgl_store, 1200, "lattice");
        let lamps = DynBuf::new(&device, &bgl_store, 8192, "lamps");
        let bloom = DynBuf::new(&device, &bgl_store, 8304, "bloom");
        let rain = DynBuf::new(&device, &bgl_store, 4096, "rain");
        let wash = DynBuf::new(&device, &bgl_store, 1024, "wash");
        let chrome_q = DynBuf::new(&device, &bgl_store, 512, "chrome");

        // ---- glyph faces + per-face text buffers
        let mut text = HashMap::new();
        let mut text_bufs = HashMap::new();
        for px in [SZ_SMALL, SZ_BODY, SZ_TITLE] {
            let face = Face::build(font, px, scale)?;
            let tex = device.create_texture(&wgpu::TextureDescriptor {
                label: Some("atlas"),
                size: wgpu::Extent3d {
                    width: face.size as u32,
                    height: face.size as u32,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::R8Unorm,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });
            queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &tex,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                &face.pixels,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(face.size as u32),
                    rows_per_image: Some(face.size as u32),
                },
                wgpu::Extent3d {
                    width: face.size as u32,
                    height: face.size as u32,
                    depth_or_array_layers: 1,
                },
            );
            let view = tex.create_view(&wgpu::TextureViewDescriptor::default());
            let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
                label: Some("atlas-sampler"),
                mag_filter: wgpu::FilterMode::Linear,
                min_filter: wgpu::FilterMode::Linear,
                ..Default::default()
            });
            let bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("atlas"),
                layout: &bgl_tex,
                entries: &[
                    wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&view) },
                    wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(&sampler) },
                ],
            });
            text.insert(px, FaceGpu { face, bind, _tex: tex, _view: view });
            text_bufs.insert(px, DynBuf::new(&device, &bgl_store, 2048, "text"));
        }

        let layout = Layout::compute(w as f32 / scale, h as f32 / scale);
        queue.write_buffer(&globals_buf, 0, bytemuck::bytes_of(&Globals {
            res: [w as f32, h as f32],
            scale,
            _pad: 0.0,
        }));

        let mut r = Renderer {
            surface,
            device,
            queue,
            config,
            globals_buf,
            globals_bind,
            quad_pipe,
            quad_add_pipe,
            glyph_pipe,
            cells,
            lattice,
            lamps,
            bloom,
            rain,
            wash,
            chrome: chrome_q,
            text_bufs,
            text,
            store_layout: bgl_store,
            scale,
            layout,
            adapter: format!("{} {:?}", info.backend.to_string(), info.name),
            format: format!("{format:?}"),
            present_mode: "Fifo".into(),
        };
        r.resize(size);
        Ok(r)
    }

    pub fn face(&self, px: u32) -> &Face {
        &self.text[&px].face
    }

    pub fn resize(&mut self, size: PhysicalSize<u32>) {
        if size.width == 0 || size.height == 0 {
            return;
        }
        self.config.width = size.width;
        self.config.height = size.height;
        self.surface.configure(&self.device, &self.config);
        let css_w = size.width as f32 / self.scale;
        let css_h = size.height as f32 / self.scale;
        self.layout = Layout::compute(css_w, css_h);
        self.queue.write_buffer(&self.globals_buf, 0, bytemuck::bytes_of(&Globals {
            res: [size.width as f32, size.height as f32],
            scale: self.scale,
            _pad: 0.0,
        }));
        self.rebuild_lattice();
    }

    /// The residency lattice: one quad per (layer, expert), drawn at the inactive
    /// colour. Expert residency is not part of the v1 event stream, so the app
    /// does not paint residency it cannot see; the lattice geometry itself is the
    /// real 48 x 512 from the collector's `meta` event.
    fn rebuild_lattice(&mut self) {
        let v = self.layout;
        let mut cell_quads: Vec<Quad> = Vec::with_capacity(N_LAYERS * N_EXPERTS);
        for layer in 0..N_LAYERS {
            let y = v.grid.y + (N_LAYERS - 1 - layer) as f32 * v.cell;
            for e in 0..N_EXPERTS {
                cell_quads.push(Quad {
                    rect: [v.grid.x + e as f32 * v.cell, y, v.cell, v.cell],
                    color: CELL_OFF,
                });
            }
        }
        let dev = &self.device;
        let q = &self.queue;
        let lay = &self.store_layout;
        self.cells.upload(dev, q, &cell_quads, lay, "cells");

        let mut grid_quads: Vec<Quad> = Vec::with_capacity(1200);
        for e in 0..=N_EXPERTS {
            let x = v.grid.x + e as f32 * v.cell;
            grid_quads.push(Quad {
                rect: [x, v.grid.y, 1.0, N_LAYERS as f32 * v.cell],
                color: [0.0, 0.0, 0.0, 0.55],
            });
        }
        for l in 0..=N_LAYERS {
            let y = v.grid.y + l as f32 * v.cell;
            grid_quads.push(Quad {
                rect: [v.grid.x, y, N_EXPERTS as f32 * v.cell, 1.0],
                color: [0.0, 0.0, 0.0, 0.55],
            });
        }
        self.lattice.upload(dev, q, &grid_quads, lay, "lattice");
    }

    fn push_text(&self, tb: &mut TextBuf, px: u32, x: f32, y: f32, s: &str, color: [f32; 4], max_x: f32) -> f32 {
        let face = &self.text[&px].face;
        let out = tb.by_size.get_mut(&px).expect("text buffer for size");
        text::push_str(out, face, x, y, s, color, max_x)
    }

    fn row(&self, tb: &mut TextBuf, x: f32, max_x: f32, label: &str, val: &str, col: [f32; 4], y: &mut f32) {
        let f = self.face(SZ_BODY);
        let lw = text::text_width(f, label) + 10.0;
        self.push_text(tb, SZ_SMALL, x, *y + 2.0, label, FAINT, max_x);
        let v = text::fit(f, val, (max_x - x - lw).max(20.0));
        self.push_text(tb, SZ_BODY, x + lw, *y, &v, col, max_x);
        *y += 19.0;
    }

    /// Draw one frame. `now` is milliseconds since app start (visual clock).
    pub fn draw(&mut self, st: &State, overlay: &Overlay, rain_eng: &mut Rain, now: f64, dt_ms: f32) -> FrameStats {
        let frame = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(f) | wgpu::CurrentSurfaceTexture::Suboptimal(f) => f,
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                self.surface.configure(&self.device, &self.config);
                match self.surface.get_current_texture() {
                    wgpu::CurrentSurfaceTexture::Success(f) | wgpu::CurrentSurfaceTexture::Suboptimal(f) => f,
                    _ => return FrameStats::default(),
                }
            }
            _ => return FrameStats::default(),
        };

        let v = self.layout;
        let mut q: Vec<Quad> = Vec::with_capacity(1024);
        let mut tb = TextBuf::new();
        let mut lamp_q: Vec<Quad> = Vec::with_capacity(4096);
        let mut bloom_q: Vec<Quad> = Vec::with_capacity(4096);
        let mut rain_g: Vec<GInst> = Vec::with_capacity(2048);
        let mut wash_g: Vec<GInst> = Vec::with_capacity(1400);

        // ---------------------------------------------------------- chrome
        panel(&mut q, v.header);
        panel(&mut q, v.left);
        panel(&mut q, v.center);
        panel(&mut q, v.right);

        // ---------------------------------------------------------- header
        let hx = v.header.x + 14.0;
        let tx = self.push_text(&mut tb, SZ_TITLE, hx, v.header.y + 5.0, "THE MATRIX", BRIGHT, v.header.right() - 8.0);
        self.push_text(&mut tb, SZ_BODY, tx + 16.0, v.header.y + 14.0,
            "stage 4 · native viewer · live collector socket", DIM, v.header.right() - 8.0);

        let (model_txt, model_col) = match (&st.model, st.meta_source.as_deref()) {
            (Some(m), Some("collector-store")) => (format!("MODEL {m} · collector store"), DIM),
            (Some(m), _) => (format!("MODEL {m}"), DIM),
            (None, _) => ("MODEL unavailable — no meta event".to_string(), AMBER),
        };
        let mw = text::text_width(self.face(SZ_SMALL), &model_txt) + 16.0;
        let mx = v.header.right() - 12.0 - mw;
        chip(&mut q, Rect { x: mx, y: v.header.y + 8.0, w: mw, h: 24.0 }, model_col);
        self.push_text(&mut tb, SZ_SMALL, mx + 9.0, v.header.y + 12.0, &model_txt, model_col, v.header.right() - 12.0);

        let mut cx = hx;
        let (conn_txt, conn_col) = if st.connected {
            (format!("COLLECTOR CONNECTED  {}", short(&st.socket, 34)), GREEN)
        } else {
            ("COLLECTOR NOT CONNECTED — retrying".to_string(), RED)
        };
        let row2 = v.header.y + 40.0;
        cx = chip_text(self, &mut q, &mut tb, cx, row2, &conn_txt, conn_col);
        let (tel_txt, tel_col) = match &st.status {
            Some(s) => (
                format!("TELEMETRY {} · {:.0} ev/s · {:.0} rec/s · drops {} · gaps {} · skipped {} B",
                    if s.engine_state.is_empty() { "?" } else { &s.engine_state },
                    s.events_per_s, s.records_per_s, s.drops_total, st.gaps_seen, s.skipped_bytes_total),
                if s.engine_ok { GREEN } else { AMBER },
            ),
            None => ("TELEMETRY unavailable — no telemetry_status yet".to_string(), AMBER),
        };
        let _ = chip_text(self, &mut q, &mut tb, cx, row2, &tel_txt, tel_col);

        // ---------------------------------------------------------- panels
        self.left_panel(st, &mut q, &mut tb);
        self.right_panel(st, overlay, &mut q, &mut tb);

        // ---------------------------------------------------------- centre
        let gy_top = v.center.y + 8.0;
        self.push_text(&mut tb, SZ_BODY, v.center.x + 14.0, gy_top,
            &format!("ROUTING LATTICE · {} layers × {} experts · cell {:.2} px", st.layers, st.experts, v.cell),
            DIM, v.center.right() - 14.0);
        for layer in (0..N_LAYERS).step_by(8) {
            let y = v.grid.y + (N_LAYERS - 1 - layer) as f32 * v.cell + v.cell * 0.5 - 7.0;
            let s = format!("L{layer:02}");
            let wpx = text::text_width(self.face(SZ_SMALL), &s);
            self.push_text(&mut tb, SZ_SMALL, v.grid.x - 8.0 - wpx, y, &s, FAINT, v.center.right());
        }
        for e in (0..N_EXPERTS).step_by(64) {
            let x = v.grid.x + e as f32 * v.cell;
            self.push_text(&mut tb, SZ_SMALL, x, v.grid.bottom() + 5.0, &format!("{e}"), FAINT, v.center.right());
        }
        let cline = match (&st.current_session, st.connected) {
            (Some(id), _) => format!("live session {} · passes {} · positions {} · records {}", id, st.passes, st.positions, st.records),
            (None, true) => "no active session — lattice idle, rain continues · no activity is invented".to_string(),
            (None, false) => "collector not connected — no data to draw".to_string(),
        };
        self.push_text(&mut tb, SZ_SMALL, v.center.x + 14.0, v.center.bottom() - 18.0, &cline, DIM, v.center.right() - 14.0);
        let ftext = self.footer_string(st);
        self.push_text(&mut tb, SZ_SMALL, v.footer.x + 4.0, v.footer.y + 2.0, &ftext, FAINT, v.footer.right() - 4.0);

        // ---------------------------------------------------------- lamps
        {
            let mut painted = 0usize;
            let mut bloom_n = 0usize;
            let mut push_lamp = |m: &Marker, a: f32| {
                if painted >= TRAIL_PAINT_LIMIT {
                    return;
                }
                painted += 1;
                let x = v.grid.x + m.expert as f32 * v.cell;
                let y = v.grid.y + (N_LAYERS - 1 - m.layer as usize) as f32 * v.cell;
                let g = a * GREEN_MAX;
                if g >= 0.012 {
                    lamp_q.push(Quad {
                        rect: [x, y, v.cell.max(1.0), v.cell.max(1.0)],
                        color: [0.470, 1.0, 0.651, g],
                    });
                }
                if a >= BLOOM_MIN && bloom_n < BLOOM_CAP {
                    bloom_n += 1;
                    let ba = (a - BLOOM_MIN) / (1.0 - BLOOM_MIN);
                    for (ring, alpha) in [(5.0f32, 0.16f32), (2.0, 0.30), (0.0, 0.55)] {
                        bloom_q.push(Quad {
                            rect: [x - ring, y - ring, v.cell.max(1.0) + 2.0 * ring, v.cell.max(1.0) + 2.0 * ring],
                            color: [0.776, 1.0, 0.863, alpha * ba],
                        });
                    }
                }
            };
            for (_, m) in overlay.hist.iter() {
                let a = overlay.alpha(m, now);
                if a > 0.004 {
                    push_lamp(m, a);
                }
            }
            for (_, m) in overlay.cur.iter() {
                let a = overlay.alpha(m, now);
                if a > 0.004 {
                    push_lamp(m, a);
                }
            }
        }

        // ---------------------------------------------------------- rain
        let mut streams = 0;
        if st.rain {
            rain_eng.resize(v.w, v.h);
            rain_eng.update(dt_ms, false);
            streams = rain_eng.streams.len();
            let slots = &self.text[&SZ_SMALL].face.slots;
            let ascent = self.text[&SZ_SMALL].face.ascent;
            let mut budget = MAX_STREAM_GLYPHS;
            let mut wash_budget = WASH_BUDGET;
            for s in rain_eng.streams.iter() {
                for i in 0..s.len {
                    if budget == 0 && wash_budget == 0 {
                        break;
                    }
                    let y = s.y - i as f32 * RAIN_FONT_PX;
                    if y < -RAIN_FONT_PX || y > v.h {
                        continue;
                    }
                    let ch = *rain_eng.glyphs.get(s.codes[i] as usize).unwrap_or(&'0');
                    let Some(slot) = slots.get(&ch) else { continue };
                    let y = y - ascent; // rain positions are baselines
                    let fade = 1.0 - (i as f32 / s.len as f32) * 0.85;
                    let a = fade.max(0.05) * 0.85;
                    if budget > 0 {
                        budget -= 1;
                        let lead = i == 0;
                        if lead {
                            rain_g.push(GInst {
                                rect: [s.x - slot.w * 0.5 - 2.0 + slot.ox, y + slot.oy - 1.5, slot.w + 4.0, slot.h + 3.0],
                                uv: [slot.u0, slot.v0, slot.du, slot.dv],
                                color: [0.25, 1.0, 0.45, a * 0.28],
                            });
                        }
                        let col = if lead { [0.85, 1.0, 0.88, a] } else { [0.302, 1.0, 0.522, a] };
                        rain_g.push(GInst {
                            rect: [s.x - slot.w * 0.5 + slot.ox, y + slot.oy, slot.w, slot.h],
                            uv: [slot.u0, slot.v0, slot.du, slot.dv],
                            color: col,
                        });
                    }
                    if i < WASH_TAIL && wash_budget > 0 {
                        wash_budget -= 1;
                        let wa = (a * WASH_ALPHA).min(0.14);
                        wash_g.push(GInst {
                            rect: [s.x - slot.w * 0.5 + slot.ox, y + slot.oy, slot.w, slot.h],
                            uv: [slot.u0, slot.v0, slot.du, slot.dv],
                            color: [0.302, 1.0, 0.522, wa],
                        });
                    }
                }
            }
        }

        // ---------------------------------------------------------- upload + pass
        let nl = self.lamps.upload(&self.device, &self.queue, &lamp_q, &self.store_layout, "lamps");
        let nb = self.bloom.upload(&self.device, &self.queue, &bloom_q, &self.store_layout, "bloom");
        let nr = self.rain.upload(&self.device, &self.queue, &rain_g, &self.store_layout, "rain");
        let nw = self.wash.upload(&self.device, &self.queue, &wash_g, &self.store_layout, "wash");
        let nq = self.chrome.upload(&self.device, &self.queue, &q, &self.store_layout, "chrome");
        let mut nt: HashMap<u32, u32> = HashMap::new();
        for (px, buf) in self.text_bufs.iter_mut() {
            let data = tb.by_size.get(px).cloned().unwrap_or_default();
            nt.insert(*px, buf.upload(&self.device, &self.queue, &data, &self.store_layout, "text"));
        }
        let glyph_count: usize = tb.by_size.values().map(|v| v.len()).sum();

        let view_tex = frame.texture.create_view(&wgpu::TextureViewDescriptor::default());
        let mut enc = self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("frame") });
        {
            let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("main"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view_tex,
                    resolve_target: None,
                    depth_slice: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: BG[0] as f64,
                            g: BG[1] as f64,
                            b: BG[2] as f64,
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
            pass.set_bind_group(0, &self.globals_bind, &[]);
            // chrome first: the panels are the frame, everything else draws into it
            pass.set_pipeline(&self.quad_pipe);
            pass.set_bind_group(1, &self.chrome.bind, &[]);
            pass.draw(0..6, 0..nq);
            // rain, clipped to the centre panel so the text panels stay legible
            if nr > 0 {
                pass.set_pipeline(&self.glyph_pipe);
                let sc = self.scale;
                let rx = ((v.center.x + 1.0) * sc).max(1.0) as u32;
                let ry = ((v.center.y + 1.0) * sc).max(1.0) as u32;
                let rw = (((v.center.w - 2.0) * sc) as u32).min(self.config.width.saturating_sub(rx)).max(1);
                let rh = (((v.center.h - 2.0) * sc) as u32).min(self.config.height.saturating_sub(ry)).max(1);
                pass.set_scissor_rect(rx, ry, rw, rh);
                pass.set_bind_group(1, &self.rain.bind, &[]);
                pass.set_bind_group(2, &self.text[&SZ_SMALL].bind, &[]);
                pass.draw(0..6, 0..nr);
                pass.set_scissor_rect(0, 0, self.config.width, self.config.height);
            }
            // cells + lattice
            pass.set_bind_group(1, &self.cells.bind, &[]);
            pass.draw(0..6, 0..(N_LAYERS * N_EXPERTS) as u32);
            pass.set_bind_group(1, &self.lattice.bind, &[]);
            pass.draw(0..6, 0..((N_EXPERTS as u32 + 1) + (N_LAYERS as u32 + 1)));
            // wash clipped to the lattice footprint
            if nw > 0 {
                pass.set_pipeline(&self.glyph_pipe);
                let sc = self.scale;
                let xs = ((v.grid.x * sc).max(0.0)) as u32;
                let ys = ((v.grid.y * sc).max(0.0)) as u32;
                let xw = ((v.grid.w * sc) as u32).min(self.config.width.saturating_sub(xs)).max(1);
                let yh = ((v.grid.h * sc) as u32).min(self.config.height.saturating_sub(ys)).max(1);
                pass.set_scissor_rect(xs, ys, xw, yh);
                pass.set_bind_group(1, &self.wash.bind, &[]);
                pass.set_bind_group(2, &self.text[&SZ_SMALL].bind, &[]);
                pass.draw(0..6, 0..nw);
                pass.set_scissor_rect(0, 0, self.config.width, self.config.height);
            }
            // lamps
            if nl > 0 {
                pass.set_pipeline(&self.quad_pipe);
                pass.set_bind_group(1, &self.lamps.bind, &[]);
                pass.draw(0..6, 0..nl);
            }
            // bloom
            if nb > 0 {
                pass.set_pipeline(&self.quad_add_pipe);
                pass.set_bind_group(1, &self.bloom.bind, &[]);
                pass.draw(0..6, 0..nb);
            }
            // text, one draw per glyph size (each size has its own atlas)
            pass.set_pipeline(&self.glyph_pipe);
            for px in [SZ_SMALL, SZ_BODY, SZ_TITLE] {
                let n = *nt.get(&px).unwrap_or(&0);
                if n == 0 {
                    continue;
                }
                pass.set_bind_group(1, &self.text_bufs[&px].bind, &[]);
                pass.set_bind_group(2, &self.text[&px].bind, &[]);
                pass.draw(0..6, 0..n);
            }
        }
        self.queue.submit(Some(enc.finish()));
        self.queue.present(frame);

        FrameStats { quads: q.len(), glyphs: glyph_count, lamps: lamp_q.len(), streams }
    }

    fn left_panel(&self, st: &State, q: &mut Vec<Quad>, tb: &mut TextBuf) {
        let p = self.layout.left;
        let x = p.x + 12.0;
        let max_x = p.right() - 10.0;
        let mut y = p.y + 10.0;
        self.push_text(tb, SZ_TITLE, x, y, "SESSION", GREEN, max_x);
        y += 28.0;

        let cur = st.current_session.clone();
        let s = cur.as_ref().and_then(|id| st.session(id));
        match (cur.as_ref(), s) {
            (Some(id), Some(s)) => {
                self.row(tb, x, max_x, "id", id, DIM, &mut y);
                self.row(tb, x, max_x, "tier", &s.tier, DIM, &mut y);
                let state = if s.finished { format!("finished ({})", s.end_reason) } else { "live".to_string() };
                self.row(tb, x, max_x, "state", &state, DIM, &mut y);
                self.row(tb, x, max_x, "requests",
                    &format!("{} → {} (Δ{})", s.requests_before, s.requests_after, s.request_delta), DIM, &mut y);
                self.row(tb, x, max_x, "passes", &format!("{}", s.seen_passes), DIM, &mut y);
                self.row(tb, x, max_x, "positions", &format!("{}", s.seen_positions), DIM, &mut y);
                self.row(tb, x, max_x, "records", &format!("{}", s.seen_records), DIM, &mut y);
            }
            _ => {
                self.row(tb, x, max_x, "id", "unavailable", AMBER, &mut y);
                let state = if st.connected { "no active session" } else { "not connected" };
                self.row(tb, x, max_x, "state", state, AMBER, &mut y);
            }
        }
        self.row(tb, x, max_x, "sessions", &format!("{} started / {} finished", st.sessions_started, st.sessions_finished), FAINT, &mut y);
        self.row(tb, x, max_x, "totals", &format!("{} passes · {} pos · {} rec", st.passes, st.positions, st.records), FAINT, &mut y);
        y += 14.0;

        self.push_text(tb, SZ_TITLE, x, y, "ENGINE", GREEN, max_x);
        y += 28.0;
        match &st.perf {
            Some(p) => {
                let state = if p.state.is_empty() { "unavailable".to_string() } else { p.state.clone() };
                self.row(tb, x, max_x, "state", &state, DIM, &mut y);
                self.row(tb, x, max_x, "tok/s", &p.tok_s.map(|v| format!("{v:.1}")).unwrap_or("unavailable".into()), DIM, &mut y);
                self.row(tb, x, max_x, "requests", &p.requests.map(|v| format!("{v}")).unwrap_or("unavailable".into()), DIM, &mut y);
                self.row(tb, x, max_x, "output tok", &format!("{}", p.output_tokens), DIM, &mut y);
                self.row(tb, x, max_x, "drafts", &format!("{} / {}", p.drafts_offered, p.drafts_accepted), DIM, &mut y);
            }
            None => {
                self.row(tb, x, max_x, "state", "unavailable", AMBER, &mut y);
                self.row(tb, x, max_x, "tok/s", "unavailable", AMBER, &mut y);
            }
        }
        y += 14.0;

        self.push_text(tb, SZ_TITLE, x, y, "HARDWARE", GREEN, max_x);
        y += 28.0;
        match &st.hw {
            Some(h) => {
                let g = if h.gpu_name.is_empty() { "unavailable".to_string() } else { h.gpu_name.clone() };
                self.row(tb, x, max_x, "gpu", &g, DIM, &mut y);
                self.row(tb, x, max_x, "gpu util", &h.util_pct.map(|v| format!("{v:.0}%")).unwrap_or("unavailable".into()), DIM, &mut y);
                self.row(tb, x, max_x, "gpu mem", &h.mem_used_mib.map(|v| format!("{v:.0} MiB")).unwrap_or("unavailable".into()), DIM, &mut y);
                self.row(tb, x, max_x, "gpu temp", &h.temp_c.map(|v| format!("{v:.0} C")).unwrap_or("unavailable".into()), DIM, &mut y);
                self.row(tb, x, max_x, "load1", &h.load1.map(|v| format!("{v:.2}")).unwrap_or("unavailable".into()), DIM, &mut y);
            }
            None => {
                self.row(tb, x, max_x, "gpu", "unavailable", AMBER, &mut y);
            }
        }
        y += 14.0;

        self.push_text(tb, SZ_TITLE, x, y, "STREAM HEALTH", GREEN, max_x);
        y += 28.0;
        self.row(tb, x, max_x, "last seq", &format!("{}", st.last_seq), DIM, &mut y);
        self.row(tb, x, max_x, "seq gaps",
            &format!("{} ({} missing)", st.seq_gaps, st.seq_missing),
            if st.seq_missing > 0 { AMBER } else { DIM }, &mut y);
        let drops = match &st.status {
            Some(s) => format!("{} ours / {} coll", s.sub_dropped, s.drops_total),
            None => "unavailable".to_string(),
        };
        self.row(tb, x, max_x, "drops", &drops, DIM, &mut y);
        self.row(tb, x, max_x, "gaps", &format!("{}", st.gaps_seen), DIM, &mut y);
        let replay = match &st.status {
            Some(s) => format!("{} / {} trunc", s.sub_replayed, s.sub_replay_truncated),
            None => "unavailable".to_string(),
        };
        self.row(tb, x, max_x, "replay", &replay, DIM, &mut y);

        // explicit placeholders
        let py = p.bottom() - 66.0;
        self.push_text(tb, SZ_SMALL, x, py, "not implemented this stage:", FAINT, max_x);
        self.push_text(tb, SZ_SMALL, x, py + 16.0, "· SQLITE HISTORY / SESSION LIST", FAINT, max_x);
        self.push_text(tb, SZ_SMALL, x, py + 32.0, "· PER-EXPERT DRILL-DOWN", FAINT, max_x);
        self.push_text(tb, SZ_SMALL, x, py + 48.0, "· EXPERT RESIDENCY (not in v1 stream)", FAINT, max_x);
        let _ = q;
    }

    fn right_panel(&self, st: &State, overlay: &Overlay, q: &mut Vec<Quad>, tb: &mut TextBuf) {
        let p = self.layout.right;
        let x = p.x + 12.0;
        let max_x = p.right() - 10.0;
        let mut y = p.y + 10.0;
        self.push_text(tb, SZ_TITLE, x, y, "ACTIVATION", GREEN, max_x);
        y += 24.0;
        self.push_text(tb, SZ_SMALL, x, y,
            &format!("live lamps {} · records {} (cumulative)", overlay.live(), st.records), FAINT, max_x);
        y += 18.0;
        self.push_text(tb, SZ_SMALL, x, y, "records per layer", FAINT, max_x);
        y += 16.0;

        let top = st.layer_counts.iter().copied().max().unwrap_or(0).max(1);
        let bar_x = x + 34.0;
        let bar_w = (p.w - 96.0).max(40.0);
        let limit = p.bottom() - 120.0;
        for layer in 0..N_LAYERS {
            if y + 13.0 > limit {
                break;
            }
            let c = st.layer_counts[layer];
            self.push_text(tb, SZ_SMALL, x, y, &format!("L{layer:02}"), FAINT, max_x);
            if c > 0 {
                let w = (bar_w * (c as f32 / top as f32)).max(2.0);
                q.push(Quad { rect: [bar_x, y + 3.0, w, 7.0], color: [0.18, 0.62, 0.32, 0.85] });
            }
            let label = if c > 0 { format!("{c}") } else { "·".to_string() };
            let lw = text::text_width(self.face(SZ_SMALL), &label);
            self.push_text(tb, SZ_SMALL, bar_x + bar_w + 6.0 - lw, y, &label, DIM, max_x);
            y += 13.0;
        }

        let py = p.bottom() - 106.0;
        self.push_text(tb, SZ_SMALL, x, py, "not implemented this stage:", FAINT, max_x);
        self.push_text(tb, SZ_SMALL, x, py + 16.0, "· EXPERT TOPOLOGY / CO-ACTIVATION", FAINT, max_x);
        self.push_text(tb, SZ_SMALL, x, py + 32.0, "· ROUTED TOKEN TEXT — unavailable", FAINT, max_x);
        self.push_text(tb, SZ_SMALL, x, py + 47.0, "  the v1 stream carries no token text,", FAINT, max_x);
        self.push_text(tb, SZ_SMALL, x, py + 61.0, "  so it is shown as unavailable, never invented", FAINT, max_x);
        self.push_text(tb, SZ_SMALL, x, py + 77.0, "· SCORES / WEIGHTS — unavailable", FAINT, max_x);
    }

    pub fn footer_string(&self, st: &State) -> String {
        self.footer_text(st, crate::now_unix())
    }

    pub fn footer_text(&self, st: &State, now_wall: f64) -> String {
        let age = if st.last_event_ts > 0.0 {
            format!("{:.1}s", (now_wall - st.last_event_ts).max(0.0))
        } else {
            "n/a".to_string()
        };
        format!(
            "seq {} · last event {} ago · rain {} (r) · esc quit · {}x{} · {} · {}",
            st.last_seq, age, if st.rain { "ON" } else { "OFF" },
            self.config.width, self.config.height, self.present_mode, self.adapter
        )
    }
}

/* ------------------------------------------------------------------ helpers */

fn panel(q: &mut Vec<Quad>, r: Rect) {
    q.push(Quad { rect: [r.x, r.y, r.w, r.h], color: PANEL_BG });
    let e = PANEL_EDGE;
    q.push(Quad { rect: [r.x, r.y, r.w, 1.0], color: e });
    q.push(Quad { rect: [r.x, r.bottom() - 1.0, r.w, 1.0], color: e });
    q.push(Quad { rect: [r.x, r.y, 1.0, r.h], color: e });
    q.push(Quad { rect: [r.right() - 1.0, r.y, 1.0, r.h], color: e });
}

fn chip(q: &mut Vec<Quad>, r: Rect, col: [f32; 4]) {
    q.push(Quad { rect: [r.x, r.y, r.w, r.h], color: [col[0] * 0.10, col[1] * 0.16, col[2] * 0.12, 0.9] });
    q.push(Quad { rect: [r.x, r.y, 1.0, r.h], color: [col[0], col[1], col[2], 0.8] });
    q.push(Quad { rect: [r.x, r.y, r.w, 1.0], color: [col[0], col[1], col[2], 0.35] });
    q.push(Quad { rect: [r.x, r.bottom() - 1.0, r.w, 1.0], color: [col[0], col[1], col[2], 0.35] });
    q.push(Quad { rect: [r.right() - 1.0, r.y, 1.0, r.h], color: [col[0], col[1], col[2], 0.8] });
}

fn chip_text(r: &Renderer, q: &mut Vec<Quad>, tb: &mut TextBuf, x: f32, y: f32, s: &str, col: [f32; 4]) -> f32 {
    let w = text::text_width(r.face(SZ_SMALL), s) + 18.0;
    chip(q, Rect { x, y: y - 5.0, w, h: 23.0 }, col);
    r.push_text(tb, SZ_SMALL, x + 9.0, y - 1.0, s, col, x + w);
    x + w + 8.0
}

fn short(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let keep: String = s.chars().rev().take(n - 2).collect::<Vec<_>>().into_iter().rev().collect();
        format!("..{keep}")
    }
}
