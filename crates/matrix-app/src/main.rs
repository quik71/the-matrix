// THE MATRIX Stage 5 — native application.
//
//   collector unix socket (v1 NDJSON)  ->  in-app state  ->  winit + wgpu window
//
// No browser, no webview, no HTTP client: the app reads the collector's unix
// socket, keeps its own state from the events it actually received, and draws
// that state with the same instanced-quad pipeline the measured bench-A harness
// used. Anything the event stream does not carry is rendered as unavailable.
//
// Stage 5 adds the monitoring workstation (SESSIONS / ACTIVATION / ENGINE PERF /
// STREAM HEALTH / HARDWARE + the interaction lattice), explicit --monitor
// placement verified against the compositor, and a repaint rule that presents a
// frame only when a displayed value actually changed.

mod client;
mod json;
mod render;
mod state;
mod text;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, NamedKey};
use winit::window::{Window, WindowId};

use client::Msg;
use render::{Overlay, Rain, Renderer, VIEW_ACTIVATION, VIEW_COACT, VIEW_OVERVIEW, VIEW_PERF,
             VIEW_SESSIONS, VIEW_STREAM};
use state::State;

pub fn now_unix() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn handle_signal(_sig: i32) {
    STOP.store(true, Ordering::SeqCst);
}

unsafe extern "C" {
    fn signal(sig: i32, handler: extern "C" fn(i32)) -> usize;
}

const TITLE: &str = "THE MATRIX";

/* ------------------------------------------------------------------ args */

struct Args {
    socket: String,
    font: String,
    stats: Option<String>,
    stats_ms: u64,
    rain: bool,
    width: u32,
    height: u32,
    wm_fullscreen: bool,
    meta_db: Option<String>,
    /// Monitor to place the window on. "active" (the default) means whatever
    /// monitor Hyprland had focused when the app started.
    monitor: String,
    view: u8,
    // ---- Stage 6
    banner: bool,
    banner_interval: f64,
    motion_reduced: bool,
    trails: bool,
    rain_density: f32,
    rain_speed: f32,
    glow: f32,
    zoom: f32,
    pan_x: f32,
    pan_y: f32,
    /// Labelled INPUT SELF-TEST hook: a control file whose lines inject the same
    /// commands the keyboard does (zoom/pan/view/trails). It changes UI state
    /// only — it cannot create telemetry.
    ctl: Option<String>,
}

fn parse_view(s: &str) -> u8 {
    match s.to_ascii_lowercase().as_str() {
        "1" | "overview" => VIEW_OVERVIEW,
        "2" | "activation" => VIEW_ACTIVATION,
        "3" | "sessions" => VIEW_SESSIONS,
        "4" | "perf" => VIEW_PERF,
        "5" | "stream" => VIEW_STREAM,
        "6" | "coact" | "co-activation" => VIEW_COACT,
        _ => VIEW_OVERVIEW,
    }
}

fn parse_args() -> Args {
    let mut a = Args {
        socket: "/home/main/the-matrix/run/collector.sock".into(),
        font: "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc".into(),
        stats: None,
        stats_ms: 1000,
        rain: true,
        width: 1600,
        height: 900,
        wm_fullscreen: true,
        meta_db: None,
        monitor: "active".into(),
        view: VIEW_OVERVIEW,
        banner: true,
        banner_interval: 26.0,
        motion_reduced: false,
        trails: true,
        rain_density: 0.55,
        rain_speed: 1.0,
        glow: 0.60,
        zoom: 1.0,
        pan_x: 0.0,
        pan_y: 0.0,
        ctl: None,
    };
    let argv: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < argv.len() {
        let need = |i: usize| -> String { argv.get(i + 1).cloned().unwrap_or_default() };
        match argv[i].as_str() {
            "--socket" => {
                a.socket = need(i);
                i += 2;
            }
            "--font" => {
                a.font = need(i);
                i += 2;
            }
            "--stats" => {
                a.stats = Some(need(i));
                i += 2;
            }
            "--stats-ms" => {
                a.stats_ms = need(i).parse().unwrap_or(1000).max(50);
                i += 2;
            }
            "--rain" => {
                a.rain = need(i) != "off";
                i += 2;
            }
            "--width" => {
                a.width = need(i).parse().unwrap_or(1600).max(640);
                i += 2;
            }
            "--height" => {
                a.height = need(i).parse().unwrap_or(900).max(400);
                i += 2;
            }
            "--meta-db" => {
                a.meta_db = Some(need(i));
                i += 2;
            }
            "--monitor" => {
                a.monitor = need(i);
                i += 2;
            }
            "--view" => {
                a.view = parse_view(&need(i));
                i += 2;
            }
            "--banner" => {
                a.banner = need(i) != "off";
                i += 2;
            }
            "--banner-interval" => {
                let v: f64 = need(i).parse().unwrap_or(26.0);
                a.banner_interval = v.clamp(4.0, 600.0);
                i += 2;
            }
            "--motion" => {
                a.motion_reduced = need(i) == "reduced";
                i += 2;
            }
            "--trails" => {
                a.trails = need(i) != "off";
                i += 2;
            }
            "--rain-density" => {
                let v: f32 = need(i).parse().unwrap_or(0.55);
                a.rain_density = v.clamp(0.05, 1.0);
                i += 2;
            }
            "--rain-speed" => {
                let v: f32 = need(i).parse().unwrap_or(1.0);
                a.rain_speed = v.clamp(0.2, 3.0);
                i += 2;
            }
            "--glow" => {
                let v: f32 = need(i).parse().unwrap_or(0.6);
                a.glow = v.clamp(0.0, 1.0);
                i += 2;
            }
            "--zoom" => {
                a.zoom = need(i).parse().unwrap_or(1.0);
                i += 2;
            }
            "--pan-x" => {
                a.pan_x = need(i).parse().unwrap_or(0.0);
                i += 2;
            }
            "--pan-y" => {
                a.pan_y = need(i).parse().unwrap_or(0.0);
                i += 2;
            }
            "--ctl" => {
                a.ctl = Some(need(i));
                i += 2;
            }
            "--wm-fullscreen" => {
                a.wm_fullscreen = need(i) != "off";
                i += 2;
            }
            "--help" | "-h" => {
                println!(
                    "matrix-app — THE MATRIX native viewer (stage 5)\n\
                     --socket <path>        collector unix socket (default /home/main/the-matrix/run/collector.sock)\n\
                     --font <ttf/ttc>       font for the glyph atlas\n\
                     --stats <path|->       write the state/panel dump as JSON every --stats-ms (or print to stdout)\n\
                     --stats-ms <n>         dump interval in ms (1000)\n\
                     --rain <on|off>        Matrix rain animation (on)\n\
                     --width/--height <n>   initial window size (1600x900); the WM makes it fullscreen\n\
                     --monitor <name|active> place the window on this Hyprland monitor (default: the focused one)\n\
                     --view <1..6|name>     overview | activation | sessions | perf | stream | coact\n\
                     --wm-fullscreen <on|off> ask Hyprland for WM-fullscreen on the target monitor (on)\n\
                     --meta-db <sqlite>     fallback: read the collector's recorded meta row when the socket\n\
                                           never delivered one (labelled 'collector store' in the UI)\n\
                     --banner <on|off>      FOLLOW THE WHITE RABBIT banner (on; obeys the ambient gate)\n\
                     --banner-interval <s>  banner repeat interval in seconds (26)\n\
                     --motion <full|reduced> reduced disables rain, banner, bursts and pulses\n\
                     --trails <on|off>      historical trail bars on the lattice (on)\n\
                     --rain-density <f>     rain density 0.05..1.0 (0.55)\n\
                     --rain-speed <f>       rain speed multiplier 0.2..3.0 (1.0)\n\
                     --glow <f>             activation bloom intensity 0..1 (0.6)\n\
                     --zoom <f>             initial lattice zoom 0.5..12 (1.0)\n\
                     --pan-x/--pan-y <px>   initial lattice pan in CSS px (0)\n\
                     --ctl <path>           INPUT SELF-TEST hook: a control file whose lines inject the\n\
                                            same commands the keyboard does (zoom/pan/view/trails);\n\
                                            UI state only, it cannot create telemetry\n\
                     keys: esc quit · r rain · f fullscreen · 1-6 views · j/k layer · +/- zoom ·\n\
                           w/a/s/d pan · 0 reset · t trails · b banner"
                );
                std::process::exit(0);
            }
            other => {
                if other.starts_with("--") {
                    eprintln!("unknown flag {other} (try --help)");
                    std::process::exit(2);
                }
                i += 1;
            }
        }
    }
    a
}

/* ------------------------------------------------------------------ app */

struct App {
    args: Args,
    window: Option<Arc<Window>>,
    gpu: Option<Renderer>,
    st: Arc<Mutex<State>>,
    overlay: Overlay,
    rain: Rain,
    rx: Option<Receiver<Msg>>,
    t0: Instant,
    last_draw_ms: f64,
    redraws: u64,
    frames: u64,
    spawned: bool,
    next_frame: Instant,
    last_fp: u64,
    resize_pending: bool,
    /// Set when the loop asked for a redraw because a lamp was still fading or
    /// the rain was running; a frame inside this grace window is animation, not
    /// a redundant repaint.
    anim_until: Instant,
    /// messages applied since the previous presented frame (diagnostic for the
    /// "a frame was drawn without any new data" case)
    msgs_since_draw: u64,
    /// the outstanding redraw was requested by the animation tick
    req_anim: bool,
    /// a redraw was requested because a displayed value changed
    req_change: bool,
    last_groups: Vec<(&'static str, u64)>,
    /// the decorative banner is on screen right now (its own animation clock)
    banner_anim: bool,
}

impl App {
    fn elapsed_ms(&self) -> f64 {
        self.t0.elapsed().as_secs_f64() * 1000.0
    }

    fn drain(&mut self) {
        let Some(rx) = self.rx.as_ref() else { return };
        let mut st = self.st.lock().unwrap_or_else(|p| p.into_inner());
        let mut got = 0usize;
        loop {
            match rx.try_recv() {
                Ok(Msg::Line(l)) => {
                    if let Some((pass, layer, ids)) = st.apply(&l) {
                        let now = self.elapsed_ms();
                        self.overlay.begin_pass(pass);
                        for e in ids {
                            self.overlay.activate(layer, e, now);
                        }
                    }
                    got += 1;
                }
                Ok(Msg::Connected { wall }) => {
                    st.connected = true;
                    st.connects += 1;
                    st.connected_since = wall;
                    st.connect_wall = wall;
                }
                Ok(Msg::Disconnected { reason, wall }) => {
                    st.connected = false;
                    st.disconnects += 1;
                    st.connected_since = 0.0;
                    st.last_disconnect_reason = reason;
                    let _ = wall;
                }
                Ok(Msg::ConnectError { err, wall }) => {
                    st.connect_errors += 1;
                    st.connect_error_wall = wall;
                    if st.connected {
                        st.connected = false;
                        st.disconnects += 1;
                    }
                    st.last_disconnect_reason = format!("connect: {err}");
                }
                Err(_) => break,
            }
            self.msgs_since_draw += 1;
            if got > 200_000 {
                break;
            }
        }
        drop(st);
        self.overlay.tick(self.elapsed_ms());
    }

    /// True when a visible value changed (or a lamp is still fading). This is
    /// the whole present-on-demand rule: no animation, no change, no frame.
    fn visual_change(&mut self) -> bool {
        let fp = {
            let st = self.st.lock().unwrap_or_else(|p| p.into_inner());
            st.fingerprint()
        };
        let anim = self.overlay.animating(self.elapsed_ms()) || self.banner_anim;
        if fp != self.last_fp || anim {
            true
        } else {
            let mut st = self.st.lock().unwrap_or_else(|p| p.into_inner());
            st.wakes_skipped += 1;
            false
        }
    }

    /// Ask for one frame and remember that it is still owed: a Wayland client
    /// cannot force a present, so a request can sit in the compositor for a long
    /// time (idle session). The dump exposes both the request count and whether
    /// one is still pending, which is what makes the idle measurement honest.
    fn request_frame(&mut self) {
        if let Some(w) = self.window.as_ref() {
            {
                let mut st = self.st.lock().unwrap_or_else(|p| p.into_inner());
                st.requested_frames += 1;
                st.pending_redraw = true;
            }
            w.request_redraw();
        }
    }

    /// Advance the decorative banner clock and report whether it is visible.
    /// The ambient gate is the same one the rain obeys, so `--rain off` (the
    /// strict idle measurement) silences the banner as well.
    fn step_banner(&mut self, now: f64) -> bool {
        let mut st = self.st.lock().unwrap_or_else(|p| p.into_inner());
        let ambient = st.rain && !st.motion_reduced;
        let on = st.banner.step(now, ambient);
        on
    }

    fn spawn_client(&mut self, proxy: EventLoopProxy<()>) {
        if self.spawned {
            return;
        }
        self.spawned = true;
        let (tx, rx): (Sender<Msg>, Receiver<Msg>) = channel();
        self.rx = Some(rx);
        let socket = self.args.socket.clone();
        let p = Arc::new(move || {
            let _ = proxy.send_event(());
        });
        let stop = Arc::new(AtomicBool::new(false));
        client::spawn(socket, tx, p, stop);
    }
}

impl ApplicationHandler<()> for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title(TITLE)
            .with_inner_size(LogicalSize::new(self.args.width as f64, self.args.height as f64))
            .with_min_inner_size(LogicalSize::new(1024.0, 600.0));
        let window = Arc::new(el.create_window(attrs).expect("window"));
        let size = window.inner_size();
        eprintln!("window: {TITLE} {}x{} physical", size.width, size.height);
        let gpu = match Renderer::new(window.clone(), size, &self.args.font) {
            Ok(g) => g,
            Err(e) => {
                eprintln!("renderer init failed: {e}");
                el.exit();
                return;
            }
        };
        eprintln!(
            "gpu: {} surface {:?} {}x{} (present {})",
            gpu.adapter, gpu.format, gpu.config.width, gpu.config.height, "Fifo"
        );
        self.gpu = Some(gpu);
        self.window = Some(window.clone());
        {
            let mut st = self.st.lock().unwrap_or_else(|p| p.into_inner());
            st.rain = self.args.rain;
            st.view = self.args.view;
            st.set_zoom(self.args.zoom);
            st.pan_x = self.args.pan_x;
            st.pan_y = self.args.pan_y;
            st.trails = self.args.trails;
            st.motion_reduced = self.args.motion_reduced;
            st.rain_density = self.args.rain_density;
            st.rain_speed = self.args.rain_speed;
            st.glow = self.args.glow;
            st.banner = state::Banner::new(self.args.banner, self.args.banner_interval);
            if st.motion_reduced {
                st.rain = false;
            }
        }
        self.rain = Rain::new(size.width as f32, size.height as f32);
        self.t0 = Instant::now();
        if self.args.wm_fullscreen {
            let target = if self.args.monitor == "active" { None } else { Some(self.args.monitor.clone()) };
            let st2 = self.st.clone();
            std::thread::spawn(move || placement_task(target, st2));
        } else {
            let st2 = self.st.clone();
            let req = self.args.monitor.clone();
            std::thread::spawn(move || read_placement(req, st2, false));
        }
        self.request_frame();
    }

    fn user_event(&mut self, _el: &ActiveEventLoop, _ev: ()) {
        self.drain();
        if self.visual_change() {
            self.req_change = true;
            self.request_frame();
        }
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _id: WindowId, ev: WindowEvent) {
        match ev {
            WindowEvent::CloseRequested => el.exit(),
            WindowEvent::Resized(size) => {
                if let Some(g) = self.gpu.as_mut() {
                    g.resize(size);
                    eprintln!("resize: surface {}x{} css {:.0}x{:.0} · lattice cell {:.2}px",
                        g.config.width, g.config.height,
                        g.layout.w, g.layout.h, g.layout.cell);
                }
                self.resize_pending = true;
                self.request_frame();
            }
            WindowEvent::KeyboardInput { event, .. } => {
                if event.state == ElementState::Pressed {
                    let mut dirty = false;
                    match event.logical_key {
                        Key::Named(NamedKey::Escape) => el.exit(),
                        Key::Character(ref c) if c.eq_ignore_ascii_case("r") => {
                            let mut st = self.st.lock().unwrap_or_else(|p| p.into_inner());
                            st.rain = !st.rain;
                            drop(st);
                            dirty = true;
                        }
                        Key::Character(ref c) if c.eq_ignore_ascii_case("f") => {
                            std::thread::spawn(wm_fullscreen_toggle);
                        }
                        Key::Character(ref c) if c.eq_ignore_ascii_case("j") => {
                            let mut st = self.st.lock().unwrap_or_else(|p| p.into_inner());
                            st.sel_pinned = true;
                            st.sel_layer = (st.sel_layer + 1) % st.layers.max(1);
                            drop(st);
                            dirty = true;
                        }
                        Key::Character(ref c) if c.eq_ignore_ascii_case("k") => {
                            let mut st = self.st.lock().unwrap_or_else(|p| p.into_inner());
                            st.sel_pinned = true;
                            let n = st.layers.max(1);
                            st.sel_layer = (st.sel_layer + n - 1) % n;
                            drop(st);
                            dirty = true;
                        }
                        Key::Named(NamedKey::ArrowDown) => {
                            let mut st = self.st.lock().unwrap_or_else(|p| p.into_inner());
                            st.sel_pinned = true;
                            st.sel_layer = (st.sel_layer + 1) % st.layers.max(1);
                            drop(st);
                            dirty = true;
                        }
                        Key::Named(NamedKey::ArrowUp) => {
                            let mut st = self.st.lock().unwrap_or_else(|p| p.into_inner());
                            st.sel_pinned = true;
                            let n = st.layers.max(1);
                            st.sel_layer = (st.sel_layer + n - 1) % n;
                            drop(st);
                            dirty = true;
                        }
                        Key::Character(ref c) if c == "+" || c == "=" => {
                            let mut st = self.st.lock().unwrap_or_else(|p| p.into_inner());
                            st.zoom_by(1.25);
                            drop(st);
                            dirty = true;
                        }
                        Key::Character(ref c) if c == "-" || c == "_" => {
                            let mut st = self.st.lock().unwrap_or_else(|p| p.into_inner());
                            st.zoom_by(0.8);
                            drop(st);
                            dirty = true;
                        }
                        Key::Character(ref c) if c == "0" => {
                            let mut st = self.st.lock().unwrap_or_else(|p| p.into_inner());
                            st.reset_view();
                            drop(st);
                            dirty = true;
                        }
                        Key::Character(ref c) if c.eq_ignore_ascii_case("w") => {
                            let mut st = self.st.lock().unwrap_or_else(|p| p.into_inner());
                            st.pan_by(0.0, -80.0);
                            drop(st);
                            dirty = true;
                        }
                        Key::Character(ref c) if c.eq_ignore_ascii_case("s") => {
                            let mut st = self.st.lock().unwrap_or_else(|p| p.into_inner());
                            st.pan_by(0.0, 80.0);
                            drop(st);
                            dirty = true;
                        }
                        Key::Character(ref c) if c.eq_ignore_ascii_case("a") => {
                            let mut st = self.st.lock().unwrap_or_else(|p| p.into_inner());
                            st.pan_by(-80.0, 0.0);
                            drop(st);
                            dirty = true;
                        }
                        Key::Character(ref c) if c.eq_ignore_ascii_case("d") => {
                            let mut st = self.st.lock().unwrap_or_else(|p| p.into_inner());
                            st.pan_by(80.0, 0.0);
                            drop(st);
                            dirty = true;
                        }
                        Key::Character(ref c) if c.eq_ignore_ascii_case("t") => {
                            let mut st = self.st.lock().unwrap_or_else(|p| p.into_inner());
                            st.trails = !st.trails;
                            drop(st);
                            dirty = true;
                        }
                        Key::Character(ref c) if c.eq_ignore_ascii_case("b") => {
                            let now = self.elapsed_ms();
                            let mut st = self.st.lock().unwrap_or_else(|p| p.into_inner());
                            st.banner.trigger(now);
                            drop(st);
                            dirty = true;
                        }
                        Key::Character(ref c) if c.eq_ignore_ascii_case("c") => {
                            let mut st = self.st.lock().unwrap_or_else(|p| p.into_inner());
                            st.view = VIEW_COACT;
                            drop(st);
                            dirty = true;
                        }
                        Key::Character(ref c) => {
                            let cs = c.to_string();
                            let v = match cs.as_str() {
                                "1" => Some(VIEW_OVERVIEW),
                                "2" => Some(VIEW_ACTIVATION),
                                "3" => Some(VIEW_SESSIONS),
                                "4" => Some(VIEW_PERF),
                                "5" => Some(VIEW_STREAM),
                                "6" => Some(VIEW_COACT),
                                _ => None,
                            };
                            if let Some(v) = v {
                                let mut st = self.st.lock().unwrap_or_else(|p| p.into_inner());
                                st.view = v;
                                drop(st);
                                dirty = true;
                            }
                        }
                        _ => {}
                    }
                    if dirty {
                        self.request_frame();
                    }
                }
            }
            WindowEvent::RedrawRequested => {
                {
                    let mut st = self.st.lock().unwrap_or_else(|p| p.into_inner());
                    st.pending_redraw = false;
                }
                self.drain();
                let now = self.elapsed_ms();
                let dt = (now - self.last_draw_ms) as f32;
                self.last_draw_ms = now;
                let fp = {
                    let st = self.st.lock().unwrap_or_else(|p| p.into_inner());
                    st.fingerprint()
                };
                let banner_on = self.step_banner(now);
                let animating = self.overlay.animating(now) || banner_on;
                let rain_on = self.st.lock().unwrap_or_else(|p| p.into_inner()).rain;
                let in_anim_grace = Instant::now() < self.anim_until || self.req_anim;
                self.req_anim = false;
                // A compositor can hand us a frame nobody asked for. If every
                // displayed value is already on screen, presenting an identical
                // pixel buffer is exactly the "continuous redraw when nothing
                // changes" the spec forbids: decline it and count it.
                let redundant = fp == self.last_fp && !animating && !rain_on
                    && !self.resize_pending && !in_anim_grace && !self.req_change;
                self.req_change = false;
                if redundant && self.last_fp != u64::MAX {
                    let mut st = self.st.lock().unwrap_or_else(|p| p.into_inner());
                    st.repaints_unchanged += 1;
                    st.pending_redraw = false;
                    return;
                }
                {
                    let mut st = self.st.lock().unwrap_or_else(|p| p.into_inner());
                    if self.resize_pending {
                        st.frames_resize += 1;
                    } else if fp != self.last_fp {
                        st.frames_change += 1;
                        if self.msgs_since_draw == 0 {
                            // a frame drawn with no new message: attribute it
                            let g = st.fingerprint_groups();
                            let changed: Vec<&str> = g.iter().zip(self.last_groups.iter())
                                .filter(|((_, a), (_, b))| a != b).map(|((n, _), _)| *n).collect();
                            eprintln!("frame: state groups changed without a new message: {:?}", changed);
                        }
                    } else if animating || rain_on || in_anim_grace {
                        st.frames_anim += 1;
                    } else {
                        st.frames_spurious += 1;
                        st.repaints_unchanged += 1;
                    }
                }
                self.resize_pending = false;
                self.msgs_since_draw = 0;
                if let (Some(g), Some(_w)) = (self.gpu.as_mut(), self.window.as_ref()) {
                    let mut st = self.st.lock().unwrap_or_else(|p| p.into_inner());
                    g.relayout(st.view, st.layers, st.experts, st.zoom, st.pan_x, st.pan_y);
                    // record the geometry the renderer actually used, so the dump
                    // reports the viewport instead of the request
                    let gr = g.layout.grid;
                    st.view_rect = (gr.x, gr.y, gr.w, gr.h);
                    st.view_cell = g.layout.cell;
                    st.view_cell_h = g.layout.cell_h;
                    st.visible_cells = g.visible_cells();
                    self.overlay.decay(dt);
                    let fs = g.draw(&st, &self.overlay, &mut self.rain, now, dt);
                    self.frames += 1;
                    self.redraws += 1;
                    st.frames = self.frames;
                    st.redraw_requests = self.redraws;
                    st.last_frame = fs;
                }
                self.last_fp = fp;
                {
                    let st = self.st.lock().unwrap_or_else(|p| p.into_inner());
                    self.last_groups = st.fingerprint_groups();
                }
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        if STOP.load(Ordering::SeqCst) {
            eprintln!("signal: about_to_wait sees STOP, exiting");
            el.exit();
            return;
        }
        let now = self.elapsed_ms();
        let banner_on = self.step_banner(now);
        let rain = self.st.lock().unwrap_or_else(|p| p.into_inner()).rain;
        // Present-on-demand with a 60 fps budget: animate only while the rain is
        // on or a lamp is still decaying, and never faster than one frame per
        // 16 ms. With no animation and no events the loop blocks in Wait with no
        // timer at all — zero redraws, zero wake-ups.
        if rain || banner_on || self.overlay.animating(now) {
            let now_i = Instant::now();
            if now_i >= self.next_frame {
                self.next_frame = now_i + Duration::from_millis(16);
                self.anim_until = now_i + Duration::from_millis(48);
                self.req_anim = true;
                self.request_frame();
            }
            el.set_control_flow(ControlFlow::WaitUntil(self.next_frame));
        } else {
            el.set_control_flow(ControlFlow::Wait);
        }
    }
}

/* ------------------------------------------------------------------ drive */

fn hypr_env_ok() -> bool {
    !std::env::var("HYPRLAND_INSTANCE_SIGNATURE").unwrap_or_default().is_empty()
}

struct MonInfo {
    id: i64,
    name: String,
    focused: bool,
    w: f64,
    h: f64,
    scale: f64,
}

fn monitors() -> Vec<MonInfo> {
    let out = match std::process::Command::new("hyprctl").args(["-j", "monitors"]).output() {
        Ok(o) if o.status.success() => o,
        _ => return Vec::new(),
    };
    let v = match json::parse(&out.stdout) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let list = match v {
        json::Value::Arr(a) => a,
        _ => return Vec::new(),
    };
    let mut res = Vec::new();
    for m in list {
        let name = m.get("name").and_then(|x| x.as_str()).unwrap_or("").to_string();
        if name.is_empty() {
            continue;
        }
        res.push(MonInfo {
            id: m.get("id").and_then(|x| x.as_i64()).unwrap_or(-1),
            name,
            focused: matches!(m.get("focused"), Some(json::Value::Bool(true))),
            w: m.get("width").and_then(|x| x.as_f64()).unwrap_or(0.0),
            h: m.get("height").and_then(|x| x.as_f64()).unwrap_or(0.0),
            scale: m.get("scale").and_then(|x| x.as_f64()).unwrap_or(1.0),
        });
    }
    res
}

/// This window's client record: (monitor id, x, y, w, h, fullscreen).
fn client_geometry() -> Option<(i64, i64, i64, i64, i64, i64)> {
    let out = std::process::Command::new("hyprctl").args(["-j", "clients"]).output().ok()?;
    let v = json::parse(&out.stdout).ok()?;
    let list = match v {
        json::Value::Arr(a) => a,
        _ => return None,
    };
    for c in list {
        if c.get("title").and_then(|t| t.as_str()) != Some(TITLE) {
            continue;
        }
        let num = |k: &str, i: usize| -> i64 {
            c.get(k)
                .and_then(|s| match s {
                    json::Value::Arr(a) => a.get(i).and_then(|z| z.as_i64()),
                    other => other.as_i64(),
                })
                .unwrap_or(-1)
        };
        let fs = c.get("fullscreen").and_then(|z| z.as_i64()).unwrap_or(-1);
        return Some((num("monitor", 0), num("at", 0), num("at", 1), num("size", 0), num("size", 1), fs));
    }
    None
}

fn dispatch(call: &str) -> String {
    match std::process::Command::new("hyprctl").args(["dispatch", call]).output() {
        Ok(o) => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        Err(e) => format!("hyprctl unavailable: {e}"),
    }
}

/// Place the window on `target` (or the focused monitor when None), ask for WM
/// fullscreen, then read the result back from the compositor and record what was
/// actually achieved. The app never assumes the placement it asked for.
fn placement_task(target: Option<String>, st: Arc<Mutex<State>>) {
    std::thread::sleep(Duration::from_millis(300));
    if !hypr_env_ok() {
        let mut s = st.lock().unwrap_or_else(|p| p.into_inner());
        s.placement.requested = target.clone().unwrap_or_else(|| "active".into());
        s.placement.note = "no HYPRLAND_INSTANCE_SIGNATURE: placement not verified".into();
        eprintln!("placement: no HYPRLAND_INSTANCE_SIGNATURE, leaving the window tiled");
        return;
    }
    let mons = monitors();
    let want = match &target {
        Some(t) => t.clone(),
        None => mons.iter().find(|m| m.focused).map(|m| m.name.clone()).unwrap_or_default(),
    };
    {
        let mut s = st.lock().unwrap_or_else(|p| p.into_inner());
        s.placement.requested = target.clone().unwrap_or_else(|| "active".into());
    }
    let fs_call = format!("hl.dsp.window.fullscreen_state({{internal=2, client=2, window=\"title:{TITLE}\"}})");
    let mv_call = format!("hl.dsp.window.move({{window=\"title:{TITLE}\", monitor=\"{want}\"}})");
    for attempt in 1..=12 {
        if target.is_some() {
            let r = dispatch(&mv_call);
            if r != "ok" {
                eprintln!("placement: move -> {r}");
            }
        }
        let r = dispatch(&fs_call);
        if r != "ok" {
            eprintln!("placement: fullscreen -> {r}");
        }
        std::thread::sleep(Duration::from_millis(320));
        if let Some((mid, x, y, w, h, fs)) = client_geometry() {
            let mname = mons.iter().find(|m| m.id == mid).map(|m| m.name.clone()).unwrap_or_default();
            let want_m = mons.iter().find(|m| m.name == want);
            let size_ok = match want_m {
                Some(m) => m.w > 0.0
                    && ((w as f64) - m.w / m.scale).abs() <= 3.0
                    && ((h as f64) - m.h / m.scale).abs() <= 3.0,
                None => false,
            };
            let ok = mname == want && fs == 2 && size_ok;
            {
                let mut s = st.lock().unwrap_or_else(|p| p.into_inner());
                s.placement.monitor = mname.clone();
                s.placement.monitor_id = mid;
                s.placement.x = x;
                s.placement.y = y;
                s.placement.w = w;
                s.placement.h = h;
                s.placement.fullscreen = fs;
                s.placement.verified = ok;
                eprintln!("placement-write at {:.1}s: verified={} monitor={:?} {}x{}",
                    crate::now_unix() - s.started_wall, ok, mname, w, h);
                s.placement.note = if ok {
                    format!("window on {mname} {w}x{h} at {x},{y}, fullscreen=2 (verified against hyprctl)")
                } else {
                    format!("attempt {attempt}: on {mname:?} {w}x{h} fullscreen={fs}, wanted {want:?} size {size_ok}")
                };
            }
            if ok {
                eprintln!("placement: window on {} {}x{} at {},{} fullscreen=2 (asked for {})", mname, w, h, x, y, want);
                return;
            }
            eprintln!("placement: attempt {attempt}: on {mname:?} {w}x{h} fs {fs}, wanted {want:?}");
        } else {
            eprintln!("placement: attempt {attempt}: window not mapped yet");
        }
    }
    eprintln!("placement: gave up; the dump records what the compositor actually did");
}

/// Read-only placement probe (used with --wm-fullscreen off).
fn read_placement(requested: String, st: Arc<Mutex<State>>, _fs: bool) {
    std::thread::sleep(Duration::from_millis(700));
    if !hypr_env_ok() {
        return;
    }
    let mons = monitors();
    if let Some((mid, x, y, w, h, fs)) = client_geometry() {
        let mname = mons.iter().find(|m| m.id == mid).map(|m| m.name.clone()).unwrap_or_default();
        let mut s = st.lock().unwrap_or_else(|p| p.into_inner());
        s.placement.requested = requested.clone();
        s.placement.monitor = mname;
        s.placement.monitor_id = mid;
        s.placement.x = x;
        s.placement.y = y;
        s.placement.w = w;
        s.placement.h = h;
        s.placement.fullscreen = fs;
        s.placement.verified = false;
        s.placement.note = "wm-fullscreen off: placement read back, not requested".into();
    }
}

fn wm_fullscreen_toggle() {
    if !hypr_env_ok() {
        eprintln!("wm-fullscreen: no HYPRLAND_INSTANCE_SIGNATURE, leaving the window alone");
        return;
    }
    let call = format!("hl.dsp.window.fullscreen_state({{internal=2, client=2, window=\"title:{TITLE}\"}})");
    for _ in 1..=6 {
        dispatch(&call);
        std::thread::sleep(Duration::from_millis(250));
        if let Some((_, _, _, _, _, fs)) = client_geometry() {
            if fs == 2 {
                eprintln!("wm-fullscreen: window fullscreen=2");
                return;
            }
        }
    }
    eprintln!("wm-fullscreen: gave up; window stays as the compositor left it");
}

/// Read the collector's recorded `meta` line back from its own SQLite store.
/// Fallback only: the collector now replays its stored meta to every subscriber
/// at connect, so this is used only when --meta-db is given and the socket never
/// delivered one. Read-only, one row.
fn meta_backfill(db: String, st: Arc<Mutex<State>>) {
    for attempt in 1..=6 {
        std::thread::sleep(Duration::from_millis(500));
        let have = {
            let s = st.lock().unwrap_or_else(|p| p.into_inner());
            s.model.is_some()
        };
        if have {
            return;
        }
        if !std::path::Path::new(&db).exists() {
            continue;
        }
        let out = std::process::Command::new("sqlite3")
            .args(["-json", &db, "SELECT line FROM events WHERE kind='meta' ORDER BY seq DESC LIMIT 1"])
            .output();
        let Ok(out) = out else {
            eprintln!("meta-db: sqlite3 unavailable");
            return;
        };
        if !out.status.success() {
            eprintln!("meta-db: query failed: {}", String::from_utf8_lossy(&out.stderr).trim());
            continue;
        }
        let list = match json::parse(&out.stdout) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("meta-db: parse {e}");
                continue;
            }
        };
        let line = match list {
            json::Value::Arr(a) => a
                .first()
                .and_then(|r| r.get("line"))
                .and_then(|l| l.as_str())
                .map(|s| s.to_string()),
            _ => None,
        };
        if let Some(l) = line {
            let mut s = st.lock().unwrap_or_else(|p| p.into_inner());
            if s.backfill_meta(&l, "collector-store") {
                eprintln!("meta-db: model {:?} from the collector's recorded meta row", s.model);
                return;
            }
        } else {
            eprintln!("meta-db: no meta row yet (attempt {attempt})");
        }
    }
}

/// INPUT SELF-TEST: apply one control-file line to the UI state. This is the
/// same set of operations the keyboard handlers perform (zoom, pan, view,
/// trails, layer, rain, banner) and nothing else — it cannot inject telemetry,
/// create activations, or move data. The app labels every dump with
/// `input_selftest` so no reader can mistake it for real input.
fn apply_command(cmd: &str, st: &mut State, now_ms: f64) -> Result<String, String> {
    let parts: Vec<&str> = cmd.split_whitespace().collect();
    match parts.as_slice() {
        ["zoom", z] => {
            let v: f32 = z.parse().map_err(|_| format!("bad zoom {z:?}"))?;
            st.set_zoom(v);
            Ok(format!("zoom {:.3}", st.zoom))
        }
        ["pan", x, y] => {
            let dx: f32 = x.parse().map_err(|_| format!("bad pan x {x:?}"))?;
            let dy: f32 = y.parse().map_err(|_| format!("bad pan y {y:?}"))?;
            st.pan_by(dx, dy);
            Ok(format!("pan {:+.1},{:+.1}", st.pan_x, st.pan_y))
        }
        ["view", v] => {
            st.view = parse_view(v);
            Ok(format!("view {}", st.view))
        }
        ["rain", s] => {
            st.rain = *s != "off";
            Ok(format!("rain {}", if st.rain { "on" } else { "off" }))
        }
        ["trails", s] => {
            st.trails = *s != "off";
            Ok(format!("trails {}", st.trails))
        }
        ["layer", l] => {
            let v: usize = l.parse().map_err(|_| format!("bad layer {l:?}"))?;
            st.sel_layer = v.min(st.layers.saturating_sub(1));
            st.sel_pinned = true;
            Ok(format!("layer {}", st.sel_layer))
        }
        ["banner"] => {
            st.banner.trigger(now_ms);
            Ok(format!("banner cycle {}", st.banner.cycles))
        }
        ["reset"] => {
            st.reset_view();
            Ok("reset view".into())
        }
        ["motion", s] => {
            st.motion_reduced = *s == "reduced";
            if st.motion_reduced {
                st.rain = false;
            }
            Ok(format!("motion {}", if st.motion_reduced { "reduced" } else { "full" }))
        }
        [] => Err("empty command".into()),
        _ => Err(format!("unknown command {cmd:?}")),
    }
}

/// Watch the labelled control file and inject its commands as user events.
fn ctl_watch(path: String, st: Arc<Mutex<State>>, proxy: EventLoopProxy<()>) {
    use std::io::{Read, Seek, SeekFrom};
    let mut off: u64 = 0;
    loop {
        if STOP.load(Ordering::SeqCst) {
            return;
        }
        std::thread::sleep(Duration::from_millis(60));
        let len = match std::fs::metadata(&path) {
            Ok(m) => m.len(),
            Err(_) => continue,
        };
        if len < off {
            off = 0;
        }
        if len == off {
            continue;
        }
        let mut f = match std::fs::File::open(&path) {
            Ok(f) => f,
            Err(_) => continue,
        };
        if f.seek(SeekFrom::Start(off)).is_err() {
            continue;
        }
        let mut buf = String::new();
        if f.read_to_string(&mut buf).is_err() {
            continue;
        }
        off = len;
        for line in buf.lines() {
            let cmd = line.trim();
            if cmd.is_empty() || cmd.starts_with('#') {
                continue;
            }
            let res = {
                let mut s = st.lock().unwrap_or_else(|p| p.into_inner());
                let now_ms = (crate::now_unix() - s.started_wall) * 1000.0;
                let r = apply_command(cmd, &mut s, now_ms);
                s.selftest_commands += 1;
                s.selftest_last = cmd.to_string();
                s.selftest_last_ts = crate::now_unix();
                if r.is_err() {
                    s.selftest_errors += 1;
                }
                r
            };
            match res {
                Ok(m) => eprintln!("ctl: INPUT SELF-TEST {cmd:?} -> {m}"),
                Err(e) => eprintln!("ctl: INPUT SELF-TEST {cmd:?} rejected: {e}"),
            }
            let _ = proxy.send_event(());
        }
    }
}

fn main() {
    let args = parse_args();
    unsafe {
        signal(2, handle_signal);   // SIGINT
        signal(15, handle_signal);  // SIGTERM
    }
    let st = Arc::new(Mutex::new(State::new(std::process::id(), &args.socket, args.rain)));

    // ---- counter/panel dump thread: never touches the GPU or the window, so
    // the window can stay in ControlFlow::Wait while this keeps the dump current.
    if let Some(path) = args.stats.clone() {
        let st2 = st.clone();
        let ms = args.stats_ms;
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(Duration::from_millis(ms));
                let dump = {
                    let s = st2.lock().unwrap_or_else(|p| p.into_inner());
                    s.dump()
                };
                if path == "-" {
                    println!("{dump}");
                    use std::io::Write;
                    let _ = std::io::stdout().flush();
                } else {
                    let tmp = format!("{path}.tmp");
                    if std::fs::write(&tmp, dump.as_bytes()).is_ok() {
                        let _ = std::fs::rename(&tmp, &path);
                    }
                }
                if STOP.load(Ordering::SeqCst) {
                    return;
                }
            }
        });
    }

    if let Some(db) = args.meta_db.clone() {
        meta_backfill(db, st.clone());
    }

    let event_loop = match EventLoop::<()>::with_user_event().build() {
        Ok(l) => l,
        Err(e) => {
            eprintln!("event loop: {e}");
            std::process::exit(1);
        }
    };
    event_loop.set_control_flow(ControlFlow::Wait);
    let mut app = App {
        rx: None,
        window: None,
        gpu: None,
        st: st.clone(),
        overlay: Overlay::new(),
        rain: Rain::new(1600.0, 900.0),
        t0: Instant::now(),
        last_draw_ms: 0.0,
        redraws: 0,
        frames: 0,
        spawned: false,
        next_frame: Instant::now(),
        last_fp: u64::MAX,
        resize_pending: true,
        anim_until: Instant::now(),
        msgs_since_draw: 0,
        req_anim: false,
        req_change: false,
        last_groups: Vec::new(),
        banner_anim: false,
        args,
    };
    let proxy = event_loop.create_proxy();
    app.spawn_client(proxy.clone());
    if let Some(path) = app.args.ctl.clone() {
        if std::fs::File::open(&path).is_err() {
            let _ = std::fs::write(&path, "");
        }
        {
            let mut s = app.st.lock().unwrap_or_else(|p| p.into_inner());
            s.selftest_enabled = true;
            s.selftest_source = path.clone();
        }
        let st2 = app.st.clone();
        let p = proxy.clone();
        eprintln!("ctrl: INPUT SELF-TEST hook armed on {path}");
        std::thread::spawn(move || ctl_watch(path, st2, p));
    }
    // The event loop blocks in ControlFlow::Wait when nothing is animating, so a
    // signal alone cannot wake it. A 150 ms watcher turns STOP into one user
    // event, which lets about_to_wait() exit cleanly — no periodic redraws.
    {
        let p = proxy.clone();
        std::thread::spawn(move || {
            loop {
                if STOP.load(Ordering::SeqCst) {
                    eprintln!("signal: STOP observed, waking the event loop");
                    match p.send_event(()) {
                        Ok(_) => eprintln!("signal: wake event sent"),
                        Err(e) => eprintln!("signal: wake failed: {e:?}"),
                    }
                    return;
                }
                std::thread::sleep(Duration::from_millis(150));
            }
        });
    }
    eprintln!("matrix-app: socket {} · stats {:?} · rain {} · monitor {} · view {}",
        app.args.socket, app.args.stats, app.args.rain, app.args.monitor, app.args.view);
    event_loop.run_app(&mut app).expect("run_app");

    // final dump so a stopped app still leaves its counters behind
    if let Some(path) = app.args.stats.clone() {
        let dump = { app.st.lock().unwrap_or_else(|p| p.into_inner()).dump() };
        if path == "-" {
            println!("{dump}");
        } else {
            let tmp = format!("{path}.tmp");
            if std::fs::write(&tmp, dump.as_bytes()).is_ok() {
                let _ = std::fs::rename(&tmp, &path);
            }
        }
    }
    eprintln!("matrix-app: exit · frames {} · redraws {}", app.frames, app.redraws);
}
