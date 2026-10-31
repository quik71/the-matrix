// THE MATRIX Stage 4 — native application.
//
//   collector unix socket (v1 NDJSON)  ->  in-app state  ->  winit + wgpu window
//
// No browser, no webview, no HTTP client: the app reads the collector's unix
// socket, keeps its own state from the events it actually received, and draws
// that state with the same instanced-quad pipeline the measured bench-A harness
// used. Anything the event stream does not carry is rendered as unavailable.

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
use render::{Overlay, Rain, Renderer};
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
            "--wm-fullscreen" => {
                a.wm_fullscreen = need(i) != "off";
                i += 2;
            }
            "--help" | "-h" => {
                println!(
                    "matrix-app — THE MATRIX native viewer\n\
                     --socket <path>        collector unix socket (default /home/main/the-matrix/run/collector.sock)\n\
                     --font <ttf/ttc>       font for the glyph atlas\n\
                     --stats <path|->       write the counter dump as JSON every --stats-ms (or print to stdout)\n\
                     --stats-ms <n>         dump interval in ms (1000)\n\
                     --rain <on|off>        Matrix rain animation (on)\n\
                     --width/--height <n>   initial window size (1600x900); the WM makes it fullscreen\n\
                     --wm-fullscreen <on|off> ask Hyprland for WM-fullscreen on the monitor (on)\n\
                     --meta-db <sqlite>     read the collector's recorded meta event back when the\n\
                                           socket stream never carried one (late attach); labelled\n\
                                           as 'collector store' in the UI and in the dump\n\
                     keys: esc quit · r toggle rain · f toggle WM-fullscreen"
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
                Ok(Msg::ConnectError { err, .. }) => {
                    st.connect_errors += 1;
                    if st.connected {
                        st.connected = false;
                        st.disconnects += 1;
                    }
                    st.last_disconnect_reason = format!("connect: {err}");
                }
                Err(_) => break,
            }
            if got > 200_000 {
                break;
            }
        }
        drop(st);
        self.overlay.tick(self.elapsed_ms());
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
        { let mut st = self.st.lock().unwrap_or_else(|p| p.into_inner()); st.rain = self.args.rain; }
        self.rain = Rain::new(size.width as f32, size.height as f32);
        self.t0 = Instant::now();
        if self.args.wm_fullscreen {
            std::thread::spawn(wm_fullscreen);
        }
        window.request_redraw();
    }

    fn user_event(&mut self, _el: &ActiveEventLoop, _ev: ()) {
        self.drain();
        if let Some(w) = self.window.as_ref() {
            w.request_redraw();
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
                if let Some(w) = self.window.as_ref() {
                    w.request_redraw();
                }
            }
            WindowEvent::KeyboardInput { event, .. } => {
                if event.state == ElementState::Pressed {
                    match event.logical_key {
                        Key::Named(NamedKey::Escape) => el.exit(),
                        Key::Character(ref c) if c.eq_ignore_ascii_case("r") => {
                            let mut st = self.st.lock().unwrap_or_else(|p| p.into_inner());
                            st.rain = !st.rain;
                            drop(st);
                            if let Some(w) = self.window.as_ref() {
                                w.request_redraw();
                            }
                        }
                        _ => {}
                    }
                }
            }
            WindowEvent::RedrawRequested => {
                self.drain();
                let now = self.elapsed_ms();
                let dt = (now - self.last_draw_ms) as f32;
                self.last_draw_ms = now;
                if let (Some(g), Some(_w)) = (self.gpu.as_mut(), self.window.as_ref()) {
                    let mut st = self.st.lock().unwrap_or_else(|p| p.into_inner());
                    let fs = g.draw(&st, &self.overlay, &mut self.rain, now, dt);
                    self.frames += 1;
                    self.redraws += 1;
                    st.frames = self.frames;
                    st.redraw_requests = self.redraws;
                    st.last_frame = fs;
                }
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        if STOP.load(Ordering::SeqCst) {
            el.exit();
            return;
        }
        let now = self.elapsed_ms();
        let rain = self.st.lock().unwrap_or_else(|p| p.into_inner()).rain;
        // Present-on-demand with a 60 fps budget: animate only while the rain is
        // on or a lamp is still decaying, and never faster than one frame per
        // 16 ms. With no animation and no events the loop blocks in Wait with no
        // timer at all — zero redraws, zero wake-ups.
        if rain || self.overlay.animating(now) {
            let now_i = Instant::now();
            if now_i >= self.next_frame {
                self.next_frame = now_i + Duration::from_millis(16);
                if let Some(w) = self.window.as_ref() {
                    w.request_redraw();
                }
            }
            el.set_control_flow(ControlFlow::WaitUntil(self.next_frame));
        } else {
            el.set_control_flow(ControlFlow::Wait);
        }
    }
}

/* ------------------------------------------------------------------ drive */

fn wm_fullscreen() {
    // WM fullscreen on the monitor the window is on: the compositor keeps the
    // surface at the monitor's own resolution (1920x1080 here). Deliberately NOT
    // winit Borderless(4K): that path was measured to stall the swapchain at
    // 1 fps. The window is addressed by title, so this never fullscreens
    // whichever window happens to be focused.
    std::thread::sleep(Duration::from_millis(250));
    if std::env::var("HYPRLAND_INSTANCE_SIGNATURE").unwrap_or_default().is_empty() {
        eprintln!("wm-fullscreen: no HYPRLAND_INSTANCE_SIGNATURE, leaving the window tiled");
        return;
    }
    // Hyprland >= 0.56 takes Lua dispatcher calls here. The window has to be
    // mapped before the dispatch can match it, so retry until the compositor
    // reports this window at fullscreen=2 (checking, never blind-toggling).
    let call = format!(
        "hl.dsp.window.fullscreen_state({{internal=2, client=2, window=\"title:{TITLE}\"}})"
    );
    for attempt in 1..=10 {
        match client_geometry() {
            Some((size, 2)) => {
                eprintln!("wm-fullscreen: window {size} fullscreen=2");
                return;
            }
            Some((size, fs)) => eprintln!("wm-fullscreen: attempt {attempt}: window {size} fullscreen={fs}"),
            None => eprintln!("wm-fullscreen: attempt {attempt}: window not mapped yet"),
        }
        match std::process::Command::new("hyprctl").args(["dispatch", &call]).output() {
            Ok(o) => {
                let out = String::from_utf8_lossy(&o.stdout).trim().to_string();
                if out != "ok" {
                    eprintln!("wm-fullscreen: hyprctl dispatch -> {out}");
                }
            }
            Err(e) => {
                eprintln!("wm-fullscreen: hyprctl unavailable ({e}), leaving the window tiled");
                return;
            }
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    eprintln!("wm-fullscreen: gave up; window stays as the compositor left it");
}

/// Ask Hyprland for this window's size and fullscreen state. Read-only: the app
/// never writes window rules or touches other windows.
fn client_geometry() -> Option<(String, i64)> {
    let out = std::process::Command::new("hyprctl").args(["-j", "clients"]).output().ok()?;
    let v = json::parse(&out.stdout).ok()?;
    let list = match v {
        json::Value::Arr(a) => a,
        _ => return None,
    };
    for c in list {
        if c.get("title").and_then(|t| t.as_str()) == Some(TITLE) {
            let w = c.get("size").and_then(|s| match s {
                json::Value::Arr(a) => a.first().and_then(|z| z.as_i64()),
                _ => None,
            });
            let h = c.get("size").and_then(|s| match s {
                json::Value::Arr(a) => a.get(1).and_then(|z| z.as_i64()),
                _ => None,
            });
            let fs = c.get("fullscreen").and_then(|z| z.as_i64()).unwrap_or(-1);
            return Some((format!("{}x{}", w.unwrap_or(0), h.unwrap_or(0)), fs));
        }
    }
    None
}

/// Read the collector's recorded `meta` line back from its own SQLite store.
/// Read-only, one row, only when --meta-db is given and the socket never
/// delivered a meta event. See State::backfill_meta for why.
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

fn main() {
    let args = parse_args();
    unsafe {
        signal(2, handle_signal);   // SIGINT
        signal(15, handle_signal);  // SIGTERM
    }
    let st = Arc::new(Mutex::new(State::new(std::process::id(), &args.socket, args.rain)));

    // ---- counter dump thread: never touches the GPU or the window, so the
    // window can stay in ControlFlow::Wait while this keeps the dump current.
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
        args,
    };
    let proxy = event_loop.create_proxy();
    app.spawn_client(proxy);
    eprintln!("matrix-app: socket {} · stats {:?} · rain {}",
        app.args.socket, app.args.stats, app.args.rain);
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
