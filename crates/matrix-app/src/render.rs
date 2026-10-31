// THE MATRIX Stage 5 — native renderer.
//
// Lifted from the measured bench-A harness (/srv/storage/work/the-matrix-bench/
// harness-a-wgpu): same wgpu 30 / winit 0.30 pipeline, same instanced-quad
// geometry, same 48x512 lattice, same Matrix rain, same activation envelope.
// What changed is the data source (events that came off the collector's unix
// socket) and the scope: Stage 5 draws the monitoring workstation — lattice,
// ACTIVATION, SESSIONS, ENGINE PERF, STREAM HEALTH and HARDWARE — from the v1
// event stream only. Nothing here is simulated: when the stream does not carry
// a field, the panel says unavailable.
//
// Rendering is present-on-demand: the event loop repaints only when a displayed
// value changes, when a lamp is still decaying, or while the rain runs.

use std::collections::HashMap;
use std::sync::Arc;

use bytemuck::{Pod, Zeroable};
use winit::dpi::PhysicalSize;
use winit::window::Window;

use crate::state::{State, EXPERT_SLOTS, LAYER_SLOTS};
use crate::text::{self, Face};

/* ------------------------------------------------------------------ palette */

// Stage 6 palette, §8.1 + the legacy theme (moe-explorer matrix theme):
//   near-black background      #010402
//   deep forest-green surfaces #030f08 (panels), #123a22 (edges)
//   neon green primary accent  #4ade80
//   emerald secondary accent   #10b981 (trails, secondary bars)
//   bright white-green         #d6ffe2 (activation highlights, banner)
// Every value is authored in sRGB, like the CSS the legacy page used; the shader
// converts to linear so the swapchain does not wash the greens out.
const BG: [f32; 4] = [0.004, 0.016, 0.008, 1.0];
const GREEN: [f32; 4] = [0.290, 0.871, 0.502, 1.0];
const BRIGHT: [f32; 4] = [0.841, 1.0, 0.886, 1.0];
const DIM: [f32; 4] = [0.420, 0.800, 0.530, 1.0];
const FAINT: [f32; 4] = [0.220, 0.470, 0.290, 1.0];
const EMERALD: [f32; 4] = [0.063, 0.725, 0.506, 1.0];
const RED: [f32; 4] = [1.0, 0.36, 0.34, 1.0];
const AMBER: [f32; 4] = [1.0, 0.74, 0.26, 1.0];
const PANEL_BG: [f32; 4] = [0.012, 0.059, 0.031, 1.0];
const PANEL_EDGE: [f32; 4] = [0.071, 0.227, 0.133, 1.0];
const CELL_OFF: [f32; 4] = [0.090, 0.150, 0.120, 1.0];
/// Residency ESTIMATE tint (VRAM-prefix cells). Deliberately a quiet shift of
/// the inactive colour: it marks structure, never activation, and the panel
/// legend and the dump both say `estimate`.
const CELL_EST_VRAM: [f32; 4] = [0.150, 0.330, 0.245, 1.0];
/// Panel background opacity (the legacy `bgOpacity` knob): the panels are opaque
/// so the data stays legible; the ambient rain shows in the gutters instead.
const PANEL_ALPHA: f32 = 1.0;

/* --------------------------------------------------------------- theme knobs */

/// Rain wash over the lattice footprint — the legacy `washAlpha` (0.40), with
/// the same hard cap on the final alpha the web theme used (0.14).
pub const WASH_ALPHA: f32 = 0.40;
/// Legacy `bgOpacity` (0.55): the opacity of the background rain pass.
pub const BG_OPACITY: f32 = 0.55;
/// Legacy `glow` (0.6): bloom ring alpha multiplier.
const GLOW: f32 = 0.60;

/// The theme as the app actually renders it: palette plus every animation knob.
/// The dump carries this verbatim so the acceptance packet can quote the values
/// that were on screen instead of a description of them.
pub fn theme_json() -> String {
    // Concatenated, not one format!: every brace below is JSON structure, and a
    // format! string would need each of them doubled. Values are formatted from
    // the very constants the renderer uses, so the dump cannot drift from what
    // is on screen.
    let mut o = String::with_capacity(2048);
    o.push_str("\"theme\":{\"name\":\"matrix-observatory\",\"palette\":{\"bg\":\"#010402\",\"panel\":\"#030f08\",\"panel_edge\":\"#123a22\",\"accent\":\"#4ade80\",\"emerald\":\"#10b981\",\"highlight\":\"#d6ffe2\",\"cell_off\":\"#171f1d\",\"alarm\":\"#ff5c57\",\"warn\":\"#ffbd42\"},");
    o.push_str("\"typography\":{\"family\":\"Noto Sans CJK (technical/monospaced)\",\"sizes_px\":[");
    o.push_str(&format!("{},{},{},{}", SZ_MICRO, SZ_SMALL, SZ_BODY, SZ_TITLE));
    o.push_str("],\"tracking\":\"uppercase panel titles, thin 1px borders\"},");
    o.push_str("\"rain\":{\"font_px\":");
    o.push_str(&format!("{:.0}", RAIN_FONT_PX));
    o.push_str(",\"char_w\":");
    o.push_str(&format!("{:.0}", RAIN_CHAR_W));
    o.push_str(&format!(",\"max_streams\":{},\"density\":{:.2},\"speed_mult\":1.0,\"stream_chars\":{}",
        MAX_STREAMS, DENSITY, STREAM_CHARS));
    o.push_str(",\"mut_ms_min\":30,\"mut_ms_max\":110,\"mut_per_tick\":\"1..3 + the leading glyph\",\"wash_alpha\":");
    o.push_str(&format!("{:.2}", WASH_ALPHA));
    o.push_str(&format!(",\"wash_tail\":{},\"wash_cap\":0.14,\"bg_opacity\":{:.2},\"idle_factor\":{:.2}",
        WASH_TAIL, BG_OPACITY, IDLE_FACTOR));
    o.push_str(",\"glyph_families\":[\"katakana\",\"numbers\",\"hex\",\"latin\",\"math\",\"binary\"],\"forbidden_glyphs\":[\"L\",\"l\",\"#\"]},");
    o.push_str(&format!("\"activation\":{{\"flash_in_ms\":{:.0},\"hold_ms\":{:.0},\"fade_ms\":{:.0}",
        FLASH_IN, HOLD, FADE));
    o.push_str(",\"fade_ease\":\"ease-out (1-t)^2\",");
    o.push_str(&format!("\"trail_ms\":{:.0},\"bloom_rings\":3,\"border_ms\":\"alpha>0.5\",\"burst_radius_px\":{:.0},\"pulse_ms\":{:.0}}},",
        HISTORY_MS, BURST_RADIUS, PULSE_MS));
    o.push_str("\"banner\":{\"text\":\"FOLLOW THE WHITE RABBIT\",");
    o.push_str(&format!("\"reveal_ms\":{:.0},\"hold_ms\":{:.0},\"dissolve_ms\":{:.0}",
        crate::state::BANNER_REVEAL_MS, crate::state::BANNER_HOLD_MS, crate::state::BANNER_DISSOLVE_MS));
    o.push_str(",\"glitch_period_ms\":700,\"glitch_len_ms\":90,\"decorative\":true,");
    o.push_str("\"note\":\"decorative: it obeys the ambient animation gate and never depends on inference activity\"}}");
    o
}

/* ------------------------------------------------------------------ constants */

pub const N_LAYERS: usize = LAYER_SLOTS;
pub const N_EXPERTS: usize = EXPERT_SLOTS;

pub const VIEW_OVERVIEW: u8 = 0;
pub const VIEW_ACTIVATION: u8 = 1;
pub const VIEW_SESSIONS: u8 = 2;
pub const VIEW_PERF: u8 = 3;
pub const VIEW_STREAM: u8 = 4;
pub const VIEW_COACT: u8 = 5;
pub const VIEW_NAMES: [&str; 6] = ["OVERVIEW", "ACTIVATION", "SESSIONS", "PERF", "STREAM", "CO-ACTIVATION"];

const SZ_MICRO: u32 = 11;
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
const GREEN_MAX: f32 = 0.72;
const BLOOM_MIN: f32 = 0.5;
const BLOOM_CAP: usize = 2200;
const TRAIL_PAINT_LIMIT: usize = 9000;
/// Cap on the 4-quad activation border: 600 lit cells is already a wall of
/// light, and the cap keeps a full-lattice flash from quadrupling the quad count.
const BORDER_CAP: usize = 600;

const FLASH_IN: f64 = 80.0;
const HOLD: f64 = 260.0;
const FADE: f64 = 1200.0;
const FLOOR: f64 = 0.32;
const HISTORY_MS: f64 = 4200.0;
/// Activation-linked effects (§8.3): a rain burst intensifies inside this radius
/// of an activated cell, and a pulse ring travels out. Both are spawned by a
/// real expert_selected record and fade on their own clock.
const BURST_RADIUS: f32 = 96.0;
const PULSE_MS: f64 = 720.0;
const MAX_PULSES: usize = 96;
const MAX_BURSTS: usize = 32;
/// Trail bar: the thin emerald bar drawn in the lower part of a cell that has
/// real history (distinct in colour and position from the live lamp fill).
const TRAIL_BAR_H: f32 = 0.34;
const TRAIL_MIN_CELL: f32 = 1.0;
/// Quad budget for the trail pass (each bar is one instanced quad).
const TRAIL_QUAD_CAP: usize = 3000;
/// Layer-row activity decays with this time constant (ms) — a real measurement
/// of recent routing activity per layer, not a decoration.
const HEAT_TAU: f32 = 1400.0;

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

/// One activation-linked effect. Both are spawned by a real expert_selected
/// record and expire on their own clock — nothing here runs per frame and
/// nothing is spawned without an event.
#[derive(Clone, Copy)]
pub struct Pulse {
    pub layer: u16,
    pub expert: u16,
    pub t0: f64,
}

#[derive(Clone, Copy)]
pub struct Burst {
    pub layer: u16,
    pub expert: u16,
    pub t0: f64,
}

pub struct Overlay {
    cur: HashMap<u32, Marker>,
    hist: HashMap<u32, Marker>,
    pass: u32,
    /// Per-layer recent-activity heat (decays), the lattice's row highlight.
    pub heat: Vec<f32>,
    pub activated: u64,
    pub expired: u64,
    pub pulses: Vec<Pulse>,
    pub bursts: Vec<Burst>,
}

impl Overlay {
    pub fn new() -> Overlay {
        Overlay {
            cur: HashMap::with_capacity(4096),
            hist: HashMap::with_capacity(8192),
            pass: u32::MAX,
            heat: vec![0.0; N_LAYERS],
            activated: 0,
            expired: 0,
            pulses: Vec::with_capacity(MAX_PULSES),
            bursts: Vec::with_capacity(MAX_BURSTS),
        }
    }

    /// Spawn the §8.3 pair for one real activation: a local rain burst and a
    /// pulse that travels outward from the cell. Bounded: the oldest effect is
    /// dropped when the cap is reached.
    fn spawn_effect(&mut self, layer: u16, expert: u16, now: f64) {
        if self.pulses.len() >= MAX_PULSES {
            self.pulses.remove(0);
        }
        self.pulses.push(Pulse { layer, expert, t0: now });
        if self.bursts.len() >= MAX_BURSTS {
            self.bursts.remove(0);
        }
        self.bursts.push(Burst { layer, expert, t0: now });
    }

    /// 0..1 lifetime of the pulse ring, or None once it has expired.
    pub fn pulse_progress(&self, p: &Pulse, now: f64) -> Option<f32> {
        let age = now - p.t0;
        if age < 0.0 || age >= PULSE_MS {
            return None;
        }
        Some((age / PULSE_MS) as f32)
    }

    /// 0..1 lifetime of a burst, or None once it has expired.
    pub fn burst_progress(&self, b: &Burst, now: f64) -> Option<f32> {
        let age = now - b.t0;
        if age < 0.0 || age >= PULSE_MS * 1.6 {
            return None;
        }
        Some((age / (PULSE_MS * 1.6)) as f32)
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
            // ease-out: the lamp leaves the hold quickly, then settles, which
            // reads as a fade rather than a linear ramp down
            let t = (age - h1) / FADE;
            let e = 1.0 - (1.0 - t) * (1.0 - t);
            return (1.0 - (1.0 - FLOOR) * e) as f32;
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

    /// A real selection: light the lamp AND spawn the activation-linked effects.
    pub fn activate(&mut self, layer: u16, expert: u16, now: f64) {
        self.activate_lamp(layer, expert, now);
        self.spawn_effect(layer, expert, now);
    }

    fn activate_lamp(&mut self, layer: u16, expert: u16, now: f64) {
        let idx = layer as u32 * N_EXPERTS as u32 + expert as u32;
        if let Some(m) = self.cur.get(&idx).copied() {
            let a = self.alpha(&m, now);
            if let Some(mm) = self.cur.get_mut(&idx) {
                mm.t0 = now - FLASH_IN * a as f64;
            }
            self.activated += 1;
            self.bump_heat(layer);
            return;
        }
        if let Some(mut m) = self.hist.remove(&idx) {
            let a = self.alpha(&m, now);
            m.t0 = now - FLASH_IN * a as f64;
            self.cur.insert(idx, m);
            self.activated += 1;
            self.bump_heat(layer);
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
        self.bump_heat(layer);
    }

    fn bump_heat(&mut self, layer: u16) {
        if let Some(h) = self.heat.get_mut(layer as usize) {
            *h = (*h + 1.0).min(12.0);
        }
    }

    /// Time-based decay of the layer-row heat; called once per drawn frame.
    pub fn decay(&mut self, dt_ms: f32) {
        let k = (-dt_ms.clamp(0.0, 1000.0) / HEAT_TAU).exp();
        for h in self.heat.iter_mut() {
            *h *= k;
            if *h < 0.01 {
                *h = 0.0;
            }
        }
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
        self.pulses.retain(|p| now - p.t0 < PULSE_MS);
        self.bursts.retain(|b| now - b.t0 < PULSE_MS * 1.6);
    }

    pub fn live(&self) -> usize {
        self.cur.len() + self.hist.len()
    }

    /// True while any painted marker still has visible alpha at `now`.
    pub fn animating(&self, now: f64) -> bool {
        self.cur.values().any(|m| self.alpha(m, now) > 0.004)
            || self.hist.values().any(|m| self.alpha(m, now) > 0.004)
            || self.pulses.iter().any(|p| self.pulse_progress(p, now).is_some())
            || self.bursts.iter().any(|b| self.burst_progress(b, now).is_some())
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
    /// Which glyph family this stream is currently drawing from. Re-rolled on
    /// every mutation tick, so a stream visibly cycles through the spec's sets
    /// (katakana → numbers → hex → latin → math → binary) as it falls.
    family: u8,
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
    /// Indices into `glyphs` for each family, in the spec's order.
    family_codes: Vec<Vec<u16>>,
    /// Characters re-rolled since start: proof the animation is not a fixed
    /// string sliding down (spec §8.2 "characters must visibly change").
    pub mutations: u64,
    /// Live knobs (legacy density / speed sliders).
    pub density: f32,
    pub speed_mult: f32,
}

impl Rain {
    pub fn new(w: f32, h: f32) -> Rain {
        let (glyphs, family_codes) = build_families();
        Rain {
            streams: Vec::new(),
            rng: Rng(0x5eed1234),
            w,
            h,
            glyphs,
            family_codes,
            mutations: 0,
            density: DENSITY,
            speed_mult: 1.0,
        }
    }

    fn target(&self, idle: bool) -> usize {
        let f = if idle { IDLE_FACTOR } else { 1.0 };
        ((MAX_STREAMS as f32) * self.density * f) as usize
    }

    fn new_stream(&mut self) -> Stream {
        let cols = ((self.w / RAIN_CHAR_W) as usize).max(1);
        let x = (self.rng.int(cols) as f32) * RAIN_CHAR_W + RAIN_CHAR_W * 0.5;
        let len = 8 + self.rng.int(STREAM_CHARS.saturating_sub(8).max(1));
        let spd = RAIN_FONT_PX * (5.0 + self.rng.next() * 22.0) * RAIN_SPEED * self.speed_mult;
        let y = -self.rng.next() * self.h * 0.6;
        let family = self.rng.int(self.family_codes.len().max(1)) as u8;
        let set = &self.family_codes[family as usize % self.family_codes.len().max(1)];
        let mut codes = [0u16; STREAM_CHARS];
        for c in codes.iter_mut() {
            *c = set[self.rng.int(set.len().max(1))];
        }
        let mut_ms = 30.0 + self.rng.next() * 80.0;
        let next = self.rng.next() * mut_ms;
        Stream { x, y, spd, len, codes, next, mut_ms, family }
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
        let n_fam = self.family_codes.len().max(1);
        for i in 0..self.streams.len() {
            let (mut y, spd, mut next, mut_ms, len) = {
                let s = &self.streams[i];
                (s.y, s.spd, s.next, s.mut_ms, s.len)
            };
            y += spd * dt / 1000.0;
            next -= dt;
            if next <= 0.0 {
                next = mut_ms * (0.7 + self.rng.next() * 0.6);
                // cycle the family too: the set itself changes with the glyphs
                let fam = self.rng.int(n_fam) as u8;
                self.streams[i].family = fam;
                let set_len = self.family_codes[fam as usize % n_fam].len().max(1);
                let n = 1 + self.rng.int(3);
                // the leading glyph always changes; up to 2 more anywhere
                let lead = self.family_codes[fam as usize % n_fam][self.rng.int(set_len)];
                self.streams[i].codes[0] = lead;
                self.mutations += 1;
                for _k in 1..n {
                    let slot = self.rng.int(len);
                    let c = self.family_codes[fam as usize % n_fam][self.rng.int(set_len)];
                    self.streams[i].codes[slot] = c;
                    self.mutations += 1;
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

    /// How many distinct glyph families are on screen right now.
    pub fn families_live(&self) -> usize {
        let mut seen = [false; 8];
        for s in self.streams.iter() {
            seen[(s.family as usize) % 8] = true;
        }
        seen.iter().filter(|x| **x).count()
    }
}

/// The spec's glyph sets (§8.2), each as its own family. 'L', 'l' and '#' are
/// excluded on purpose: the rain can then never spell a real routing label
/// (L07E123 / #hex·t42), the same convention the legacy theme documents.
fn build_families() -> (Vec<char>, Vec<Vec<u16>>) {
    let mut glyphs: Vec<char> = Vec::new();
    let mut families: Vec<Vec<u16>> = Vec::new();
    let mut names: Vec<&'static str> = Vec::new();
    let add_family = |set: Vec<char>, name: &'static str, glyphs: &mut Vec<char>,
                          families: &mut Vec<Vec<u16>>, names: &mut Vec<&'static str>| {
        let mut idxs = Vec::with_capacity(set.len());
        for ch in set {
            if ch == 'L' || ch == 'l' || ch == '#' {
                continue;
            }
            let pos = match glyphs.iter().position(|g| *g == ch) {
                Some(p) => p,
                None => {
                    glyphs.push(ch);
                    glyphs.len() - 1
                }
            };
            idxs.push(pos as u16);
        }
        if !idxs.is_empty() {
            families.push(idxs);
            names.push(name);
        }
    };
    let katakana: Vec<char> = (0xff66u32..=0xff9d).filter_map(char::from_u32).collect();
    add_family(katakana, "katakana", &mut glyphs, &mut families, &mut names);
    add_family(('0'..='9').collect(), "numbers", &mut glyphs, &mut families, &mut names);
    add_family("ABCDEF".chars().collect(), "hex", &mut glyphs, &mut families, &mut names);
    add_family("GHIJKMNOPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz".chars().collect(),
               "latin", &mut glyphs, &mut families, &mut names);
    add_family("+-*/=<>%&|~^$!§±÷×≠≤≥∞≡⊕⊗∴∵¬∂∇{}[]()/:;·".chars().collect(),
               "math", &mut glyphs, &mut families, &mut names);
    add_family("01".chars().collect(), "binary", &mut glyphs, &mut families, &mut names);
    let _ = names;
    (glyphs, families)
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
    fn empty(&self) -> bool {
        self.w <= 1.0 || self.h <= 1.0
    }
}

#[derive(Clone, Copy)]
pub struct Layout {
    pub w: f32,
    pub h: f32,
    pub view: u8,
    pub header: Rect,
    pub footer: Rect,
    pub sessions: Rect,
    pub activation: Rect,
    pub lattice_panel: Rect,
    pub perf: Rect,
    pub stream: Rect,
    pub hardware: Rect,
    pub full: Rect,
    /// The viewport-transformed lattice rect (what is actually drawn).
    pub grid: Rect,
    pub cell: f32,   // column width
    pub cell_h: f32, // row height (>= cell when the panel is taller than wide)
    /// The un-zoomed, un-panned lattice rect: the reference the viewport
    /// transform is applied to, so zoom is anchored on the base grid's centre.
    pub grid_base: Rect,
    /// Banner strip: inside the lattice panel, directly under its title.
    pub banner: Rect,
    /// Visible cell index ranges (l0, l1, e0, e1) after culling to the panel.
    pub vis: (usize, usize, usize, usize),
}

impl Layout {
    #[allow(clippy::too_many_arguments)]
    pub fn compute(w: f32, h: f32, view: u8, layers: usize, experts: usize,
                   zoom: f32, pan_x: f32, pan_y: f32) -> Layout {
        let pad = 10.0;
        let header_h = 78.0;
        let footer_h = 22.0;
        let top = header_h + 4.0;
        let bottom_line = (h - footer_h - 2.0).max(top + 120.0);
        let header = Rect { x: pad, y: 4.0, w: (w - 2.0 * pad).max(100.0), h: header_h - 6.0 };
        let footer = Rect { x: pad, y: h - footer_h, w: (w - 2.0 * pad).max(100.0), h: footer_h - 4.0 };
        let body = Rect { x: pad, y: top, w: (w - 2.0 * pad).max(100.0), h: (bottom_line - top).max(120.0) };

        let mut l = Layout {
            w, h, view, header, footer,
            sessions: Rect::default(),
            activation: Rect::default(),
            lattice_panel: Rect::default(),
            perf: Rect::default(),
            stream: Rect::default(),
            hardware: Rect::default(),
            full: Rect::default(),
            grid: Rect::default(),
            cell: 0.0,
            cell_h: 0.0,
            grid_base: Rect::default(),
            banner: Rect::default(),
            vis: (0, 0, 0, 0),
        };

        match view {
            VIEW_OVERVIEW => {
                let bottom_h = (body.h * 0.27).clamp(170.0, 260.0).min(body.h * 0.45);
                let main_h = (body.h - bottom_h - 8.0).max(160.0);
                let side = (w * 0.175).clamp(250.0, 360.0).min(body.w * 0.30);
                l.sessions = Rect { x: body.x, y: body.y, w: side, h: main_h };
                l.activation = Rect { x: body.right() - side, y: body.y, w: side, h: main_h };
                l.lattice_panel = Rect {
                    x: l.sessions.right() + 8.0,
                    y: body.y,
                    w: (l.activation.x - l.sessions.right() - 16.0).max(160.0),
                    h: main_h,
                };
                // bottom row: ENGINE PERF | STREAM HEALTH | HARDWARE
                let gap = 8.0;
                let avail = body.w - 2.0 * gap;
                let pw = (avail * 0.30).max(200.0);
                let sw = (avail * 0.42).max(240.0);
                let hw = (avail - pw - sw).max(160.0);
                let by = body.bottom() - bottom_h;
                l.perf = Rect { x: body.x, y: by, w: pw, h: bottom_h };
                l.stream = Rect { x: body.x + pw + gap, y: by, w: sw, h: bottom_h };
                l.hardware = Rect { x: body.x + pw + sw + 2.0 * gap, y: by, w: hw, h: bottom_h };
            }
            VIEW_ACTIVATION => {
                let lw = (body.w * 0.52).max(320.0);
                l.lattice_panel = Rect { x: body.x, y: body.y, w: lw, h: body.h };
                l.activation = Rect { x: body.x + lw + 8.0, y: body.y, w: (body.w - lw - 8.0).max(200.0), h: body.h };
            }
            VIEW_COACT => {
                let lw = (body.w * 0.58).max(360.0);
                l.lattice_panel = Rect { x: body.x, y: body.y, w: lw, h: body.h };
                l.activation = Rect { x: body.x + lw + 8.0, y: body.y,
                                      w: (body.w - lw - 8.0).max(220.0), h: body.h };
            }
            VIEW_SESSIONS => l.full = body,
            VIEW_PERF => l.full = body,
            VIEW_STREAM => {
                let hh = (body.h * 0.34).clamp(150.0, 300.0);
                l.stream = Rect { x: body.x, y: body.y, w: body.w, h: (body.h - hh - 8.0).max(120.0) };
                l.hardware = Rect { x: body.x, y: body.y + l.stream.h + 8.0, w: body.w, h: hh };
            }
            _ => l.full = body,
        }

        // the lattice fits its panel, keeping a layer gutter on the left
        if !l.lattice_panel.empty() {
            let p = l.lattice_panel;
            let gutter = 40.0;
            let avail_w = (p.w - gutter - 18.0).max(64.0);
            let avail_h = (p.h - 58.0).max(32.0);
            let cell = (avail_w / experts.max(1) as f32)
                .min(avail_h / layers.max(1) as f32)
                .max(0.10);
            // Rows may be taller than columns (a 48 x 512 lattice is a wall, not
            // a square): use the panel's height up to 3x the column width so the
            // rows stay readable, and never distort beyond that.
            let cell_h = (avail_h / layers.max(1) as f32).min(cell * 3.0).max(cell);
            let gw = experts.max(1) as f32 * cell;
            let gh = layers.max(1) as f32 * cell_h;
            let base = Rect {
                x: p.x + gutter + ((avail_w - gw) / 2.0).max(0.0),
                y: p.y + 58.0 + ((avail_h - gh) / 2.0).max(0.0),
                w: gw,
                h: gh,
            };
            l.grid_base = base;
            // banner strip: inside the panel, between the title and the lattice
            l.banner = Rect { x: p.x + 12.0, y: p.y + 25.0, w: (p.w - 24.0).max(40.0), h: 27.0 };
            // ---- viewport transform (zoom anchored on the base grid centre)
            let zoom = zoom.clamp(crate::state::MIN_ZOOM, crate::state::MAX_ZOOM);
            let cx = base.x + base.w * 0.5;
            let cy = base.y + base.h * 0.5;
            let gw2 = (base.w * zoom).max(1.0);
            let gh2 = (base.h * zoom).max(1.0);
            l.grid = Rect { x: cx - gw2 * 0.5 + pan_x, y: cy - gh2 * 0.5 + pan_y, w: gw2, h: gh2 };
            l.cell = (cell * zoom).max(0.05);
            l.cell_h = (cell_h * zoom).max(0.05);
            // ---- cull to the panel: only cells that can be seen are built
            let (l0, l1) = Layout::visible_rows(layers, l.grid.y, l.cell_h, p.y + 2.0, p.bottom() - 2.0);
            let (e0, e1) = Layout::visible_cols(experts, l.grid.x, l.cell, p.x + gutter * 0.2, p.right() - 2.0);
            l.vis = (l0, l1, e0, e1);
        }
        l
    }

    /// Rows/columns intersecting a band. Layer rows are drawn bottom-up
    /// (row 0 at the bottom), so they are mapped through the same formula the
    /// drawing code uses.
    fn visible_rows(n: usize, grid_y: f32, cell_h: f32, top: f32, bottom: f32) -> (usize, usize) {
        let mut a = n;
        let mut b = 0usize;
        for i in 0..n {
            let y = grid_y + (n - 1 - i) as f32 * cell_h;
            if y + cell_h >= top && y <= bottom {
                a = a.min(i);
                b = b.max(i + 1);
            }
        }
        if a >= b {
            (0, 0)
        } else {
            (a, b)
        }
    }

    fn visible_cols(n: usize, grid_x: f32, cell: f32, left: f32, right: f32) -> (usize, usize) {
        let mut a = n;
        let mut b = 0usize;
        for i in 0..n {
            let x = grid_x + i as f32 * cell;
            if x + cell >= left && x <= right {
                a = a.min(i);
                b = b.max(i + 1);
            }
        }
        if a >= b {
            (0, 0)
        } else {
            (a, b)
        }
    }

    /// Rectangles that get the panel chrome, in draw order.
    fn chromes(&self) -> Vec<Rect> {
        let mut v = vec![self.header];
        for r in [self.sessions, self.activation, self.lattice_panel, self.perf, self.stream, self.hardware, self.full] {
            if !r.empty() {
                v.push(r);
            }
        }
        v
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

const SIZES: [u32; 4] = [SZ_MICRO, SZ_SMALL, SZ_BODY, SZ_TITLE];

impl TextBuf {
    fn new() -> TextBuf {
        let mut by_size = HashMap::new();
        for px in SIZES {
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
    pub trail_quads: usize,
    pub pulse_quads: usize,
    pub banner_quads: usize,
    pub visible_cells: usize,
    pub families: usize,
    /// cells painted with the residency ESTIMATE tint (0 when no estimate)
    pub res_cells: usize,
    pub res_len: usize,
    pub res_epoch_seen: u64,
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
    rowheat: DynBuf,
    rain: DynBuf,
    bgrain: DynBuf,
    wash: DynBuf,
    chrome: DynBuf,
    trails_buf: DynBuf,
    pulses_buf: DynBuf,
    text_bufs: HashMap<u32, DynBuf>,
    text: HashMap<u32, FaceGpu>,
    store_layout: wgpu::BindGroupLayout,
    pub scale: f32,
    pub layout: Layout,
    pub adapter: String,
    pub format: String,
    pub present_mode: String,
    css: (f32, f32),
    dim: (usize, usize),
    /// (x, y, w, h, cell) the cell/gridline quads were built for: any change
    /// re-uploads them, so the lattice always sits inside its current panel.
    grid_key: (f32, f32, f32, f32, f32),
    n_cells: usize,
    /// residency estimate bitmap, one byte per (layer, expert); empty = none
    res: Vec<u8>,
    res_epoch: u64,
    res_cells: usize,
    n_lattice: usize,
    /// cells built after culling (zoom/pan can hide most of the lattice)
    visible: usize,
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
        let rowheat = DynBuf::new(&device, &bgl_store, 256, "rowheat");
        let rain = DynBuf::new(&device, &bgl_store, 4096, "rain");
        let bgrain = DynBuf::new(&device, &bgl_store, 1024, "bgrain");
        let wash = DynBuf::new(&device, &bgl_store, 1024, "wash");
        let chrome_q = DynBuf::new(&device, &bgl_store, 512, "chrome");
        let trails_buf = DynBuf::new(&device, &bgl_store, 8192, "trails");
        let pulses_buf = DynBuf::new(&device, &bgl_store, 1024, "pulses");

        // ---- glyph faces + per-face text buffers
        let mut text = HashMap::new();
        let mut text_bufs = HashMap::new();
        for px in SIZES {
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

        let css = (w as f32 / scale, h as f32 / scale);
        let layout = Layout::compute(css.0, css.1, VIEW_OVERVIEW, N_LAYERS, N_EXPERTS, 1.0, 0.0, 0.0);
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
            rowheat,
            rain,
            bgrain,
            wash,
            chrome: chrome_q,
            trails_buf,
            pulses_buf,
            text_bufs,
            text,
            store_layout: bgl_store,
            scale,
            layout,
            adapter: format!("{} {:?}", info.backend.to_string(), info.name),
            format: format!("{format:?}"),
            present_mode: "Fifo".into(),
            css,
            dim: (N_LAYERS, N_EXPERTS),
            grid_key: (f32::NAN, 0.0, 0.0, 0.0, 0.0),
            n_cells: 0,
            res: Vec::new(),
            res_epoch: u64::MAX,
            res_cells: 0,
            n_lattice: (N_EXPERTS + 1) + (N_LAYERS + 1),
            visible: 0,
        };
        r.rebuild_lattice();
        Ok(r)
    }

    /// Cells built after culling (zoom/pan can hide most of the lattice).
    pub fn visible_cells(&self) -> usize {
        self.visible
    }

    pub fn face(&self, px: u32) -> &Face {
        &self.text[&px].face
    }

    /// Recompute the layout for the current CSS size, view and grid dimensions.
    pub fn relayout(&mut self, view: u8, layers: usize, experts: usize,
                    zoom: f32, pan_x: f32, pan_y: f32,
                    res: Option<&crate::state::Residency>, res_epoch: u64) {
        self.layout = Layout::compute(self.css.0, self.css.1, view, layers, experts, zoom, pan_x, pan_y);
        let g = self.layout.grid;
        let key = (g.x, g.y, g.w, g.h, self.layout.cell + self.layout.cell_h * 1000.0);
        // The residency estimate arrives once, after the lattice exists: its
        // epoch is part of the rebuild key so the tint appears without any
        // per-frame bitmap copy.
        let epoch = if res.map(|r| r.status == "estimate").unwrap_or(false) {
            res_epoch.max(1)
        } else {
            0
        };
        if self.dim != (layers, experts) || self.grid_key != key || self.res_epoch != epoch {
            self.dim = (layers, experts);
            self.grid_key = key;
            self.res_epoch = epoch;
            self.res = match res {
                Some(r) if r.status == "estimate" && r.cells.len() >= layers.saturating_mul(experts) => {
                    r.cells.iter().map(|b| if *b { 1u8 } else { 0u8 }).collect()
                }
                _ => Vec::new(),
            };
            self.rebuild_lattice();
        }
    }

    pub fn resize(&mut self, size: PhysicalSize<u32>) {
        if size.width == 0 || size.height == 0 {
            return;
        }
        self.config.width = size.width;
        self.config.height = size.height;
        self.surface.configure(&self.device, &self.config);
        self.css = (size.width as f32 / self.scale, size.height as f32 / self.scale);
        let (v, l, e) = (self.layout.view, self.dim.0, self.dim.1);
        self.relayout(v, l, e, 1.0, 0.0, 0.0, None, 0);
        self.queue.write_buffer(&self.globals_buf, 0, bytemuck::bytes_of(&Globals {
            res: [size.width as f32, size.height as f32],
            scale: self.scale,
            _pad: 0.0,
        }));
    }

    /// The residency lattice: one quad per (layer, expert) at the inactive
    /// colour. Expert residency is not part of the v1 event stream, so the app
    /// does not paint residency it cannot see; the lattice geometry is the real
    /// layers x experts from the collector's `meta` event.
    fn rebuild_lattice(&mut self) {
        let v = self.layout;
        let (nl, ne) = self.dim;
        let (l0, l1, e0, e1) = v.vis;
        let mut cell_quads: Vec<Quad> = Vec::with_capacity((l1 - l0).saturating_mul(e1 - e0).max(1));
        let mut painted = 0usize;
        let use_res = !self.res.is_empty() && self.res.len() >= nl.saturating_mul(ne);
        for layer in l0..l1 {
            let y = v.grid.y + (nl - 1 - layer) as f32 * v.cell_h;
            for e in e0..e1 {
                let tint = use_res && self.res[layer * ne + e] == 1;
                if tint {
                    painted += 1;
                }
                cell_quads.push(Quad {
                    rect: [v.grid.x + e as f32 * v.cell, y, v.cell, v.cell_h],
                    color: if tint { CELL_EST_VRAM } else { CELL_OFF },
                });
            }
        }
        self.res_cells = painted;
        self.visible = cell_quads.len();
        let dev = &self.device;
        let q = &self.queue;
        let lay = &self.store_layout;
        self.n_cells = self.cells.upload(dev, q, &cell_quads, lay, "cells") as usize;

        let mut grid_quads: Vec<Quad> = Vec::with_capacity(1200);
        let gh = (l1 - l0) as f32 * v.cell_h;
        let gw = (e1 - e0) as f32 * v.cell;
        let gy0 = v.grid.y + (nl - l1) as f32 * v.cell_h;
        let gx0 = v.grid.x + e0 as f32 * v.cell;
        for e in e0..=e1 {
            let x = v.grid.x + e as f32 * v.cell;
            grid_quads.push(Quad {
                rect: [x, gy0, 1.0, gh],
                color: [0.0, 0.0, 0.0, 0.55],
            });
        }
        for l in l0..=l1 {
            let y = v.grid.y + (nl - l) as f32 * v.cell_h;
            grid_quads.push(Quad {
                rect: [gx0, y, gw, 1.0],
                color: [0.0, 0.0, 0.0, 0.55],
            });
        }
        self.n_lattice = self.lattice.upload(dev, q, &grid_quads, lay, "lattice") as usize;
    }

    fn push_text(&self, tb: &mut TextBuf, px: u32, x: f32, y: f32, s: &str, color: [f32; 4], max_x: f32) -> f32 {
        let face = &self.text[&px].face;
        let out = tb.by_size.get_mut(&px).expect("text buffer for size");
        text::push_str(out, face, x, y, s, color, max_x)
    }

    /// Left label / right value row, `step` pixels tall.
    fn row(&self, tb: &mut TextBuf, x: f32, max_x: f32, label: &str, val: &str, col: [f32; 4], y: &mut f32, step: f32) {
        let f = self.face(SZ_BODY);
        let lw = text::text_width(f, label) + 10.0;
        self.push_text(tb, SZ_SMALL, x, *y + 2.0, label, FAINT, max_x);
        let v = text::fit(f, val, (max_x - x - lw).max(20.0));
        self.push_text(tb, SZ_BODY, x + lw, *y, &v, col, max_x);
        *y += step;
    }

    fn right_text(&self, tb: &mut TextBuf, px: u32, x_right: f32, y: f32, s: &str, col: [f32; 4], max_x: f32) {
        let w = text::text_width(self.face(px), s);
        self.push_text(tb, px, (x_right - w).max(x_right - 200.0), y, s, col, max_x);
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
        let (nl, _ne) = self.dim;
        let has_grid = v.cell > 0.0 && !v.lattice_panel.empty();
        let mut q: Vec<Quad> = Vec::with_capacity(2048);
        let mut tb = TextBuf::new();
        let mut lamp_q: Vec<Quad> = Vec::with_capacity(4096);
        let mut bloom_q: Vec<Quad> = Vec::with_capacity(4096);
        let mut row_q: Vec<Quad> = Vec::with_capacity(256);
        let mut rain_g: Vec<GInst> = Vec::with_capacity(2048);
        let mut bg_g: Vec<GInst> = Vec::with_capacity(512);
        let mut wash_g: Vec<GInst> = Vec::with_capacity(1400);
        let mut trail_q: Vec<Quad> = Vec::with_capacity(8192);
        let mut pulse_q: Vec<Quad> = Vec::with_capacity(MAX_PULSES * 4 + 8);

        // ---------------------------------------------------------- rain state
        // The update runs before anything is laid out because TWO passes use the
        // result: a low-alpha background pass (behind the panels, visible in the
        // gutters) and the full pass over the lattice.
        rain_eng.density = st.rain_density;
        rain_eng.speed_mult = st.rain_speed;
        let mut streams = 0usize;
        // The co-activation view is a data matrix, not the lattice: rain over it
        // would obscure the real pair counts, so the ambient layer stays off there.
        if st.rain && has_grid && v.view != VIEW_COACT {
            rain_eng.resize(v.w, v.h);
            rain_eng.update(dt_ms, false);
            streams = rain_eng.streams.len();
            let slots = &self.text[&SZ_SMALL].face.slots;
            let ascent = self.text[&SZ_SMALL].face.ascent;

            // where activation bursts are on screen right now (§8.3)
            let mut burst_pts: Vec<(f32, f32, f32)> = Vec::with_capacity(MAX_BURSTS);
            for b in overlay.bursts.iter() {
                if let Some(t) = overlay.burst_progress(b, now) {
                    if b.layer as usize >= nl {
                        continue;
                    }
                    let bx = v.grid.x + b.expert as f32 * v.cell + v.cell * 0.5;
                    let by = v.grid.y + (nl - 1 - b.layer as usize) as f32 * v.cell_h + v.cell_h * 0.5;
                    burst_pts.push((bx, by, 1.0 - t));
                }
            }

            // background pass: the leading glyph of every other stream, dim
            for (i, s) in rain_eng.streams.iter().enumerate() {
                if i % 2 != 0 {
                    continue;
                }
                let ch = *rain_eng.glyphs.get(s.codes[0] as usize).unwrap_or(&'0');
                let Some(slot) = slots.get(&ch) else { continue };
                let y = s.y - ascent;
                if y < -RAIN_FONT_PX || y > v.h {
                    continue;
                }
                bg_g.push(GInst {
                    rect: [s.x - slot.w * 0.5 + slot.ox, y + slot.oy, slot.w, slot.h],
                    uv: [slot.u0, slot.v0, slot.du, slot.dv],
                    color: [GREEN[0], GREEN[1], GREEN[2], 0.16 * BG_OPACITY],
                });
            }

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
                    let mut a = fade.max(0.05) * 0.85;
                    // activation-linked intensification: glyphs near a live burst
                    // are brighter and longer (spawned by a real selection)
                    let mut boost = 0.0f32;
                    if !burst_pts.is_empty() {
                        for (bx, by, inten) in burst_pts.iter() {
                            let dx = s.x - bx;
                            let dy = (y + ascent) - by;
                            let d = (dx * dx + dy * dy).sqrt();
                            if d < BURST_RADIUS {
                                boost = boost.max(inten * (1.0 - d / BURST_RADIUS));
                            }
                        }
                    }
                    a = (a * (1.0 + boost * 2.2)).min(1.0);
                    if budget > 0 {
                        budget -= 1;
                        let lead = i == 0;
                        if lead || boost > 0.25 {
                            rain_g.push(GInst {
                                rect: [s.x - slot.w * 0.5 - 2.0 + slot.ox, y + slot.oy - 1.5, slot.w + 4.0, slot.h + 3.0],
                                uv: [slot.u0, slot.v0, slot.du, slot.dv],
                                color: [0.25, 1.0, 0.45, a * 0.28],
                            });
                        }
                        let col = if lead || boost > 0.35 {
                            [BRIGHT[0], BRIGHT[1], BRIGHT[2], a]
                        } else {
                            [GREEN[0], GREEN[1], GREEN[2], a]
                        };
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
                            color: [GREEN[0], GREEN[1], GREEN[2], wa],
                        });
                    }
                }
            }

            // burst sparks: a few extra glyphs around each live burst, so the
            // rain visibly intensifies where an expert just fired
            for (bi, (bx, by, inten)) in burst_pts.iter().enumerate() {
                let n = 3usize;
                for k in 0..n {
                    let code = (bi * 7 + k * 13 + 3) % rain_eng.glyphs.len().max(1);
                    let ch = rain_eng.glyphs[code];
                    let Some(slot) = slots.get(&ch) else { continue };
                    let ang = (k as f32) * 2.1 + bi as f32;
                    let rad = 10.0 + (1.0 - inten) * 26.0;
                    let px = bx + ang.cos() * rad;
                    let py = by + ang.sin() * rad - (1.0 - inten) * 8.0;
                    if px < v.lattice_panel.x || px > v.lattice_panel.right()
                        || py < v.lattice_panel.y || py > v.lattice_panel.bottom() {
                        continue;
                    }
                    rain_g.push(GInst {
                        rect: [px - slot.w * 0.5 + slot.ox, py + slot.oy, slot.w, slot.h],
                        uv: [slot.u0, slot.v0, slot.du, slot.dv],
                        color: [BRIGHT[0], BRIGHT[1], BRIGHT[2], (0.75 * inten).min(1.0)],
                    });
                }
            }
        }

        // ---------------------------------------------------------- chrome
        for r in v.chromes() {
            panel(&mut q, r);
        }

        // ---------------------------------------------------------- header
        self.header(st, &mut q, &mut tb);
        let banner_quads = self.banner(st, &mut q, &mut tb, now);

        // ---------------------------------------------------------- panels
        if !v.sessions.empty() {
            self.panel_sessions(v.sessions, st, &mut q, &mut tb);
        }
        if !v.activation.empty() {
            if v.view == VIEW_COACT {
                self.panel_coact_side(v.activation, st, &mut q, &mut tb);
            } else {
                self.panel_activation(v.activation, st, overlay, &mut q, &mut tb);
            }
        }
        if !v.perf.empty() {
            self.panel_perf(v.perf, st, &mut q, &mut tb);
        }
        if !v.stream.empty() {
            self.panel_stream(v.stream, st, &mut q, &mut tb);
        }
        if !v.hardware.empty() {
            self.panel_hardware(v.hardware, st, &mut q, &mut tb);
        }
        if !v.full.empty() {
            match v.view {
                VIEW_SESSIONS => self.panel_sessions(v.full, st, &mut q, &mut tb),
                VIEW_PERF => self.panel_perf(v.full, st, &mut q, &mut tb),
                VIEW_STREAM => self.panel_stream(v.full, st, &mut q, &mut tb),
                _ => self.panel_activation(v.full, st, overlay, &mut q, &mut tb),
            }
        }

        // ---------------------------------------------------------- centre
        if has_grid {
            if v.view == VIEW_COACT {
                self.coact_matrix(st, &mut q, &mut tb);
            } else {
                self.lattice(st, overlay, &mut tb, &mut row_q);
            }
        }
        self.footer(st, &mut tb);

        // ---------------------------------------------------------- trails
        let trail_quads = if has_grid && v.view != VIEW_COACT {
            self.trails(st, &mut trail_q)
        } else {
            0
        };

        // ---------------------------------------------------------- lamps
        let glow = (st.glow / 0.6).clamp(0.0, 2.0) * GLOW;
        if has_grid {
            let mut painted = 0usize;
            let mut bloom_n = 0usize;
            let mut borders = 0usize;
            let (l0, l1, e0, e1) = v.vis;
            let mut push_lamp = |m: &Marker, a: f32| {
                if painted >= TRAIL_PAINT_LIMIT {
                    return;
                }
                if (m.layer as usize) < l0 || (m.layer as usize) >= l1
                    || (m.expert as usize) < e0 || (m.expert as usize) >= e1 {
                    return;
                }
                painted += 1;
                let x = v.grid.x + m.expert as f32 * v.cell;
                let y = v.grid.y + (nl - 1 - m.layer as usize) as f32 * v.cell_h;
                let g = a * GREEN_MAX;
                if g >= 0.012 {
                    lamp_q.push(Quad {
                        rect: [x, y, v.cell.max(1.0), v.cell_h.max(1.0)],
                        color: [0.470, 1.0, 0.651, g],
                    });
                    // §7.3 "apply a bright border or glow": a 1 px white-green
                    // stroke on the cells that are actually lit right now, capped
                    // so a full-lattice flash cannot quadruple the quad count
                    if a >= 0.5 && borders < BORDER_CAP {
                        borders += 1;
                        let bw = (v.cell * 0.22).clamp(0.8, 1.6);
                        for (rx, ry, rw, rh) in [
                            (x, y, v.cell.max(1.0), bw),
                            (x, y + v.cell_h.max(1.0) - bw, v.cell.max(1.0), bw),
                            (x, y, bw, v.cell_h.max(1.0)),
                            (x + v.cell.max(1.0) - bw, y, bw, v.cell_h.max(1.0)),
                        ] {
                            lamp_q.push(Quad { rect: [rx, ry, rw, rh], color: [0.92, 1.0, 0.95, a * 0.85] });
                        }
                    }
                }
                if a >= BLOOM_MIN && bloom_n < BLOOM_CAP {
                    bloom_n += 1;
                    let ba = (a - BLOOM_MIN) / (1.0 - BLOOM_MIN);
                    for (ring, alpha) in [(5.0f32, 0.16f32), (2.0, 0.30), (0.0, 0.55)] {
                        bloom_q.push(Quad {
                            rect: [x - ring, y - ring, v.cell.max(1.0) + 2.0 * ring, v.cell_h.max(1.0) + 2.0 * ring],
                            color: [0.776, 1.0, 0.863, alpha * ba * glow],
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

        // ---------------------------------------------------------- pulses
        let mut pulse_quads = 0usize;
        if has_grid && st.rain && !st.motion_reduced {
            for p in overlay.pulses.iter() {
                let Some(t) = overlay.pulse_progress(p, now) else { continue };
                if (p.layer as usize) >= nl {
                    continue;
                }
                let x = v.grid.x + p.expert as f32 * v.cell + v.cell * 0.5;
                let y = v.grid.y + (nl - 1 - p.layer as usize) as f32 * v.cell_h + v.cell_h * 0.5;
                let grow = t * BURST_RADIUS * 0.55;
                let a = (1.0 - t) * 0.55;
                if a <= 0.01 {
                    continue;
                }
                let (w, h) = (grow * 2.0, grow * 2.0);
                let t1 = (1.0f32).max(grow * 0.06);
                for (rx, ry, rw, rh) in [
                    (x - grow, y - grow, w, t1),
                    (x - grow, y + grow, w, t1),
                    (x - grow, y - grow, t1, h),
                    (x + grow, y - grow, t1, h),
                ] {
                    pulse_q.push(Quad { rect: [rx, ry, rw.max(0.6), rh.max(0.6)],
                                        color: [EMERALD[0], EMERALD[1], EMERALD[2], a] });
                    pulse_quads += 1;
                }
            }
        }

        // ---------------------------------------------------------- upload + pass
        let nl_upload = self.lamps.upload(&self.device, &self.queue, &lamp_q, &self.store_layout, "lamps");
        let nb = self.bloom.upload(&self.device, &self.queue, &bloom_q, &self.store_layout, "bloom");
        let nh = self.rowheat.upload(&self.device, &self.queue, &row_q, &self.store_layout, "rowheat");
        let nr = self.rain.upload(&self.device, &self.queue, &rain_g, &self.store_layout, "rain");
        let nbg = self.bgrain.upload(&self.device, &self.queue, &bg_g, &self.store_layout, "bgrain");
        let nw = self.wash.upload(&self.device, &self.queue, &wash_g, &self.store_layout, "wash");
        let nq = self.chrome.upload(&self.device, &self.queue, &q, &self.store_layout, "chrome");
        let ntr = self.trails_buf.upload(&self.device, &self.queue, &trail_q, &self.store_layout, "trails");
        let npu = self.pulses_buf.upload(&self.device, &self.queue, &pulse_q, &self.store_layout, "pulses");
        let mut nt: HashMap<u32, u32> = HashMap::new();
        for (px, buf) in self.text_bufs.iter_mut() {
            let data = tb.by_size.get(px).cloned().unwrap_or_default();
            nt.insert(*px, buf.upload(&self.device, &self.queue, &data, &self.store_layout, "text"));
        }
        let glyph_count: usize = tb.by_size.values().map(|v| v.len()).sum();

        // the lattice panel is the clipping rectangle for every grid pass, so a
        // zoomed/panned lattice can never paint over the side panels
        let clip = if v.lattice_panel.empty() { v.full } else { v.lattice_panel };
        let sc = self.scale;
        let clip_rect = scissor(clip, sc, self.config.width, self.config.height);

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
            // ambient background rain: drawn BEHIND the panels, so it lives in
            // the gutters and margins and never crosses a data panel
            if nbg > 0 {
                pass.set_pipeline(&self.glyph_pipe);
                pass.set_bind_group(1, &self.bgrain.bind, &[]);
                pass.set_bind_group(2, &self.text[&SZ_SMALL].bind, &[]);
                pass.draw(0..6, 0..nbg);
            }
            // chrome: the panels are the frame, everything else draws into it
            pass.set_pipeline(&self.quad_pipe);
            pass.set_bind_group(1, &self.chrome.bind, &[]);
            pass.draw(0..6, 0..nq);
            // rain, clipped to the lattice panel so the text panels stay legible
            if nr > 0 {
                pass.set_pipeline(&self.glyph_pipe);
                pass.set_scissor_rect(clip_rect.0, clip_rect.1, clip_rect.2, clip_rect.3);
                pass.set_bind_group(1, &self.rain.bind, &[]);
                pass.set_bind_group(2, &self.text[&SZ_SMALL].bind, &[]);
                pass.draw(0..6, 0..nr);
                pass.set_scissor_rect(0, 0, self.config.width, self.config.height);
            }
            // cells + lattice + layer-row heat. In the co-activation view the same
            // panel holds the pair matrix, so the lattice's own cell/grid buffers
            // must not be drawn over it.
            let draw_lattice = has_grid && v.view != VIEW_COACT;
            if draw_lattice && self.n_cells > 0 {
                pass.set_scissor_rect(clip_rect.0, clip_rect.1, clip_rect.2, clip_rect.3);
                pass.set_bind_group(1, &self.cells.bind, &[]);
                pass.draw(0..6, 0..self.n_cells as u32);
            }
            if draw_lattice && self.n_lattice > 0 {
                pass.set_bind_group(1, &self.lattice.bind, &[]);
                pass.draw(0..6, 0..self.n_lattice as u32);
            }
            if draw_lattice && nh > 0 {
                pass.set_bind_group(1, &self.rowheat.bind, &[]);
                pass.draw(0..6, 0..nh);
            }
            pass.set_scissor_rect(0, 0, self.config.width, self.config.height);
            // wash clipped to the lattice footprint
            if nw > 0 && draw_lattice {
                pass.set_pipeline(&self.glyph_pipe);
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
            // historical trails (emerald bars, under the live lamps)
            if ntr > 0 {
                pass.set_pipeline(&self.quad_pipe);
                pass.set_scissor_rect(clip_rect.0, clip_rect.1, clip_rect.2, clip_rect.3);
                pass.set_bind_group(1, &self.trails_buf.bind, &[]);
                pass.draw(0..6, 0..ntr);
                pass.set_scissor_rect(0, 0, self.config.width, self.config.height);
            }
            // lamps
            if nl_upload > 0 {
                pass.set_pipeline(&self.quad_pipe);
                pass.set_scissor_rect(clip_rect.0, clip_rect.1, clip_rect.2, clip_rect.3);
                pass.set_bind_group(1, &self.lamps.bind, &[]);
                pass.draw(0..6, 0..nl_upload);
            }
            // activation pulses
            if npu > 0 {
                pass.set_scissor_rect(clip_rect.0, clip_rect.1, clip_rect.2, clip_rect.3);
                pass.set_bind_group(1, &self.pulses_buf.bind, &[]);
                pass.draw(0..6, 0..npu);
            }
            // bloom
            if nb > 0 {
                pass.set_pipeline(&self.quad_add_pipe);
                pass.set_bind_group(1, &self.bloom.bind, &[]);
                pass.draw(0..6, 0..nb);
            }
            pass.set_scissor_rect(0, 0, self.config.width, self.config.height);
            // text, one draw per glyph size (each size has its own atlas)
            pass.set_pipeline(&self.glyph_pipe);
            for px in SIZES {
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

        FrameStats {
            quads: q.len(),
            glyphs: glyph_count,
            lamps: lamp_q.len(),
            streams,
            trail_quads,
            pulse_quads,
            banner_quads,
            visible_cells: self.visible,
            families: rain_eng.families_live(),
            res_cells: self.res_cells,
            res_len: self.res.len(),
            res_epoch_seen: self.res_epoch,
        }
    }



    // ------------------------------------------------------------ banner
    /// §8.4 FOLLOW THE WHITE RABBIT: reveal left→right out of cycling glyphs,
    /// hold (with an occasional glitch), dissolve back into the rain. Decorative:
    /// it obeys the ambient animation gate (`--rain off` silences it) and never
    /// depends on inference activity.
    fn banner(&self, st: &State, q: &mut Vec<Quad>, tb: &mut TextBuf, now: f64) -> usize {
        let b = &st.banner;
        let r = self.layout.banner;
        if r.empty() || !st.rain || !b.enabled || b.phase == 0 || st.motion_reduced {
            return 0;
        }
        let chars: Vec<char> = "FOLLOW THE WHITE RABBIT".chars().collect();
        let n = chars.len() as f64;
        let tick = (now / 55.0) as u64;
        q.push(Quad { rect: [r.x, r.y, r.w, r.h], color: [0.012, 0.075, 0.038, 0.72] });
        q.push(Quad { rect: [r.x, r.y, r.w, 1.0], color: [PANEL_EDGE[0], PANEL_EDGE[1], PANEL_EDGE[2], 0.9] });
        q.push(Quad { rect: [r.x, r.y + r.h - 1.0, r.w, 1.0], color: [PANEL_EDGE[0], PANEL_EDGE[1], PANEL_EDGE[2], 0.9] });
        let revealed = match b.phase {
            1 => ((b.progress * n).ceil() as usize).min(chars.len()),
            3 => chars.len(),
            _ => chars.len(),
        };
        let base = match b.phase {
            1 => 0.45 + 0.55 * b.progress as f32,
            2 => 0.90 + 0.10 * (((now / 130.0).sin() as f32) * 0.5 + 0.5),
            3 => (1.0 - b.progress) as f32,
            _ => 0.0,
        };
        let px = SZ_TITLE;
        let mut pen = r.x + 10.0;
        let mut quads = 0usize;
        for (i, ch) in chars.iter().enumerate() {
            let glitch = b.phase == 2 && (now as u64) % 700 < 90 && (i == 4 || i == 13 || i == 17);
            let dissolve = b.phase == 3 && (i as f64) < (b.progress as f64 * 1.3 * n);
            let unresolved = i >= revealed;
            let glyph = glitch || dissolve || unresolved;
            let c = if glyph {
                char::from_u32(0xff66 + ((i as u64 * 7 + tick * 13) % 0x37) as u32).unwrap_or('·')
            } else {
                *ch
            };
            let a = (base * if glyph { 0.55 } else { 1.0 }).clamp(0.0, 1.0);
            if a <= 0.01 {
                continue;
            }
            let col = if glyph {
                [GREEN[0], GREEN[1], GREEN[2], a]
            } else {
                [BRIGHT[0], BRIGHT[1], BRIGHT[2], a]
            };
            let s: String = c.to_string();
            pen = self.push_text(tb, px, pen, r.y + 1.0, &s, col, r.right() - 4.0);
            quads += 1;
        }
        // reveal progress: a thin rule that fills left to right
        let f = if b.phase == 1 { b.progress as f32 } else if b.phase == 2 { 1.0 } else { (1.0 - b.progress) as f32 };
        q.push(Quad { rect: [r.x, r.y + r.h - 2.0, (r.w * f).max(1.0), 1.0],
                      color: [GREEN[0], GREEN[1], GREEN[2], 0.55] });
        let _ = pen;
        quads
    }

    // ------------------------------------------------------------ trails
    /// Historical trails (§7.4): a thin emerald bar per cell with real history.
    /// Bar length scales with the log of the cell's selection count, alpha with
    /// how recently it was last selected (event timestamp). Deliberately a
    /// different colour AND a different position from the live lamp fill, so
    /// history can never be mistaken for current inference.
    fn trails(&self, st: &State, out: &mut Vec<Quad>) -> usize {
        if !st.trails {
            return 0;
        }
        let v = self.layout;
        let (nl, _ne) = self.dim;
        let (l0, l1, e0, e1) = v.vis;
        if l1 <= l0 || e1 <= e0 {
            return 0;
        }
        let now_wall = crate::now_unix();
        let mut maxc = 1u32;
        for l in l0..l1 {
            for e in e0..e1 {
                maxc = maxc.max(st.cell(l, e).0);
            }
        }
        // keep the quad budget bounded on a full 48x512 lattice: raise the
        // threshold until the estimated number of bars fits
        let count_at = |t: u32| -> usize {
            let mut n = 0usize;
            for l in l0..l1 {
                for e in e0..e1 {
                    if st.cell(l, e).0 >= t {
                        n += 1;
                    }
                }
            }
            n
        };
        // Smallest threshold that still fits the quad budget, so a dense lattice
        // keeps its long tail of cells instead of only the peak. The common case
        // (fewer than the budget) costs one pass.
        let thresh = if count_at(1) <= TRAIL_QUAD_CAP {
            1
        } else {
            (1..7u32)
                .rev()
                .map(|k| (maxc >> k).max(1))
                .find(|t| count_at(*t) <= TRAIL_QUAD_CAP)
                .unwrap_or(maxc)
        };
        let lmax = (maxc as f32).ln().max(1.0);
        let mut n = 0usize;
        for l in l0..l1 {
            for e in e0..e1 {
                let (c, ts) = st.cell(l, e);
                if c < thresh || c == 0 {
                    continue;
                }
                let frac = (c as f32).ln() / lmax;
                let age = (now_wall - ts).max(0.0);
                let recency = (1.0 - age / 300.0).clamp(0.30, 1.0) as f32;
                let x = v.grid.x + e as f32 * v.cell;
                let y = v.grid.y + (nl - 1 - l) as f32 * v.cell_h;
                let h = (v.cell_h * TRAIL_BAR_H).max(TRAIL_MIN_CELL);
                let w = (v.cell * frac).max(TRAIL_MIN_CELL);
                out.push(Quad {
                    rect: [x, y + v.cell_h - h, w, h],
                    color: [EMERALD[0], EMERALD[1], EMERALD[2], 0.16 + 0.55 * recency * frac],
                });
                n += 1;
            }
        }
        n
    }

    // ---------------------------------------------------------- co-activation
    /// View 6, lattice half: the co-selection matrix over the hottest experts.
    /// Each cell is how many times those two experts appeared in the SAME real
    /// top-k set (one expert_selected event = one routing decision).
    fn coact_matrix(&self, st: &State, q: &mut Vec<Quad>, tb: &mut TextBuf) {
        let v = self.layout;
        let p = v.lattice_panel;
        let max_x = p.right() - 10.0;
        self.push_text(tb, SZ_BODY, p.x + 12.0, p.y + 6.0,
            &format!("CO-ACTIVATION MATRIX · {} records folded · {} distinct pairs",
                     st.pairs_records, st.pairs_distinct()), DIM, max_x);
        self.push_text(tb, SZ_MICRO, p.x + 12.0, p.y + 26.0,
            "cell (i,j) = times that expert pair sat in the same top-k set · all layers · cumulative",
            FAINT, max_x);

        let hot = st.hot_experts(40);
        let n = hot.len();
        if n < 2 {
            self.push_text(tb, SZ_SMALL, p.x + 12.0, p.y + 60.0,
                "no co-activation yet — run inference through the collector; nothing is invented", AMBER, max_x);
            return;
        }
        let box_y = p.y + 48.0;
        let avail_w = (p.w - 120.0).max(80.0);
        let avail_h = (p.h - 120.0).max(80.0);
        let cell = (avail_w / n as f32).min(avail_h / n as f32).max(1.0);
        let gx = p.x + 72.0;
        let gy = box_y + 6.0;
        let mut mx = 1u32;
        for (i, (a, _)) in hot.iter().enumerate() {
            for (j, (b, _)) in hot.iter().enumerate() {
                if i == j {
                    continue;
                }
                mx = mx.max(st.pair(*a, *b));
            }
        }
        let lmx = (mx as f32).ln_1p();
        for (i, (a, _)) in hot.iter().enumerate() {
            for (j, (b, _)) in hot.iter().enumerate() {
                let c = if i == j { 0 } else { st.pair(*a, *b) };
                let (col, a2) = if i == j {
                    ([0.03, 0.05, 0.04, 1.0], 1.0)
                } else if c == 0 {
                    ([0.05, 0.08, 0.06, 1.0], 1.0)
                } else {
                    let f = ((c as f32).ln_1p() / lmx).min(1.0);
                    ([GREEN[0] * f + EMERALD[0] * (1.0 - f), GREEN[1] * f + EMERALD[1] * (1.0 - f),
                      GREEN[2] * f + EMERALD[2] * (1.0 - f), 1.0], 0.10 + 0.85 * f)
                };
                q.push(Quad { rect: [gx + j as f32 * cell, gy + i as f32 * cell, cell.max(1.0), cell.max(1.0)],
                              color: [col[0], col[1], col[2], a2] });
            }
        }
        // axis: every 5th expert of the crop
        for (i, (e, _)) in hot.iter().enumerate() {
            if i % 5 != 0 {
                continue;
            }
            let s = format!("{e}");
            let w = text::text_width(self.face(SZ_MICRO), &s);
            self.push_text(tb, SZ_MICRO, gx + i as f32 * cell + (cell - w) * 0.5, gy - 12.0, &s, FAINT, max_x);
            let y = gy + i as f32 * cell + cell * 0.5 - 5.0;
            self.push_text(tb, SZ_MICRO, gx - 30.0, y, &format!("E{e:03}"), FAINT, max_x);
        }
        let foot = p.bottom() - 34.0;
        self.push_text(tb, SZ_SMALL, p.x + 12.0, foot,
            &format!("hottest {} experts by selection count · peak pair count {} · log-scaled colour", n, mx),
            FAINT, max_x);
        self.push_text(tb, SZ_SMALL, p.x + 12.0, foot + 16.0,
            "6 switches back to the lattice · the ranks come from this app's own stream", FAINT, max_x);
    }

    /// View 6, side panel: the heaviest pairs, as a table with real counts.
    fn panel_coact_side(&self, r: Rect, st: &State, _q: &mut Vec<Quad>, tb: &mut TextBuf) {
        let x = r.x + 12.0;
        let max_x = r.right() - 10.0;
        let mut y = r.y + 8.0;
        self.push_text(tb, SZ_BODY, x, y, "CO-ACTIVATION", GREEN, max_x);
        self.push_text(tb, SZ_MICRO, x, y + 19.0,
            "expert pairs inside one real top-k set", FAINT, max_x);
        y += 38.0;
        let recs = st.pairs_records;
        let pairs = st.pairs_total();
        self.row(tb, x, max_x, "records folded", &format!("{recs}"), DIM, &mut y, 18.0);
        self.row(tb, x, max_x, "pairs counted", &format!("{pairs}"), DIM, &mut y, 18.0);
        self.row(tb, x, max_x, "distinct pairs", &format!("{}", st.pairs_distinct()), DIM, &mut y, 18.0);
        let k = if st.k > 0 { st.k } else { 0 };
        self.row(tb, x, max_x, "top-k per record", &format!("{k} · {} per record", if k > 1 { k * (k - 1) / 2 } else { 0 }), DIM, &mut y, 18.0);
        y += 8.0;
        self.push_text(tb, SZ_MICRO, x, y, "PAIR              COUNT   SHARE", FAINT, max_x);
        y += 14.0;
        let top = st.top_pairs(22);
        let denom = pairs.max(1) as f64;
        for (a, b, c) in top.iter() {
            if y + 14.0 > r.bottom() - 40.0 {
                break;
            }
            let line = format!("E{a:03} <-> E{b:03}   {c:>7}   {:>5.1}%", *c as f64 / denom * 100.0);
            self.push_text(tb, SZ_MICRO, x, y, &line, DIM, max_x);
            y += 14.0;
        }
        if top.is_empty() {
            self.push_text(tb, SZ_SMALL, x, y, if st.connected { "no pairs yet — nothing invented" } else { "collector not connected" }, AMBER, max_x);
        }
        self.push_text(tb, SZ_MICRO, x, r.bottom() - 34.0,
            "weights are not in the trace (serve path writes 0.0),", FAINT, max_x);
        self.push_text(tb, SZ_MICRO, x, r.bottom() - 21.0,
            "so this counts CO-SELECTION, never a score.", FAINT, max_x);
    }

    // ------------------------------------------------------------ header
    fn header(&self, st: &State, q: &mut Vec<Quad>, tb: &mut TextBuf) {
        let h = self.layout.header;
        let hx = h.x + 14.0;
        let max_x = h.right() - 10.0;
        let tx = self.push_text(tb, SZ_TITLE, hx, h.y + 4.0, "THE MATRIX", BRIGHT, max_x);
        self.push_text(tb, SZ_SMALL, tx + 16.0, h.y + 10.0,
            "stage 6 · local intelligence observatory · collector v1 socket", DIM, max_x);

        // model chip, right aligned
        let (model_txt, model_col) = match (&st.model, st.meta_source.as_deref()) {
            (Some(m), Some("collector-store")) => (format!("MODEL {m} · collector store"), DIM),
            (Some(m), _) => (format!("MODEL {m}"), DIM),
            (None, None) => ("MODEL unavailable — no meta event on the socket".to_string(), AMBER),
            (None, Some(src)) => (format!("MODEL unavailable — {src} meta carried no engine block"), AMBER),
        };
        let mw = text::text_width(self.face(SZ_SMALL), &model_txt) + 16.0;
        let mx = (h.right() - 12.0 - mw).max(hx + 320.0);
        chip(q, Rect { x: mx, y: h.y + 6.0, w: mw, h: 22.0 }, model_col);
        self.push_text(tb, SZ_SMALL, mx + 9.0, h.y + 9.0, &model_txt, model_col, h.right() - 12.0);

        // second row: connection, telemetry, placement, view tabs
        let row2 = h.y + h.h - 26.0;
        let mut cx = hx;
        let (conn_txt, conn_col) = if st.connected {
            (format!("COLLECTOR CONNECTED  {} peers", st.status.as_ref().map(|s| s.subscribers).unwrap_or(0)), GREEN)
        } else {
            ("COLLECTOR NOT CONNECTED — retrying".to_string(), RED)
        };
        cx = chip_text(self, q, tb, cx, row2, &conn_txt, conn_col);
        cx = chip_text(self, q, tb, cx, row2,
            &format!("SEQ {} · GAPS {} · SKIPPED {} B", st.last_seq, st.gaps_seen, st.skipped_bytes_seen),
            if st.gaps_seen > 0 { AMBER } else { DIM });
        let (pl_txt, pl_col) = if st.placement.monitor.is_empty() {
            ("MONITOR unknown".to_string(), AMBER)
        } else if st.placement.verified {
            (format!("{} {}x{} {}", st.placement.monitor, st.placement.w, st.placement.h,
                     if st.placement.fullscreen == 2 { "FULLSCREEN" } else { "WINDOWED" }), DIM)
        } else {
            (format!("{} (unverified placement)", st.placement.monitor), AMBER)
        };
        cx = chip_text(self, q, tb, cx, row2, &pl_txt, pl_col);
        self.push_text(tb, SZ_MICRO, cx + 6.0, row2 + 2.0,
            "1-6 views · j/k row · +/- zoom · w/a/s/d pan · 0 reset · t trails · b banner · r rain · f fullscreen",
            FAINT, max_x);

        // view tabs, right aligned
        let mut tab_x = h.right() - 12.0;
        for (i, name) in VIEW_NAMES.iter().enumerate().rev() {
            let on = st.view as usize == i;
            let w = text::text_width(self.face(SZ_MICRO), name) + 14.0;
            tab_x -= w;
            if on {
                chip(q, Rect { x: tab_x, y: row2 - 4.0, w, h: 20.0 }, GREEN);
            }
            self.push_text(tb, SZ_MICRO, tab_x + 7.0, row2 + 1.0, name,
                if on { BRIGHT } else { FAINT }, max_x);
            tab_x -= 4.0;
        }
    }

    // ------------------------------------------------------------ lattice
    fn lattice(&self, st: &State, overlay: &Overlay, tb: &mut TextBuf, row_q: &mut Vec<Quad>) {
        let v = self.layout;
        let p = v.lattice_panel;
        let (nl, ne) = self.dim;
        let max_x = p.right() - 8.0;
        self.push_text(tb, SZ_BODY, p.x + 12.0, p.y + 6.0,
            &format!("ROUTING LATTICE · {} layers × {} experts · top-k {} · cell {:.2} px",
                     nl, ne, if st.k > 0 { st.k } else { 10 }, v.cell),
            DIM, max_x);

        // per-layer activity heat: a real decaying measure of recent selections
        let hottest = overlay.heat.iter().copied().fold(0.0f32, f32::max).max(1.0);
        let (l0, l1, e0, e1) = v.vis;
        for layer in l0..l1 {
            let h = overlay.heat.get(layer).copied().unwrap_or(0.0);
            if h <= 0.02 {
                continue;
            }
            let y = v.grid.y + (nl - 1 - layer) as f32 * v.cell_h;
            let r = (h / hottest).powf(0.6);
            // a faint full-row tint plus a 3 px marker in the gutter: enough to
            // read "this layer is routing right now" without painting the row
            // bright enough to be mistaken for activations
            row_q.push(Quad {
                rect: [v.grid.x, y, v.grid.w, v.cell_h.max(1.0)],
                color: [0.42, 1.0, 0.58, 0.03 + r * 0.07],
            });
            row_q.push(Quad {
                rect: [v.grid.x - 6.0, y, 3.0, v.cell_h.max(1.0)],
                color: [0.55, 1.0, 0.70, 0.25 + r * 0.65],
            });
        }
        // selected row outline (interaction: j/k walks the layers)
        if st.sel_layer < nl && st.sel_layer >= l0 && st.sel_layer < l1 {
            let y = v.grid.y + (nl - 1 - st.sel_layer) as f32 * v.cell_h;
            for (yy, hh) in [(y - 1.0, 1.0f32), (y + v.cell_h, 1.0)] {
                row_q.push(Quad { rect: [v.grid.x - 2.0, yy, v.grid.w + 4.0, hh], color: [0.85, 1.0, 0.88, 0.85] });
            }
            row_q.push(Quad { rect: [v.grid.x - 2.0, y, 1.0, v.cell_h.max(1.0)], color: [0.85, 1.0, 0.88, 0.85] });
            row_q.push(Quad { rect: [v.grid.x + v.grid.w + 1.0, y, 1.0, v.cell_h.max(1.0)], color: [0.85, 1.0, 0.88, 0.85] });
        }

        // layer gutter labels (only the rows the viewport shows)
        let step = if nl > 32 { 8 } else { 4 };
        for layer in (l0..l1).step_by(step.max(1)) {
            let y = v.grid.y + (nl - 1 - layer) as f32 * v.cell_h + v.cell_h * 0.5 - 6.0;
            let s = format!("L{layer:02}");
            let wpx = text::text_width(self.face(SZ_MICRO), &s);
            let col = if layer == st.sel_layer { BRIGHT } else { FAINT };
            self.push_text(tb, SZ_MICRO, v.grid.x - 6.0 - wpx, y, &s, col, max_x);
        }
        let estep = if v.cell >= 14.0 { 32 } else if v.cell >= 7.0 { 64 } else { 128 };
        for e in (e0..e1).step_by(estep) {
            let x = v.grid.x + e as f32 * v.cell;
            if x > max_x {
                break;
            }
            self.push_text(tb, SZ_MICRO, x, v.grid.bottom() + 4.0, &format!("{e}"), FAINT, max_x);
        }

        // selected layer readout (per-layer selection count, exact)
        let sel = st.sel_layer.min(nl.saturating_sub(1));
        let cnt = st.layer_counts.get(sel).copied().unwrap_or(0);
        let total = st.layer_counts.iter().sum::<u64>().max(1);
        let share = cnt as f64 / total as f64 * 100.0;
        let row_y = p.bottom() - 34.0;
        let s = format!("ROW L{sel:02} · {cnt} selections ({share:.1}% of {} records) · heat {:.2}",
            st.records, overlay.heat.get(sel).copied().unwrap_or(0.0));
        self.push_text(tb, SZ_SMALL, p.x + 12.0, row_y, &s, if cnt > 0 { DIM } else { FAINT }, max_x);
        let vp = format!("VIEW zoom {:.2}x · pan {:+.0},{:+.0} · {} cells drawn · {} of {} rows · {} of {} experts · trails {} (+)",
            st.zoom, st.pan_x, st.pan_y, self.visible, l1 - l0, nl, e1 - e0, self.dim.1,
            if st.trails { "on" } else { "off" });
        self.push_text(tb, SZ_MICRO, p.x + 12.0, p.bottom() - 48.0, &vp,
            if st.zoom != 1.0 || st.pan_x != 0.0 || st.pan_y != 0.0 { GREEN } else { FAINT }, max_x);
        let cline = match (&st.current_session, st.connected) {
            (Some(id), _) => format!("live session {} · passes {} · positions {} · records {}",
                id, st.passes, st.positions, st.records),
            (None, true) => "no active session — lattice idle, rain continues · no activity is invented".to_string(),
            (None, false) => "collector not connected — no data to draw".to_string(),
        };
        self.push_text(tb, SZ_SMALL, p.x + 12.0, p.bottom() - 16.0, &cline, DIM, max_x);
    }

    // ------------------------------------------------------------ sessions
    fn panel_sessions(&self, r: Rect, st: &State, _q: &mut Vec<Quad>, tb: &mut TextBuf) {
        let x = r.x + 12.0;
        let max_x = r.right() - 10.0;
        let mut y = r.y + 8.0;
        let wide = r.w >= 560.0;
        self.push_text(tb, SZ_BODY, x, y, "SESSIONS", GREEN, max_x);
        self.push_text(tb, SZ_MICRO, x, y + 19.0,
            "collector windows · tier is the collector's attribution at close", FAINT, max_x);
        y += 38.0;

        let sum_delta: i64 = st.sessions.iter().map(|s| s.request_delta.max(0)).sum();
        let unattributed = st.sessions.iter().filter(|s| s.tier == "unattributed").count();
        let live = st.sessions.iter().filter(|s| !s.finished).count();
        self.row(tb, x, max_x, "windows", &format!("{} in list · {} live · {} closed", st.sessions.len(), live, st.sessions.len() - live), DIM, &mut y, 18.0);
        self.row(tb, x, max_x, "started/finished", &format!("{} / {}", st.sessions_started, st.sessions_finished), DIM, &mut y, 18.0);
        self.row(tb, x, max_x, "sum Δreq", &format!("{} across {} windowed", sum_delta, st.sessions.iter().filter(|s| s.tier == "windowed").count()), DIM, &mut y, 18.0);
        let eng = st.status.as_ref().map(|s| s.engine_requests_total).unwrap_or(-1);
        self.row(tb, x, max_x, "engine requests", &if eng >= 0 { format!("{eng} total · {} unattributed",
            st.status.as_ref().map(|s| s.unattributed_requests).unwrap_or(0)) } else { "unavailable".into() },
            if eng >= 0 { DIM } else { AMBER }, &mut y, 18.0);
        self.row(tb, x, max_x, "unattributed wins", &format!("{unattributed}"), if unattributed > 0 { AMBER } else { DIM }, &mut y, 18.0);
        y += 6.0;

        if wide {
            self.push_text(tb, SZ_MICRO, x, y, "ID", FAINT, max_x);
            self.push_text(tb, SZ_MICRO, x + 190.0, y, "TIER", FAINT, max_x);
            self.push_text(tb, SZ_MICRO, x + 280.0, y, "STATE", FAINT, max_x);
            self.right_text(tb, SZ_MICRO, x + 430.0, y, "ΔREQ", FAINT, max_x);
            self.right_text(tb, SZ_MICRO, x + 500.0, y, "PASSES", FAINT, max_x);
            self.right_text(tb, SZ_MICRO, x + 570.0, y, "POS", FAINT, max_x);
            self.right_text(tb, SZ_MICRO, x + 660.0, y, "RECORDS", FAINT, max_x);
            self.right_text(tb, SZ_MICRO, x + 760.0, y, "B→A", FAINT, max_x);
            self.right_text(tb, SZ_MICRO, x + 850.0, y, "DURATION", FAINT, max_x);
            self.right_text(tb, SZ_MICRO, x + 960.0, y, "TRACE BYTES", FAINT, max_x);
            y += 15.0;
        }

        let step = if wide { 18.0 } else { 30.0 };
        let bottom = r.bottom() - 30.0;
        let mut shown = 0usize;
        for s in st.sessions.iter().rev() {
            if y + step > bottom {
                break;
            }
            shown += 1;
            let tier_col = match s.tier.as_str() {
                "windowed" => GREEN,
                "unattributed" => AMBER,
                _ => FAINT,
            };
            let state = if s.finished { format!("{}", if s.end_reason.is_empty() { "closed".into() } else { s.end_reason.clone() }) } else { "live".to_string() };
            if wide {
                let id = text::fit(self.face(SZ_SMALL), &s.id, 180.0);
                self.push_text(tb, SZ_SMALL, x, y, &id, if s.finished { DIM } else { BRIGHT }, max_x);
                self.push_text(tb, SZ_SMALL, x + 190.0, y, &s.tier, tier_col, max_x);
                self.push_text(tb, SZ_SMALL, x + 280.0, y, &state, DIM, max_x);
                let d = if s.request_delta >= 0 { s.request_delta.to_string() } else { "—".into() };
                self.right_text(tb, SZ_SMALL, x + 430.0, y, &d, if s.request_delta > 0 { BRIGHT } else { DIM }, max_x);
                self.right_text(tb, SZ_SMALL, x + 500.0, y, &fmt_i(s.passes, s.seen_passes), DIM, max_x);
                self.right_text(tb, SZ_SMALL, x + 570.0, y, &fmt_i(s.positions, s.seen_positions), DIM, max_x);
                self.right_text(tb, SZ_SMALL, x + 660.0, y, &fmt_i(s.records, s.seen_records), DIM, max_x);
                let ba = if s.requests_before >= 0 { format!("{}→{}", s.requests_before, s.requests_after) } else { "—".into() };
                self.right_text(tb, SZ_SMALL, x + 760.0, y, &ba, FAINT, max_x);
                let dur = if s.duration_s >= 0.0 { format!("{:.1}s", s.duration_s) } else { "—".into() };
                self.right_text(tb, SZ_SMALL, x + 850.0, y, &dur, FAINT, max_x);
                let tb2 = if s.trace_bytes >= 0 { format!("{}", s.trace_bytes) } else { "—".into() };
                self.right_text(tb, SZ_SMALL, x + 960.0, y, &tb2, FAINT, max_x);
            } else {
                let id = text::fit(self.face(SZ_SMALL), &s.id, max_x - x - 70.0);
                self.push_text(tb, SZ_SMALL, x, y, &id, if s.finished { DIM } else { BRIGHT }, max_x);
                self.right_text(tb, SZ_MICRO, max_x, y + 2.0, &s.tier, tier_col, max_x);
                let l2 = format!("Δ{} · {}p · {}r · {}", if s.request_delta >= 0 { s.request_delta.to_string() } else { "—".into() },
                    s.seen_passes, s.seen_records, state);
                self.push_text(tb, SZ_MICRO, x + 8.0, y + 14.0, &l2, FAINT, max_x);
            }
            y += step;
        }
        if shown == 0 {
            self.push_text(tb, SZ_SMALL, x, y, if st.connected { "no sessions yet — none invented" } else { "collector not connected" },
                AMBER, max_x);
        }
        self.push_text(tb, SZ_MICRO, x, r.bottom() - 16.0,
            "windowed = engine request seen · unattributed = closed with no request · * = seen by this app while open",
            FAINT, max_x);
    }

    // ------------------------------------------------------------ activation
    fn panel_activation(&self, r: Rect, st: &State, overlay: &Overlay, q: &mut Vec<Quad>, tb: &mut TextBuf) {
        let x = r.x + 12.0;
        let max_x = r.right() - 10.0;
        let mut y = r.y + 8.0;
        let wide = r.w >= 560.0;
        self.push_text(tb, SZ_BODY, x, y, "ACTIVATION", GREEN, max_x);
        self.push_text(tb, SZ_MICRO, x, y + 19.0,
            "expert_selected events · lamps are real selections (fade 5.7 s) · trails are history", FAINT, max_x);
        y += 38.0;

        self.row(tb, x, max_x, "records", &format!("{}", st.records), DIM, &mut y, 18.0);
        self.row(tb, x, max_x, "passes/positions", &format!("{} / {}", st.passes, st.positions), DIM, &mut y, 18.0);
        self.row(tb, x, max_x, "live lamps", &format!("{} (history {})", overlay.live().min(overlay.cur.len()), overlay.hist.len()), DIM, &mut y, 18.0);
        self.row(tb, x, max_x, "activated", &format!("{}", overlay.activated), DIM, &mut y, 18.0);
        let top = st.layer_counts.iter().copied().max().unwrap_or(0).max(1);
        let active_layers = st.layer_counts.iter().filter(|c| **c > 0).count();
        self.row(tb, x, max_x, "layers active", &format!("{} of {}", active_layers, self.dim.0), DIM, &mut y, 18.0);
        y += 4.0;

        // per-layer selection counts: the lattice's own numbers, plus a bar
        let bar_x = x + 34.0;
        let count_w = 46.0;
        let bar_w = (max_x - bar_x - count_w).max(30.0);
        let rows_h = (r.h * if wide { 0.46 } else { 0.42 }).max(120.0);
        let limit = (y + rows_h).min(r.bottom() - 150.0);
        self.push_text(tb, SZ_MICRO, x, y, "selections per layer", FAINT, max_x);
        y += 14.0;
        let step = ((limit - y) / self.dim.0.max(1) as f32).clamp(10.0, 14.0);
        for layer in 0..self.dim.0 {
            if y + step > limit {
                break;
            }
            let c = st.layer_counts[layer];
            let sel = layer == st.sel_layer;
            self.push_text(tb, SZ_MICRO, x, y, &format!("L{layer:02}"), if sel { BRIGHT } else { FAINT }, max_x);
            if c > 0 {
                let w = (bar_w * (c as f32 / top as f32)).max(2.0);
                q.push(Quad {
                    rect: [bar_x, y + 2.0, w, step - 4.0],
                    color: if sel { [0.55, 1.0, 0.70, 0.95] } else { [0.18, 0.62, 0.32, 0.85] },
                });
            }
            let label = if c > 0 { c.to_string() } else { "·".to_string() };
            self.right_text(tb, SZ_MICRO, max_x, y, &label, if c > 0 { DIM } else { FAINT }, max_x);
            y += step;
        }
        y += 6.0;

        // selected row readout
        let sel = st.sel_layer.min(self.dim.0.saturating_sub(1));
        let cnt = st.layer_counts.get(sel).copied().unwrap_or(0);
        self.push_text(tb, SZ_SMALL, x, y, &format!("ROW L{sel:02} · {cnt} selections · heat {:.2}",
            overlay.heat.get(sel).copied().unwrap_or(0.0)), BRIGHT, max_x);
        y += 20.0;

        // hot experts — exact counts from this app's stream
        self.push_text(tb, SZ_MICRO, x, y, "hottest experts (cumulative)", FAINT, max_x);
        y += 14.0;
        let hot = st.hot_experts(10);
        let hot_max = hot.first().map(|(_, c)| *c).unwrap_or(1).max(1);
        let hot_rows = if wide { 10 } else { 6 };
        for (i, (e, c)) in hot.iter().take(hot_rows).enumerate() {
            if y + 12.0 > r.bottom() - 96.0 {
                break;
            }
            self.push_text(tb, SZ_MICRO, x, y, &format!("E{e:03}"), DIM, max_x);
            let w = (bar_w * (*c as f32 / hot_max as f32)).max(1.0);
            q.push(Quad { rect: [bar_x, y + 2.0, w, 6.0], color: [0.30, 0.85, 0.50, 0.75] });
            self.right_text(tb, SZ_MICRO, max_x, y, &c.to_string(), FAINT, max_x);
            y += 12.0;
            let _ = i;
        }

        // honest unavailable block
        let uy = r.bottom() - 84.0;
        self.push_text(tb, SZ_MICRO, x, uy, "unavailable in the v1 stream (never invented):", FAINT, max_x);
        self.push_text(tb, SZ_MICRO, x, uy + 14.0, "· RESIDENCY TRUTH — none; lattice tint is a profile ESTIMATE", FAINT, max_x);
        self.push_text(tb, SZ_MICRO, x, uy + 27.0, "· ROUTING SCORES — serve-path weights are all 0.0", FAINT, max_x);
        self.push_text(tb, SZ_MICRO, x, uy + 40.0, "· TOKEN TEXT — pass_index only, no decoded tokens", FAINT, max_x);
        self.push_text(tb, SZ_MICRO, x, uy + 53.0, "· HISTORY REPLAY — Stage 7 (trails + view 6 are live)", FAINT, max_x);
    }

    // ------------------------------------------------------------ perf
    fn panel_perf(&self, r: Rect, st: &State, q: &mut Vec<Quad>, tb: &mut TextBuf) {
        let x = r.x + 12.0;
        let max_x = r.right() - 10.0;
        let mut y = r.y + 8.0;
        let wide = r.w >= 560.0;
        self.push_text(tb, SZ_BODY, x, y, "ENGINE PERF", GREEN, max_x);
        self.push_text(tb, SZ_MICRO, x, y + 19.0, "perf_sample · collector's /metrics poll", FAINT, max_x);
        y += 36.0;

        match &st.perf {
            Some(p) => {
                let state_col = if p.engine_ok { GREEN } else { RED };
                self.row(tb, x, max_x, "state", &if p.state.is_empty() { "unavailable".into() } else { p.state.clone() }, state_col, &mut y, 18.0);
                self.row(tb, x, max_x, "tok/s", &fmtf(p.tok_s, 1, ""), DIM, &mut y, 18.0);
                self.row(tb, x, max_x, "requests", &p.requests.map(|v| v.to_string()).unwrap_or("unavailable".into()), DIM, &mut y, 18.0);
                let (a, b) = (x + r.w * 0.5, x + r.w * 0.5);
                let mut y2 = y;
                self.row(tb, x, max_x, "prompt tok", &format!("{}", p.prompt_tokens), DIM, &mut y, 18.0);
                self.row(tb, x, max_x, "output tok", &format!("{}", p.output_tokens), DIM, &mut y, 18.0);
                self.row(tb, x, max_x, "drafts", &format!("{} / {} acc", p.drafts_offered, p.drafts_accepted), DIM, &mut y, 18.0);
                if wide {
                    // second column of the same real sample
                    let x2 = a;
                    self.row(tb, x2, max_x, "Δreq/Δout", &format!("{} / {}", p.delta_requests, p.delta_output), FAINT, &mut y2, 18.0);
                    self.row(tb, x2, max_x, "samples", &format!("{}", st.perf_samples), FAINT, &mut y2, 18.0);
                    self.row(tb, x2, max_x, "baseline", &format!("{}", p.baseline), FAINT, &mut y2, 18.0);
                    self.row(tb, x2, max_x, "engine", &st.engine_name.clone().unwrap_or("unavailable".into()), FAINT, &mut y2, 18.0);
                    let _ = b;
                    y = y.max(y2);
                }
                // tok/s sparkline of the samples actually received: a bounded
                // box (never the whole panel) so a handful of samples cannot
                // turn into giant bars
                let chart_h = (r.bottom() - y - 60.0).clamp(0.0, 140.0);
                if chart_h > 36.0 {
                    let cw = if wide { ((max_x - x) * 0.5).clamp(160.0, 620.0) } else { max_x - x };
                    self.push_text(tb, SZ_MICRO, x, y, &format!("tok/s history · {} samples", st.perf_hist.len()), FAINT, max_x);
                    let chart = Rect { x, y: y + 14.0, w: cw, h: chart_h };
                    q.push(Quad { rect: [chart.x, chart.y, chart.w, chart.h], color: [0.02, 0.06, 0.035, 0.9] });
                    let vmax = st.perf_hist.iter().filter_map(|(_, v)| *v).fold(0.0f64, f64::max).max(1.0);
                    let bw = (chart.w / 120.0).max(1.0).min(10.0);
                    for (i, (_ts, val)) in st.perf_hist.iter().rev().take(120).enumerate() {
                        if let Some(v) = val {
                            let hh = ((*v / vmax) as f32 * (chart.h - 4.0)).max(1.0);
                            let bx = chart.x + chart.w - (i as f32 + 1.0) * bw;
                            q.push(Quad {
                                rect: [bx, chart.y + chart.h - hh, (bw - 1.0).max(1.0), hh],
                                color: [0.30, 0.85, 0.50, 0.8],
                            });
                        }
                    }
                    self.push_text(tb, SZ_MICRO, chart.x, chart.bottom() + 2.0,
                        &format!("peak {vmax:.1} tok/s"), FAINT, max_x);
                    // the same samples as numbers, newest first
                    if wide {
                        let tx = chart.right() + 24.0;
                        self.push_text(tb, SZ_MICRO, tx, y, "SAMPLE (UTC)      TOK/S", FAINT, max_x);
                        let mut ry = y + 14.0;
                        for (tsample, val) in st.perf_hist.iter().rev().take(8) {
                            if ry + 12.0 > r.bottom() - 46.0 {
                                break;
                            }
                            let line = format!("{}   {}", hms(*tsample),
                                val.map(|v| format!("{v:.1}")).unwrap_or_else(|| "unavailable".into()));
                            self.push_text(tb, SZ_MICRO, tx, ry, &line, FAINT, max_x);
                            ry += 12.0;
                        }
                    }
                }
            }
            None => {
                self.row(tb, x, max_x, "state", "unavailable — no perf_sample", AMBER, &mut y, 18.0);
                self.row(tb, x, max_x, "tok/s", "unavailable", AMBER, &mut y, 18.0);
            }
        }
        self.push_text(tb, SZ_MICRO, x, r.bottom() - 40.0, "unavailable: TTFT · inter-token latency · prefill rate", FAINT, max_x);
        self.push_text(tb, SZ_MICRO, x, r.bottom() - 26.0, "perf_sample carries live.tok_s + totals only", FAINT, max_x);
        self.push_text(tb, SZ_MICRO, x, r.bottom() - 13.0, "— no such fields exist to draw", FAINT, max_x);
    }

    // ------------------------------------------------------------ stream
    fn panel_stream(&self, r: Rect, st: &State, _q: &mut Vec<Quad>, tb: &mut TextBuf) {
        let x = r.x + 12.0;
        let max_x = r.right() - 10.0;
        let mut y = r.y + 8.0;
        let wide = r.w >= 560.0;
        let tall = r.h > 400.0;
        let step = if tall { 21.0 } else { 17.0 };
        self.push_text(tb, SZ_BODY, x, y, "STREAM HEALTH", GREEN, max_x);
        self.push_text(tb, SZ_MICRO, x, y + 19.0,
            "telemetry_status + gap events + app-side accounting", FAINT, max_x);
        y += 38.0;
        let y_top = y;

        let s = st.status.as_ref();
        let colw = if wide { r.w * 0.32 } else { r.w };
        let c1 = x;
        let c2 = x + colw;
        let c3 = x + 2.0 * colw;

        let mut yc1 = y;
        self.row(tb, c1, max_x, "connection",
            &if st.connected { format!("up · {} reconnects", st.connects) }
             else { format!("down · {} connection errors", st.connect_errors) },
            if st.connected { GREEN } else { RED }, &mut yc1, step);
        self.row(tb, c1, max_x, "last seq", &format!("{} (max {})", st.last_seq, st.seq_max_seen), DIM, &mut yc1, step);
        self.row(tb, c1, max_x, "seq gaps", &format!("{} · {} missing", st.seq_gaps, st.seq_missing),
            if st.seq_missing > 0 { AMBER } else { DIM }, &mut yc1, step);
        self.row(tb, c1, max_x, "events/records", &format!("{} / {}", st.events_total, st.records), DIM, &mut yc1, step);
        self.row(tb, c1, max_x, "gap events", &format!("{} · {} bytes", st.gaps_seen, st.gap_bytes),
            if st.gaps_seen > 0 { AMBER } else { DIM }, &mut yc1, step);
        self.row(tb, c1, max_x, "skipped bytes", &format!("{} (seen of collector total)", st.skipped_bytes_seen),
            DIM, &mut yc1, step);
        let reasons: Vec<String> = {
            let mut v: Vec<(String, u64)> = st.gap_reasons.iter().map(|(k, c)| (k.clone(), *c)).collect();
            v.sort_by(|a, b| b.1.cmp(&a.1));
            v.iter().take(3).map(|(k, c)| format!("{k}x{c}")).collect()
        };
        self.row(tb, c1, max_x, "gap reasons", &if reasons.is_empty() { "none".into() } else { reasons.join(" ") }, FAINT, &mut yc1, step);

        match s {
            Some(s) => {
                let mut yc2 = y_top;
                self.row(tb, c2, max_x, "rates", &format!("{:.1} ev/s · {:.1} rec/s · {:.0} B/s",
                    s.events_per_s, s.records_per_s, s.bytes_per_s), DIM, &mut yc2, step);
                self.row(tb, c2, max_x, "queue ours", &format!("{} deep / {} cap (max {})", s.sub_depth, s.queue_cap, s.sub_max_depth),
                    if s.queue_cap > 0 && s.sub_depth * 2 > s.queue_cap { AMBER } else { DIM }, &mut yc2, step);
                self.row(tb, c2, max_x, "drops ours", &format!("{} droppable · {} forced", s.sub_dropped, s.sub_forced),
                    if s.sub_dropped + s.sub_forced > 0 { AMBER } else { DIM }, &mut yc2, step);
                self.row(tb, c2, max_x, "drops total", &format!("{} (all subscribers)", s.drops_total),
                    if s.drops_total > 0 { AMBER } else { DIM }, &mut yc2, step);
                self.row(tb, c2, max_x, "skipped total", &format!("{} B · {} since last event", s.skipped_bytes_total, s.skipped_since_last), DIM, &mut yc2, step);
                self.row(tb, c2, max_x, "resyncs/discard", &format!("{} / {}", s.resyncs_total, s.discarded_passes), DIM, &mut yc2, step);
                self.row(tb, c2, max_x, "replay ours", &format!("{} replayed · {} truncated", s.sub_replayed, s.sub_replay_truncated),
                    if s.sub_replay_truncated > 0 { AMBER } else { DIM }, &mut yc2, step);

                let mut yc3 = y_top;
                self.row(tb, c3, max_x, "trace", &format!("off {} / {} · held {}", s.trace_offset, s.file_size, s.held_records), FAINT, &mut yc3, step);
                self.row(tb, c3, max_x, "collector db", &format!("{} · wal {} B · seq {}", if s.db_ok { "ok" } else { "ERR" }, s.db_wal_bytes, s.db_max_seq),
                    if s.db_ok { DIM } else { RED }, &mut yc3, step);
                self.row(tb, c3, max_x, "engine reqs", &format!("{} · {} unattributed", s.engine_requests_total, s.unattributed_requests), DIM, &mut yc3, step);
                self.row(tb, c3, max_x, "subscribers", &format!("{} · queue cap {}", s.subscribers, s.queue_cap), DIM, &mut yc3, step);
                if wide && tall {
                    self.push_text(tb, SZ_MICRO, c3, yc3 + 8.0, "EVENT TYPES (this app)", FAINT, max_x);
                    let mut ey = yc3 + 24.0;
                    let mut ev: Vec<(&String, &u64)> = st.by_type.iter().collect();
                    ev.sort_by(|a, b| b.1.cmp(a.1));
                    for (k, v) in ev.iter().take(10) {
                        self.push_text(tb, SZ_MICRO, c3, ey, &format!("{:<24}{}", trunc(k, 24), v), FAINT, max_x);
                        ey += 13.0;
                    }
                }
                y = yc1.max(yc2).max(yc3) + 10.0;

                // last gap: the honest "something was lost" line
                if let Some(g) = st.gaps.last() {
                    let line = format!("last gap {} · {} B · {} · {}", g.reason, g.bytes,
                        if g.records_unattributed > 0 { format!("{} records unattributed", g.records_unattributed) }
                        else { "0 unattributed".into() },
                        trunc(&g.detail, 110));
                    self.push_text(tb, SZ_MICRO, x, y, &line, AMBER, max_x);
                    y += 16.0;
                }

                // subscribers: two columns when there is room
                if y + 40.0 < r.bottom() - 30.0 {
                    self.push_text(tb, SZ_MICRO, x, y, "SUBSCRIBER     ID  SENT      DROPPED  FORCED  DEPTH  REPLAYED  TRUNC  BYTES", FAINT, max_x);
                    y += 14.0;
                    for sub in s.subs.iter() {
                        if y + 12.0 > r.bottom() - 40.0 {
                            break;
                        }
                        let line = format!("{:<14} {:<3} {:<9} {:<8} {:<7} {:<6} {:<9} {:<6} {}",
                            trunc(&sub.name, 14), sub.id, sub.sent, sub.dropped, sub.forced,
                            sub.depth, sub.replayed, sub.truncated, sub.bytes_out);
                        let ours = s.sub_id == Some(sub.id);
                        self.push_text(tb, SZ_MICRO, x, y, &line,
                            if ours { BRIGHT } else if sub.closed { FAINT } else { DIM }, max_x);
                        y += 13.0;
                    }
                }

                // gap table, right half
                if wide && r.h > 380.0 && !st.gaps.is_empty() {
                    let gx = x + r.w * 0.52;
                    let mut gy = y_top + (r.h * 0.52);
                    self.push_text(tb, SZ_MICRO, gx, gy, "GAP EVENTS (collector, replayed on connect)", FAINT, max_x);
                    gy += 14.0;
                    for g in st.gaps.iter().rev().take(8) {
                        if gy + 13.0 > r.bottom() - 30.0 {
                            break;
                        }
                        let line = format!("{}  {:<20} {:>9} B  {:>5} unattr", hms(g.ts), trunc(&g.reason, 20), g.bytes, g.records_unattributed);
                        self.push_text(tb, SZ_MICRO, gx, gy, &line, AMBER, max_x);
                        gy += 13.0;
                    }
                }
            }
            None => {
                self.row(tb, x, max_x, "telemetry", "unavailable — no telemetry_status yet", AMBER, &mut y, step);
            }
        }
        self.push_text(tb, SZ_MICRO, x, r.bottom() - 14.0,
            "ours = this app's subscriber row · collector-wide counters are labelled (all subscribers)", FAINT, max_x);
    }

    // ------------------------------------------------------------ hardware
    fn panel_hardware(&self, r: Rect, st: &State, _q: &mut Vec<Quad>, tb: &mut TextBuf) {
        let x = r.x + 12.0;
        let max_x = r.right() - 10.0;
        let mut y = r.y + 8.0;
        self.push_text(tb, SZ_BODY, x, y, "HARDWARE", GREEN, max_x);
        self.push_text(tb, SZ_MICRO, x, y + 19.0, "hw_sample · nvidia-smi + /proc", FAINT, max_x);
        y += 36.0;
        match &st.hw {
            Some(h) => {
                self.row(tb, x, max_x, "gpu", &if h.gpu_name.is_empty() { "unavailable".into() } else { h.gpu_name.clone() }, DIM, &mut y, 17.0);
                self.row(tb, x, max_x, "gpu util", &fmtf(h.util_pct, 0, "%"), DIM, &mut y, 17.0);
                self.row(tb, x, max_x, "gpu mem", &fmtf(h.mem_used_mib, 0, " MiB"), DIM, &mut y, 17.0);
                self.row(tb, x, max_x, "temp/power", &format!("{} · {}", fmtf(h.temp_c, 0, " C"), fmtf(h.power_w, 0, " W")), DIM, &mut y, 17.0);
                self.row(tb, x, max_x, "load1", &fmtf(h.load1, 2, ""), DIM, &mut y, 17.0);
                self.row(tb, x, max_x, "mem avail", &fmtf(h.mem_available_mib, 0, " MiB"), DIM, &mut y, 17.0);
                self.row(tb, x, max_x, "collector rss", &h.rss_kib.map(|v| format!("{} KiB", v)).unwrap_or("unavailable".into()), FAINT, &mut y, 17.0);
            }
            None => {
                self.row(tb, x, max_x, "gpu", "unavailable — no hw_sample", AMBER, &mut y, 17.0);
            }
        }
        self.push_text(tb, SZ_MICRO, x, r.bottom() - 40.0, "aggregate only — per-expert residency is an estimate", FAINT, max_x);
        self.push_text(tb, SZ_MICRO, x, r.bottom() - 27.0, "is not in the v1 stream, so it is not drawn", FAINT, max_x);
        self.push_text(tb, SZ_MICRO, x, r.bottom() - 14.0, "NVMe/SSD metrics: unavailable", FAINT, max_x);
    }

    fn footer(&self, st: &State, tb: &mut TextBuf) {
        let f = self.layout.footer;
        let s = self.footer_text(st, crate::now_unix());
        self.push_text(tb, SZ_MICRO, f.x + 4.0, f.y + 2.0, &s, FAINT, f.right() - 4.0);
    }

    pub fn footer_text(&self, st: &State, now_wall: f64) -> String {
        let _ = now_wall;
        format!(
            "seq {} · last event {} UTC · view {} · rain {} (r) · {}x{} · {} · {} · frames {} · redraws {} · unchanged-repaints {}",
            st.last_seq, hms(st.last_event_ts), VIEW_NAMES[(st.view as usize).min(4)],
            if st.rain { "ON" } else { "OFF" },
            self.config.width, self.config.height, self.present_mode, self.adapter,
            st.frames, st.redraw_requests, st.repaints_unchanged
        )
    }
}

/* ------------------------------------------------------------------ helpers */

fn scissor(clip: Rect, scale: f32, w: u32, h: u32) -> (u32, u32, u32, u32) {
    let x = ((clip.x * scale).max(0.0)) as u32;
    let y = ((clip.y * scale).max(0.0)) as u32;
    let cw = ((clip.w * scale) as u32).min(w.saturating_sub(x)).max(1);
    let ch = ((clip.h * scale) as u32).min(h.saturating_sub(y)).max(1);
    (x, y, cw, ch)
}

fn panel(q: &mut Vec<Quad>, r: Rect) {
    q.push(Quad { rect: [r.x, r.y, r.w, r.h],
                  color: [PANEL_BG[0], PANEL_BG[1], PANEL_BG[2], PANEL_ALPHA] });
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
    chip(q, Rect { x, y: y - 5.0, w, h: 22.0 }, col);
    r.push_text(tb, SZ_SMALL, x + 9.0, y - 1.0, s, col, x + w);
    x + w + 8.0
}

fn trunc(s: &str, n: usize) -> String {
    let v: Vec<char> = s.chars().collect();
    if v.len() <= n {
        s.to_string()
    } else {
        let mut out: String = v[..n.saturating_sub(2)].iter().collect();
        out.push_str("..");
        out
    }
}

fn fmt_i(finished: i64, seen: u64) -> String {
    if finished >= 0 {
        finished.to_string()
    } else {
        format!("{seen}*")
    }
}

fn fmtf(v: Option<f64>, prec: usize, unit: &str) -> String {
    match v {
        Some(x) => format!("{x:.prec$}{unit}"),
        None => "unavailable".into(),
    }
}

fn hms(ts: f64) -> String {
    if ts <= 0.0 {
        return "n/a".to_string();
    }
    let secs = ts as u64 % 86_400;
    format!("{:02}:{:02}:{:02}.{}", secs / 3600, (secs / 60) % 60, secs % 60, ((ts * 10.0) as u64) % 10)
}
