// In-app state: everything the app knows comes from collector v1 events on the
// unix socket. The app never reads the engine, the trace, or the collector's
// SQLite file — if a field is not in the event stream it is reported as
// unavailable instead of being invented.
//
// Stage 5 extends this with what the panels actually display: the full session
// list (tier labels, exact per-session counts), the latest perf sample plus a
// short history of real samples, the stream-health counters and the hardware
// sample. Nothing here is derived from a model, a fixture or a guess.

use std::collections::HashMap;

use crate::json::{self, Value};

pub const EXPERT_SLOTS: usize = 512;
pub const LAYER_SLOTS: usize = 48;
pub const MAX_SESSIONS: usize = 512;
pub const PERF_HIST: usize = 240;
pub const MAX_GAPS: usize = 24;
pub const HOT_N: usize = 10;
pub const MIN_ZOOM: f32 = 0.5;
pub const MAX_ZOOM: f32 = 12.0;

/// Banner phases, in the order the spec describes them: the phrase reveals from
/// left to right (cycling glyphs first), holds, then dissolves back into the
/// rain. Decorative only — no telemetry drives it.
pub const BANNER_REVEAL_MS: f64 = 1700.0;
pub const BANNER_HOLD_MS: f64 = 2600.0;
pub const BANNER_DISSOLVE_MS: f64 = 1300.0;

#[derive(Clone, Copy)]
pub struct Banner {
    pub enabled: bool,
    pub interval_s: f64,
    pub t0_ms: f64,
    pub cycles: u64,
    pub phase: u8,
    pub progress: f64,
}

impl Banner {
    pub fn new(enabled: bool, interval_s: f64) -> Banner {
        Banner { enabled, interval_s: interval_s.max(4.0), t0_ms: f64::NEG_INFINITY,
                 cycles: 0, phase: 0, progress: 0.0 }
    }

    pub fn phase_name(&self) -> &'static str {
        match self.phase {
            1 => "reveal",
            2 => "hold",
            3 => "dissolve",
            _ => "gap",
        }
    }

    /// Advance the decorative clock. `ambient` is the app's animation gate (the
    /// same one the rain obeys), so `--rain off` silences the banner too and the
    /// strict idle measurement stays at zero frames. Returns true while a
    /// visible banner is on screen.
    pub fn step(&mut self, now: f64, ambient: bool) -> bool {
        if !self.enabled || !ambient {
            self.phase = 0;
            self.progress = 0.0;
            return false;
        }
        let span = BANNER_REVEAL_MS + BANNER_HOLD_MS + BANNER_DISSOLVE_MS;
        if now - self.t0_ms >= self.interval_s * 1000.0 {
            self.t0_ms = now;
            self.cycles += 1;
        }
        let age = (now - self.t0_ms).max(0.0);
        if age < BANNER_REVEAL_MS {
            self.phase = 1;
            self.progress = age / BANNER_REVEAL_MS;
        } else if age < BANNER_REVEAL_MS + BANNER_HOLD_MS {
            self.phase = 2;
            self.progress = (age - BANNER_REVEAL_MS) / BANNER_HOLD_MS;
        } else if age < span {
            self.phase = 3;
            self.progress = (age - BANNER_REVEAL_MS - BANNER_HOLD_MS) / BANNER_DISSOLVE_MS;
        } else {
            self.phase = 0;
            self.progress = 0.0;
        }
        self.phase != 0
    }

    pub fn trigger(&mut self, now: f64) {
        self.t0_ms = now;
        self.cycles += 1;
    }
}

#[derive(Clone)]
pub struct Session {
    pub id: String,
    pub tier: String,
    pub started_ts: f64,
    pub duration_s: f64,
    pub finished: bool,
    pub end_reason: String,
    pub requests_before: i64,
    pub requests_after: i64,
    pub request_delta: i64,
    pub passes: i64,
    pub positions: i64,
    pub records: i64,
    pub trace_bytes: i64,
    pub first_seq: i64,
    pub seen_passes: u64,
    pub seen_positions: u64,
    pub seen_records: u64,
}

impl Session {
    fn new(id: &str, started_ts: f64) -> Session {
        Session {
            id: id.to_string(),
            tier: "unknown".into(),
            started_ts,
            duration_s: -1.0,
            finished: false,
            end_reason: String::new(),
            requests_before: -1,
            requests_after: -1,
            request_delta: -1,
            passes: -1,
            positions: -1,
            records: -1,
            trace_bytes: -1,
            first_seq: -1,
            seen_passes: 0,
            seen_positions: 0,
            seen_records: 0,
        }
    }
}

#[derive(Clone)]
pub struct Perf {
    pub ts: f64,
    pub engine_ok: bool,
    pub error: String,
    pub baseline: bool,
    pub state: String,
    pub tok_s: Option<f64>,
    pub requests: Option<i64>,
    pub prompt_tokens: i64,
    pub output_tokens: i64,
    pub drafts_offered: i64,
    pub drafts_accepted: i64,
    pub delta_requests: i64,
    pub delta_prompt: i64,
    pub delta_output: i64,
    pub delta_drafts: i64,
}

#[derive(Clone, Default)]
pub struct Hw {
    pub ts: f64,
    pub gpu_name: String,
    pub util_pct: Option<f64>,
    pub mem_used_mib: Option<f64>,
    pub temp_c: Option<f64>,
    pub power_w: Option<f64>,
    pub load1: Option<f64>,
    pub mem_available_mib: Option<f64>,
    pub rss_kib: Option<i64>,
}

#[derive(Clone, Default)]
pub struct SubRow {
    pub id: i64,
    pub name: String,
    pub peer: String,
    pub since: f64,
    pub depth: i64,
    pub max_depth: i64,
    pub sent: i64,
    pub dropped: i64,
    pub forced: i64,
    pub replayed: i64,
    pub truncated: i64,
    pub bytes_out: i64,
    pub closed: bool,
}

#[derive(Clone, Default)]
pub struct GapRow {
    pub ts: f64,
    pub reason: String,
    pub bytes: i64,
    pub records_unattributed: i64,
    pub detail: String,
    pub session: String,
}

#[derive(Clone, Default)]
pub struct Status {
    pub ts: f64,
    pub uptime_s: f64,
    pub events_total: i64,
    pub sessions_total: i64,
    pub events_per_s: f64,
    pub records_per_s: f64,
    pub bytes_per_s: f64,
    pub subscribers: i64,
    pub queue_cap: i64,
    pub drops_total: i64,
    pub skipped_bytes_total: i64,
    pub skipped_since_last: i64,
    pub resyncs_total: i64,
    pub discarded_passes: i64,
    pub gap_bytes_total: i64,
    pub replay_max: i64,
    pub engine_ok: bool,
    pub engine_state: String,
    pub engine_requests_total: i64,
    pub engine_errors: i64,
    pub session_active: bool,
    pub unattributed_requests: i64,
    pub trace_offset: i64,
    pub trace_anchor: i64,
    pub held_records: i64,
    pub file_size: i64,
    pub db_ok: bool,
    pub db_enabled: bool,
    pub db_wal_bytes: i64,
    pub db_max_seq: i64,
    pub db_last_commit_ms: f64,
    pub db_error: String,
    pub subs: Vec<SubRow>,
    // our own subscriber's row in per_subscriber[], matched at connect time
    pub sub_id: Option<i64>,
    pub sub_depth: i64,
    pub sub_max_depth: i64,
    pub sub_sent: i64,
    pub sub_dropped: i64,
    pub sub_forced: i64,
    pub sub_replayed: i64,
    pub sub_replay_truncated: i64,
    pub sub_bytes_out: i64,
}

/// Where the compositor actually put the window (read back from hyprctl, never
/// assumed). Filled in once the WM has mapped and fullscreened the surface.
#[derive(Clone, Default)]
pub struct Placement {
    pub requested: String,   // --monitor value, or "active" when not given
    pub monitor: String,     // monitor name hyprctl reports for this window
    pub monitor_id: i64,
    pub x: i64,
    pub y: i64,
    pub w: i64,
    pub h: i64,
    pub fullscreen: i64,
    pub verified: bool,
    pub note: String,
}

pub struct State {
    pub pid: u32,
    pub socket: String,
    pub started_wall: f64,

    // connection
    pub connected: bool,
    pub connects: u64,
    pub disconnects: u64,
    pub connect_errors: u64,
    pub connect_error_wall: f64,
    pub connected_since: f64,
    pub connect_wall: f64, // wall clock of the connect currently in effect
    pub last_disconnect_reason: String,
    pub reconnects_with_replay: u64,

    // meta
    pub model: Option<String>,
    pub collector_version: Option<String>,
    pub schema: Option<i64>,
    pub run_id: Option<String>,
    pub layers: usize,
    pub experts: usize,
    pub k: i64,
    pub engine_name: Option<String>, // meta.engine.engine (version) — never the API key
    /// meta.engine rendered back to JSON (already parsed, no key in it: the
    /// collector's meta whitelists engine fields and the key is not one of them)
    pub engine_block: Option<String>,
    /// Where the model geometry came from: "socket" when the collector's own
    /// `meta` event arrived live, "collector-store" when it was read back from
    /// the collector's recorded meta row, None if the app has no meta at all —
    /// in which case the UI says unavailable.
    pub meta_source: Option<String>,
    pub meta_events: u64,

    // stream accounting
    pub events_total: u64,
    pub by_type: HashMap<String, u64>,
    pub events_ignored_dup: u64,
    pub last_seq: i64,
    pub last_ts: f64,
    pub seq_gaps: u64,
    pub seq_missing: u64,
    pub seq_max_seen: i64,
    pub seq_rebaselines: u64,
    pub last_event_ts: f64,

    // routing
    pub passes: u64,
    pub positions: u64,
    pub records: u64,
    pub sessions_started: u64,
    pub sessions_finished: u64,
    pub sessions: Vec<Session>,
    pub current_session: Option<String>,

    // reliability as reported by the collector
    pub gaps_seen: u64,
    pub gap_reasons: HashMap<String, u64>,
    pub gap_bytes: u64,
    pub gaps: Vec<GapRow>,
    pub skipped_bytes_seen: u64,
    pub replay_seen: u64,
    pub replay_truncated_seen: u64,

    // latest samples
    pub perf: Option<Perf>,
    pub perf_hist: Vec<(f64, Option<f64>)>, // (ts, live.tok_s) — real samples only
    pub perf_samples: u64,
    pub hw: Option<Hw>,
    pub status: Option<Status>,

    // per-(layer,expert) activation counters, cumulative since app start
    pub layer_counts: Vec<u64>,
    pub expert_counts: Vec<u64>,

    // ---- Stage 6: per-cell history (real event timestamps), co-activation pairs
    //
    // `cell_counts` / `cell_*_ts` are allocated on the first routing record: the
    // trails view is a view over real expert_selected events (count + the wall
    // timestamp the collector stamped on the event), never a decayed decoration
    // standing in for data.
    cell_counts: Vec<u32>,
    cell_first_ts: Vec<f64>,
    cell_last_ts: Vec<f64>,
    /// co-activation counts, expert-major: pairs[a * EXPERT_SLOTS + b], a < b,
    /// incremented once per top-k SET (one expert_selected event carries the
    /// whole set for that layer/position, so the pair is a real co-selection).
    pairs: Vec<u32>,
    pub pairs_records: u64,
    pub bursts_total: u64,
    pub pulses_total: u64,

    // ---- Stage 6: theme / interaction settings and their state
    pub zoom: f32,
    pub pan_x: f32,
    pub pan_y: f32,
    pub trails: bool,
    pub motion_reduced: bool,
    pub rain_density: f32,
    pub rain_speed: f32,
    pub glow: f32,
    pub banner: Banner,
    /// lattice rect as actually laid out (view, zoom, pan) — written by the
    /// event loop right after relayout so the dump reports geometry the renderer
    /// really used, not a request.
    pub view_rect: (f32, f32, f32, f32),
    pub view_cell: f32,
    pub view_cell_h: f32,
    pub visible_cells: usize,
    /// Input-injection self-test hook (acceptance harness only). Labelled as an
    /// input self-test everywhere it appears: it drives UI state, it never
    /// fabricates telemetry.
    pub selftest_enabled: bool,
    pub selftest_source: String,
    pub selftest_commands: u64,
    pub selftest_errors: u64,
    pub selftest_last: String,
    pub selftest_last_ts: f64,

    // view / selection (Stage 5 panels)
    pub view: u8,
    pub sel_layer: usize,
    pub sel_pinned: bool,
    pub last_active_layer: Option<usize>,

    // window placement read back from the compositor
    pub placement: Placement,

    // render facts (proves present-on-demand behaviour)
    pub frames: u64,
    pub redraw_requests: u64,
    pub repaints_unchanged: u64,
    pub wakes_skipped: u64,
    // why each presented frame happened (change / animation / resize / none)
    pub requested_frames: u64,
    pub pending_redraw: bool,
    pub frames_change: u64,
    pub frames_anim: u64,
    pub frames_resize: u64,
    pub frames_spurious: u64,
    pub last_frame: crate::render::FrameStats,
    pub rain: bool,
    // ---- Stage 7: residency estimate + frame-time distribution
    pub residency: Option<Residency>,
    pub res_epoch: u64,
    pub residency_events: u64,
    pub frame_hist: [u64; FRAME_BUCKETS],
    pub frame_samples: u64,
    pub frame_max_ms: f32,
}

/// Frame intervals are bucketed in 5 ms steps 0..160 ms plus one overflow
/// bucket. Cumulative counts let an external sampler diff two dumps and get the
/// exact p95 of that interval — the soak's drift metric — without the app
/// keeping any per-frame history.
pub const FRAME_BUCKETS: usize = 33;

/// Stage 7 residency: an ESTIMATE derived from model structure (the STRP
/// expert profile's ranked prefix + the engine's own expert_slots), never a
/// live per-expert truth. The engine emits no residency events and its cache
/// never evicts; the collector labels every field `estimate` and the app keeps
/// that label in the dump and on the lattice.
pub struct Residency {
    pub status: String,
    pub basis: String,
    pub source: String,
    pub confirmed: bool,
    pub policy: String,
    pub slots: u64,
    pub vram: u64,
    pub ram: u64,
    pub ssd: u64,
    pub layers: usize,
    pub experts: usize,
    pub cells: Vec<bool>,
    pub reason: String,
}

impl State {
    pub fn new(pid: u32, socket: &str, rain: bool) -> State {
        State {
            pid,
            socket: socket.to_string(),
            started_wall: crate::now_unix(),
            connected: false,
            connects: 0,
            disconnects: 0,
            connect_errors: 0,
            connect_error_wall: 0.0,
            connected_since: 0.0,
            connect_wall: 0.0,
            last_disconnect_reason: String::new(),
            reconnects_with_replay: 0,
            model: None,
            collector_version: None,
            schema: None,
            run_id: None,
            layers: LAYER_SLOTS,
            experts: EXPERT_SLOTS,
            k: 0,
            engine_name: None,
            engine_block: None,
            meta_source: None,
            meta_events: 0,
            events_total: 0,
            by_type: HashMap::new(),
            events_ignored_dup: 0,
            last_seq: -1,
            last_ts: 0.0,
            seq_gaps: 0,
            seq_missing: 0,
            seq_max_seen: -1,
            seq_rebaselines: 0,
            last_event_ts: 0.0,
            passes: 0,
            positions: 0,
            records: 0,
            sessions_started: 0,
            sessions_finished: 0,
            sessions: Vec::new(),
            current_session: None,
            gaps_seen: 0,
            gap_reasons: HashMap::new(),
            gap_bytes: 0,
            gaps: Vec::new(),
            skipped_bytes_seen: 0,
            replay_seen: 0,
            replay_truncated_seen: 0,
            perf: None,
            perf_hist: Vec::new(),
            perf_samples: 0,
            hw: None,
            status: None,
            layer_counts: vec![0; LAYER_SLOTS],
            expert_counts: vec![0; EXPERT_SLOTS],
            cell_counts: Vec::new(),
            cell_first_ts: Vec::new(),
            cell_last_ts: Vec::new(),
            pairs: Vec::new(),
            pairs_records: 0,
            bursts_total: 0,
            pulses_total: 0,
            zoom: 1.0,
            pan_x: 0.0,
            pan_y: 0.0,
            trails: true,
            motion_reduced: false,
            rain_density: 0.55,
            rain_speed: 1.0,
            glow: 0.6,
            banner: Banner::new(true, 26.0),
            view_rect: (0.0, 0.0, 0.0, 0.0),
            view_cell: 0.0,
            view_cell_h: 0.0,
            visible_cells: 0,
            selftest_enabled: false,
            selftest_source: String::new(),
            selftest_commands: 0,
            selftest_errors: 0,
            selftest_last: String::new(),
            selftest_last_ts: 0.0,
            view: 0,
            sel_layer: 0,
            sel_pinned: false,
            last_active_layer: None,
            placement: Placement::default(),
            frames: 0,
            redraw_requests: 0,
            repaints_unchanged: 0,
            wakes_skipped: 0,
            requested_frames: 0,
            pending_redraw: false,
            frames_change: 0,
            frames_anim: 0,
            frames_resize: 0,
            frames_spurious: 0,
            last_frame: Default::default(),
            rain,
            residency: None,
            res_epoch: 0,
            residency_events: 0,
            frame_hist: [0; FRAME_BUCKETS],
            frame_samples: 0,
            frame_max_ms: 0.0,
        }
    }

    /// Record one presented frame's interval (ms). Intervals above 5 s are the
    /// window's first frame after a long idle period and are not animation cost:
    /// they are counted separately instead of poisoning p95.
    pub fn record_frame_ms(&mut self, ms: f32) {
        if !ms.is_finite() || ms < 0.0 {
            return;
        }
        if ms > 5000.0 {
            return;
        }
        let b = ((ms / 5.0).floor() as usize).min(FRAME_BUCKETS - 1);
        self.frame_hist[b] += 1;
        self.frame_samples += 1;
        if ms > self.frame_max_ms {
            self.frame_max_ms = ms;
        }
    }

    /// Percentile (0..1) of the cumulative frame-interval histogram, in ms.
    pub fn frame_pct(&self, q: f32) -> f32 {
        if self.frame_samples == 0 {
            return 0.0;
        }
        let target = (self.frame_samples as f64 * q.clamp(0.0, 1.0) as f64).ceil() as u64;
        let mut acc = 0u64;
        for (i, n) in self.frame_hist.iter().enumerate() {
            acc += *n;
            if acc >= target.max(1) {
                // report the top edge of the bucket: conservative, never optimistic
                return ((i as f32 + 1.0) * 5.0).min(165.0);
            }
        }
        165.0
    }

    pub fn session(&self, id: &str) -> Option<&Session> {
        self.sessions.iter().find(|s| s.id == id)
    }

    fn session_mut(&mut self, id: &str) -> Option<&mut Session> {
        self.sessions.iter_mut().find(|s| s.id == id)
    }

    /// A client that attaches mid-session never sees `session_started` (the
    /// collector's replay window starts at the session tail), so the session
    /// entry is created lazily from the first event carrying that id. Counters
    /// stay per-session instead of being dropped on the floor.
    fn ensure_session(&mut self, id: &str, ts: f64) {
        if self.sessions.iter().any(|s| s.id == id) {
            return;
        }
        if self.sessions.len() >= MAX_SESSIONS {
            self.sessions.remove(0);
        }
        let mut s = Session::new(id, ts);
        s.tier = "unknown".into();
        self.sessions.push(s);
        if self.current_session.is_none() {
            self.current_session = Some(id.to_string());
        }
    }

    /// Every displayed value, hashed (sample timestamps are NOT hashed: a
    /// sample that repeats the same values must not cause a frame). The event
    /// loop repaints only when this
    /// changes (or while a lamp is still fading or the rain is running), which
    /// is what makes an idle, rain-off window present zero frames: a retry that
    /// does not change a single visible value produces no frame at all.
    pub fn fingerprint(&self) -> u64 {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut h = DefaultHasher::new();
        self.view.hash(&mut h);
        self.sel_layer.hash(&mut h);
        self.rain.hash(&mut h);
        self.connected.hash(&mut h);
        self.connect_errors.hash(&mut h);
        self.meta_source.hash(&mut h);
        self.meta_events.hash(&mut h);
        self.model.hash(&mut h);
        self.engine_name.hash(&mut h);
        self.layers.hash(&mut h);
        self.experts.hash(&mut h);
        self.k.hash(&mut h);
        self.collector_version.hash(&mut h);
        self.run_id.hash(&mut h);
        self.last_seq.hash(&mut h);
        self.last_event_ts.to_bits().hash(&mut h);
        self.passes.hash(&mut h);
        self.positions.hash(&mut h);
        self.records.hash(&mut h);
        self.sessions_started.hash(&mut h);
        self.sessions_finished.hash(&mut h);
        self.current_session.hash(&mut h);
        self.seq_gaps.hash(&mut h);
        self.seq_missing.hash(&mut h);
        self.seq_rebaselines.hash(&mut h);
        self.gaps_seen.hash(&mut h);
        self.gap_bytes.hash(&mut h);
        self.skipped_bytes_seen.hash(&mut h);
        self.replay_seen.hash(&mut h);
        self.replay_truncated_seen.hash(&mut h);
        self.placement.monitor.hash(&mut h);
        self.placement.verified.hash(&mut h);
        self.placement.w.hash(&mut h);
        self.placement.h.hash(&mut h);
        self.layer_counts.hash(&mut h);
        self.expert_counts.hash(&mut h);
        self.zoom.to_bits().hash(&mut h);
        self.pan_x.to_bits().hash(&mut h);
        self.pan_y.to_bits().hash(&mut h);
        self.trails.hash(&mut h);
        self.motion_reduced.hash(&mut h);
        self.rain_density.to_bits().hash(&mut h);
        self.rain_speed.to_bits().hash(&mut h);
        self.banner.enabled.hash(&mut h);
        self.sessions.len().hash(&mut h);
        for s in self.sessions.iter().rev().take(24) {
            s.id.hash(&mut h);
            s.tier.hash(&mut h);
            s.finished.hash(&mut h);
            s.end_reason.hash(&mut h);
            s.request_delta.hash(&mut h);
            s.requests_before.hash(&mut h);
            s.requests_after.hash(&mut h);
            s.seen_passes.hash(&mut h);
            s.seen_positions.hash(&mut h);
            s.seen_records.hash(&mut h);
            s.passes.hash(&mut h);
            s.positions.hash(&mut h);
            s.records.hash(&mut h);
            s.duration_s.to_bits().hash(&mut h);
        }
        if let Some(p) = &self.perf {
            p.engine_ok.hash(&mut h);
            p.error.hash(&mut h);
            p.baseline.hash(&mut h);
            p.state.hash(&mut h);
            p.tok_s.map(|v| v.to_bits()).hash(&mut h);
            p.requests.hash(&mut h);
            p.prompt_tokens.hash(&mut h);
            p.output_tokens.hash(&mut h);
            p.drafts_offered.hash(&mut h);
            p.drafts_accepted.hash(&mut h);
            p.delta_requests.hash(&mut h);
            p.delta_output.hash(&mut h);
        } else {
            0u8.hash(&mut h);
        }
        if let Some(x) = &self.hw {
            x.gpu_name.hash(&mut h);
            x.util_pct.map(|v| v.to_bits()).hash(&mut h);
            x.mem_used_mib.map(|v| v.to_bits()).hash(&mut h);
            x.temp_c.map(|v| v.to_bits()).hash(&mut h);
            x.power_w.map(|v| v.to_bits()).hash(&mut h);
            x.load1.map(|v| v.to_bits()).hash(&mut h);
            x.mem_available_mib.map(|v| v.to_bits()).hash(&mut h);
        } else {
            1u8.hash(&mut h);
        }
        if let Some(s) = &self.status {
            s.events_per_s.to_bits().hash(&mut h);
            s.records_per_s.to_bits().hash(&mut h);
            s.bytes_per_s.to_bits().hash(&mut h);
            s.events_total.hash(&mut h);
            s.sessions_total.hash(&mut h);
            s.subscribers.hash(&mut h);
            s.queue_cap.hash(&mut h);
            s.drops_total.hash(&mut h);
            s.skipped_bytes_total.hash(&mut h);
            s.resyncs_total.hash(&mut h);
            s.discarded_passes.hash(&mut h);
            s.gap_bytes_total.hash(&mut h);
            s.engine_ok.hash(&mut h);
            s.engine_state.hash(&mut h);
            s.engine_requests_total.hash(&mut h);
            s.engine_errors.hash(&mut h);
            s.session_active.hash(&mut h);
            s.unattributed_requests.hash(&mut h);
            s.db_ok.hash(&mut h);
            s.db_wal_bytes.hash(&mut h);
            s.db_max_seq.hash(&mut h);
            s.sub_depth.hash(&mut h);
            s.sub_max_depth.hash(&mut h);
            s.sub_sent.hash(&mut h);
            s.sub_dropped.hash(&mut h);
            s.sub_forced.hash(&mut h);
            s.sub_replayed.hash(&mut h);
            s.sub_replay_truncated.hash(&mut h);
            for r in &s.subs {
                r.id.hash(&mut h);
                r.sent.hash(&mut h);
                r.dropped.hash(&mut h);
                r.forced.hash(&mut h);
                r.depth.hash(&mut h);
                r.closed.hash(&mut h);
            }
        } else {
            2u8.hash(&mut h);
        }
        for g in &self.gaps {
            g.reason.hash(&mut h);
            g.bytes.hash(&mut h);
        }
        h.finish()
    }

    /// Per-group hashes of exactly the values `fingerprint()` covers, so a frame
    /// drawn without any new message can be attributed to the group that moved.
    pub fn fingerprint_groups(&self) -> Vec<(&'static str, u64)> {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let h = |f: &dyn Fn(&mut DefaultHasher)| {
            let mut d = DefaultHasher::new();
            f(&mut d);
            d.finish()
        };
        vec![
            ("conn", h(&|d| { self.connected.hash(d); self.connect_errors.hash(d); self.connects.hash(d); self.disconnects.hash(d); self.last_disconnect_reason.hash(d); })),
            ("meta", h(&|d| { self.meta_source.hash(d); self.meta_events.hash(d); self.model.hash(d); self.engine_name.hash(d); self.layers.hash(d); self.experts.hash(d); self.k.hash(d); self.run_id.hash(d); self.collector_version.hash(d); self.res_epoch.hash(d); self.residency_events.hash(d); })),
            ("stream", h(&|d| { self.last_seq.hash(d); self.events_total.hash(d); self.seq_gaps.hash(d); self.seq_missing.hash(d); self.last_event_ts.to_bits().hash(d); })),
            ("routing", h(&|d| { self.passes.hash(d); self.positions.hash(d); self.records.hash(d); self.current_session.hash(d); })),
            ("sessions", h(&|d| { self.sessions_started.hash(d); self.sessions_finished.hash(d); self.sessions.len().hash(d); for s in self.sessions.iter() { s.id.hash(d); s.tier.hash(d); s.finished.hash(d); s.seen_records.hash(d); s.seen_passes.hash(d); s.request_delta.hash(d); } })),
            ("gaps", h(&|d| { self.gaps_seen.hash(d); self.gap_bytes.hash(d); self.skipped_bytes_seen.hash(d); self.replay_seen.hash(d); self.replay_truncated_seen.hash(d); self.gaps.len().hash(d); })),
            ("perf", h(&|d| { self.perf.as_ref().map(|p| (p.state.clone(), p.tok_s.map(|v| v.to_bits()), p.requests, p.output_tokens, p.delta_requests)).hash(d); self.perf_samples.hash(d); })),
            ("hw", h(&|d| { self.hw.as_ref().map(|x| (x.util_pct.map(|v| v.to_bits()), x.mem_used_mib.map(|v| v.to_bits()), x.load1.map(|v| v.to_bits()), x.temp_c.map(|v| v.to_bits()))).hash(d); })),
            ("status", h(&|d| { self.status.as_ref().map(|x| (x.events_per_s.to_bits(), x.records_per_s.to_bits(), x.drops_total, x.sub_sent, x.sub_depth, x.unattributed_requests, x.engine_requests_total)).hash(d); })),
            ("counts", h(&|d| { self.layer_counts.hash(d); self.expert_counts.hash(d); })),
            ("ui", h(&|d| { self.view.hash(d); self.sel_layer.hash(d); self.rain.hash(d); self.placement.monitor.hash(d); self.placement.verified.hash(d); self.placement.w.hash(d); self.placement.h.hash(d); self.zoom.to_bits().hash(d); self.pan_x.to_bits().hash(d); self.pan_y.to_bits().hash(d); self.trails.hash(d); })),
        ]
    }

    /// Clamped zoom setter; the viewport is bounded so the lattice can never be
    /// scrolled into an empty region by an out-of-range self-test command.
    pub fn set_zoom(&mut self, z: f32) {
        self.zoom = z.clamp(MIN_ZOOM, MAX_ZOOM);
    }

    pub fn zoom_by(&mut self, f: f32) {
        self.set_zoom(self.zoom * f);
    }

    pub fn pan_by(&mut self, dx: f32, dy: f32) {
        self.pan_x = (self.pan_x + dx).clamp(-8000.0, 8000.0);
        self.pan_y = (self.pan_y + dy).clamp(-8000.0, 8000.0);
    }

    pub fn reset_view(&mut self) {
        self.zoom = 1.0;
        self.pan_x = 0.0;
        self.pan_y = 0.0;
    }

    /// (selection count, last event timestamp) for one lattice cell — 0 / 0.0
    /// when the app has not seen that cell activated.
    pub fn cell(&self, layer: usize, expert: usize) -> (u32, f64) {
        let idx = layer * EXPERT_SLOTS + expert;
        (
            self.cell_counts.get(idx).copied().unwrap_or(0),
            self.cell_last_ts.get(idx).copied().unwrap_or(0.0),
        )
    }

    pub fn cells_with_history(&self) -> usize {
        self.cell_counts.iter().filter(|c| **c > 0).count()
    }

    pub fn history_totals(&self) -> (u64, f64, f64) {
        let total: u64 = self.cell_counts.iter().map(|c| *c as u64).sum();
        let mut first = 0.0f64;
        let mut last = 0.0f64;
        for (i, c) in self.cell_counts.iter().enumerate() {
            if *c == 0 {
                continue;
            }
            let f = self.cell_first_ts.get(i).copied().unwrap_or(0.0);
            let l = self.cell_last_ts.get(i).copied().unwrap_or(0.0);
            if f > 0.0 && (first == 0.0 || f < first) {
                first = f;
            }
            if l > last {
                last = l;
            }
        }
        (total, first, last)
    }

    /// Co-selection count for one expert pair (order-independent).
    pub fn pair(&self, a: usize, b: usize) -> u32 {
        let (lo, hi) = if a < b { (a, b) } else { (b, a) };
        self.pairs.get(lo * EXPERT_SLOTS + hi).copied().unwrap_or(0)
    }

    pub fn pairs_distinct(&self) -> usize {
        self.pairs.iter().filter(|c| **c > 0).count()
    }

    pub fn pairs_total(&self) -> u64 {
        self.pairs.iter().map(|c| *c as u64).sum()
    }

    /// The heaviest co-selected expert pairs (a < b), highest first.
    pub fn top_pairs(&self, n: usize) -> Vec<(u16, u16, u32)> {
        let mut best: Vec<(u16, u16, u32)> = Vec::with_capacity(n + 1);
        for a in 0..EXPERT_SLOTS {
            let base = a * EXPERT_SLOTS;
            for b in (a + 1)..EXPERT_SLOTS {
                let c = self.pairs.get(base + b).copied().unwrap_or(0);
                if c == 0 {
                    continue;
                }
                if best.len() == n && c <= best[n - 1].2 {
                    continue;
                }
                let pos = best.iter().position(|e| c > e.2).unwrap_or(best.len());
                best.insert(pos, (a as u16, b as u16, c));
                best.truncate(n);
            }
        }
        best
    }

    pub fn hot_experts(&self, n: usize) -> Vec<(usize, u64)> {
        let mut hot: Vec<(usize, u64)> = self.expert_counts.iter().copied().enumerate().collect();
        hot.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        hot.truncate(n);
        hot
    }

    /// Apply one NDJSON line. Returns the activation to hand to the renderer
    /// (pass_index, layer, expert ids) when the line was an expert_selected
    /// event.
    pub fn apply(&mut self, line: &str) -> Option<(u32, u16, Vec<u16>)> {
        let v = match json::parse(line.as_bytes()) {
            Ok(v) => v,
            Err(_) => {
                // never silently: a line the app cannot parse is counted
                *self.by_type.entry("__parse_error".into()).or_insert(0) += 1;
                return None;
            }
        };
        let typ = v.get("type").and_then(|t| t.as_str()).unwrap_or("").to_string();
        if typ.is_empty() {
            *self.by_type.entry("__no_type".into()).or_insert(0) += 1;
            return None;
        }
        let seq = v.get("seq").and_then(|s| s.as_i64()).unwrap_or(-1);
        let ts = v.get("ts").and_then(|s| s.as_f64()).unwrap_or(0.0);

        // ---- seq discipline: contiguous, monotonic, never silently lost
        if typ == "meta" {
            // A new collector run may restart seq at 0 (fresh DB). Detect it by
            // run_id and re-baseline rather than mistaking the new stream for
            // duplicates of the old one.
            let rid = v.path(&["collector", "run_id"]).and_then(|s| s.as_str()).map(|s| s.to_string());
            if let (Some(old), Some(new)) = (self.run_id.as_ref(), rid.as_ref()) {
                if old != new {
                    self.last_seq = -1;
                    self.seq_max_seen = -1;
                }
            }
            if let Some(r) = rid {
                self.run_id = Some(r);
            }
            self.collector_version = v
                .path(&["collector", "version"])
                .and_then(|s| s.as_str())
                .map(|s| s.to_string());
            self.schema = v.path(&["collector", "schema"]).and_then(|s| s.as_i64());
            if let Some(m) = v.path(&["engine", "model"]).and_then(|s| s.as_str()) {
                self.model = Some(m.to_string());
                self.meta_source = Some("socket".into());
            }
            if let Some(e) = v.path(&["engine", "engine"]).and_then(|s| s.as_str()) {
                self.engine_name = Some(e.to_string());
            }
            if let Some(e) = v.get("engine") {
                self.engine_block = Some(json::render(e));
            }
            if let Some(l) = v.path(&["trace", "layers"]).and_then(|s| s.as_i64()) {
                if l > 0 && (l as usize) <= LAYER_SLOTS {
                    self.layers = l as usize;
                }
            }
            if let Some(kk) = v.path(&["trace", "k"]).and_then(|s| s.as_i64()) {
                self.k = kk;
            }
            if let Some(hi) = v.path(&["trace", "expert_id_range"]).and_then(|a| match a {
                Value::Arr(x) => x.get(1).and_then(|z| z.as_i64()),
                _ => None,
            }) {
                if hi >= 0 && (hi as usize) < EXPERT_SLOTS {
                    self.experts = hi as usize + 1;
                }
            }
            self.meta_events += 1;
        }
        if seq >= 0 {
            // `meta` is re-sent to every subscriber that attaches (additive
            // meta-on-connect), always carrying the seq it was recorded with, so
            // it is exempt from the duplicate/ordering filter: it is idempotent
            // geometry, not a data event. It never moves `last_seq`.
            // `gap` is a statement about the stream, not a data event: it is
            // replayed to late subscribers alongside meta, carries its own
            // byte/record accounting, and never moves `last_seq`.
            // residency_estimate is emitted once by the collector right after
            // meta and replayed alongside it: idempotent structure, not a data
            // event, so it never moves `last_seq` (a compaction/restart must not
            // look like a seq hole).
            let is_meta = typ == "meta" || typ == "gap" || typ == "residency_estimate";
            if !is_meta && self.last_seq >= 0 && seq <= self.last_seq {
                if ts > self.last_ts + 0.001 {
                    // A collector that restarted against a fresh database
                    // re-bases seq at 1. Fresh timestamps prove these are new
                    // events, not replayed duplicates: re-baseline instead of
                    // throwing away live data.
                    self.seq_rebaselines += 1;
                    self.last_seq = seq - 1;
                    self.seq_max_seen = seq - 1;
                } else {
                    // replayed event already accounted for
                    self.events_ignored_dup += 1;
                    *self.by_type.entry(format!("{typ}__dup")).or_insert(0) += 1;
                    return None;
                }
            }
            if !is_meta {
                if self.last_seq >= 0 && seq != self.last_seq + 1 {
                    self.seq_gaps += 1;
                    self.seq_missing += (seq - self.last_seq - 1).max(0) as u64;
                }
                self.last_seq = seq;
                if seq > self.seq_max_seen {
                    self.seq_max_seen = seq;
                }
                self.last_ts = ts;
                self.last_event_ts = ts;
            } else if seq > self.seq_max_seen {
                self.seq_max_seen = seq;
            }
        }

        self.events_total += 1;
        *self.by_type.entry(typ.clone()).or_insert(0) += 1;

        let sess = v.get("session").and_then(|s| s.as_str()).map(|s| s.to_string());
        match typ.as_str() {
            "session_started" => {
                self.sessions_started += 1;
                if let Some(id) = sess.clone() {
                    if self.sessions.len() >= MAX_SESSIONS {
                        self.sessions.remove(0);
                    }
                    let mut s = Session::new(&id, ts);
                    s.requests_before = v.get("requests_before").and_then(|z| z.as_i64()).unwrap_or(-1);
                    s.first_seq = seq;
                    s.tier = v
                        .get("tier_hint")
                        .and_then(|z| z.as_str())
                        .unwrap_or("unknown")
                        .to_string();
                    self.sessions.push(s);
                    self.current_session = Some(id);
                }
            }
            "session_finished" => {
                self.sessions_finished += 1;
                if let Some(id) = sess.clone() {
                    let (tier, reason, rb, ra, delta, passes, positions, records, dur, bs, be, fseq) = (
                        v.get("tier").and_then(|z| z.as_str()).unwrap_or("unknown").to_string(),
                        v.get("end_reason").and_then(|z| z.as_str()).unwrap_or("").to_string(),
                        v.get("requests_before").and_then(|z| z.as_i64()).unwrap_or(-1),
                        v.get("requests_after").and_then(|z| z.as_i64()).unwrap_or(-1),
                        v.get("request_delta").and_then(|z| z.as_i64()).unwrap_or(-1),
                        v.get("passes").and_then(|z| z.as_i64()).unwrap_or(-1),
                        v.get("positions").and_then(|z| z.as_i64()).unwrap_or(-1),
                        v.get("records").and_then(|z| z.as_i64()).unwrap_or(-1),
                        v.get("duration_s").and_then(|z| z.as_f64()).unwrap_or(-1.0),
                        v.get("trace_byte_start").and_then(|z| z.as_i64()).unwrap_or(-1),
                        v.get("trace_byte_end").and_then(|z| z.as_i64()).unwrap_or(-1),
                        v.get("first_seq").and_then(|z| z.as_i64()).unwrap_or(-1),
                    );
                    if self.session(&id).is_none() {
                        if self.sessions.len() >= MAX_SESSIONS {
                            self.sessions.remove(0);
                        }
                        self.sessions.push(Session::new(&id, ts));
                    }
                    if let Some(s) = self.session_mut(&id) {
                        s.finished = true;
                        s.tier = tier;
                        s.end_reason = reason;
                        s.requests_before = rb;
                        s.requests_after = ra;
                        s.request_delta = delta;
                        s.passes = passes;
                        s.positions = positions;
                        s.records = records;
                        s.duration_s = dur;
                        s.trace_bytes = if bs >= 0 && be >= bs { be - bs } else { -1 };
                        s.first_seq = fseq;
                    }
                    if self.current_session.as_deref() == Some(id.as_str()) {
                        self.current_session = None;
                    }
                }
            }
            "pass_routed" => {
                let pc = v.get("positions_count").and_then(|z| z.as_i64()).unwrap_or(0).max(0) as u64;
                self.passes += 1;
                self.positions += pc;
                let sk = v.get("skipped_bytes_since_last").and_then(|z| z.as_i64()).unwrap_or(0);
                if sk > 0 {
                    self.skipped_bytes_seen += sk as u64;
                }
                if let Some(id) = sess.as_ref() {
                    self.ensure_session(id, ts);
                    if let Some(s) = self.session_mut(id) {
                        s.seen_passes += 1;
                        s.seen_positions += pc;
                    }
                }
            }
            "expert_selected" => {
                self.records += 1;
                let layer = v.get("layer").and_then(|z| z.as_i64()).unwrap_or(-1);
                let mut ids: Vec<u16> = Vec::new();
                if let Some(Value::Arr(a)) = v.get("expert_ids") {
                    for id in a {
                        if let Some(x) = id.as_i64() {
                            if x >= 0 && (x as usize) < self.experts {
                                let ux = x as u16;
                                ids.push(ux);
                                self.expert_counts[x as usize] += 1;
                            }
                        }
                    }
                }
                if layer >= 0 && (layer as usize) < self.layers {
                    self.layer_counts[layer as usize] += 1;
                    self.last_active_layer = Some(layer as usize);
                    if !self.sel_pinned {
                        self.sel_layer = layer as usize;
                    }
                    // ---- Stage 6: per-cell history, from the event's own ts
                    if ids.is_empty() {
                        // nothing selected in the record: no history to add
                    } else {
                        let n = LAYER_SLOTS * EXPERT_SLOTS;
                        if self.cell_counts.len() != n {
                            self.cell_counts = vec![0; n];
                            self.cell_first_ts = vec![0.0; n];
                            self.cell_last_ts = vec![0.0; n];
                        }
                        let base = layer as usize * EXPERT_SLOTS;
                        for x in ids.iter() {
                            let idx = base + *x as usize;
                            self.cell_counts[idx] = self.cell_counts[idx].saturating_add(1);
                            if self.cell_first_ts[idx] <= 0.0 {
                                self.cell_first_ts[idx] = ts;
                            }
                            self.cell_last_ts[idx] = ts;
                        }
                        // ---- co-activation: every pair inside this record's
                        // top-k set. One event = one routing decision for one
                        // layer and position, so the pair co-occurred for real.
                        if ids.len() > 1 {
                            if self.pairs.len() != EXPERT_SLOTS * EXPERT_SLOTS {
                                self.pairs = vec![0u32; EXPERT_SLOTS * EXPERT_SLOTS];
                            }
                            for i in 0..ids.len() {
                                for j in (i + 1)..ids.len() {
                                    let (a, b) = (ids[i] as usize, ids[j] as usize);
                                    let (lo, hi) = if a < b { (a, b) } else { (b, a) };
                                    let k = lo * EXPERT_SLOTS + hi;
                                    self.pairs[k] = self.pairs[k].saturating_add(1);
                                }
                            }
                            self.pairs_records += 1;
                        }
                        // activation-linked effects: one burst + one pulse per
                        // real record (counted here, drawn by the renderer)
                        self.bursts_total += 1;
                        self.pulses_total += 1;
                    }
                }
                if let Some(id) = sess.as_ref() {
                    self.ensure_session(id, ts);
                    if let Some(s) = self.session_mut(id) {
                        s.seen_records += 1;
                    }
                }
                if layer >= 0 && (layer as usize) < LAYER_SLOTS && !ids.is_empty() {
                    let pass = v.get("pass_index").and_then(|z| z.as_i64()).unwrap_or(0).max(0) as u32;
                    return Some((pass, layer as u16, ids));
                }
            }
            "perf_sample" => {
                let tot = v.get("totals");
                let live = v.get("live");
                let del = v.get("delta");
                let p = Perf {
                    ts,
                    engine_ok: v.get("engine_ok").and_then(|z| match z {
                        Value::Bool(b) => Some(*b),
                        _ => None,
                    }).unwrap_or(false),
                    error: v.get("error").and_then(|z| z.as_str()).unwrap_or("").to_string(),
                    baseline: v.get("baseline").and_then(|z| match z {
                        Value::Bool(b) => Some(*b),
                        _ => None,
                    }).unwrap_or(false),
                    state: live.and_then(|l| l.get("state")).and_then(|z| z.as_str()).unwrap_or("").to_string(),
                    tok_s: live.and_then(|l| l.get("tok_s")).and_then(|z| z.as_f64()),
                    requests: tot.and_then(|t| t.get("requests")).and_then(|z| z.as_i64()),
                    prompt_tokens: tot.and_then(|t| t.get("prompt_tokens")).and_then(|z| z.as_i64()).unwrap_or(0),
                    output_tokens: tot.and_then(|t| t.get("output_tokens")).and_then(|z| z.as_i64()).unwrap_or(0),
                    drafts_offered: tot.and_then(|t| t.get("drafts_offered")).and_then(|z| z.as_i64()).unwrap_or(0),
                    drafts_accepted: tot.and_then(|t| t.get("drafts_accepted")).and_then(|z| z.as_i64()).unwrap_or(0),
                    delta_requests: del.and_then(|d| d.get("requests")).and_then(|z| z.as_i64()).unwrap_or(0),
                    delta_prompt: del.and_then(|d| d.get("prompt_tokens")).and_then(|z| z.as_i64()).unwrap_or(0),
                    delta_output: del.and_then(|d| d.get("output_tokens")).and_then(|z| z.as_i64()).unwrap_or(0),
                    delta_drafts: del.and_then(|d| d.get("drafts_offered")).and_then(|z| z.as_i64()).unwrap_or(0),
                };
                self.perf_samples += 1;
                self.perf_hist.push((ts, p.tok_s));
                if self.perf_hist.len() > PERF_HIST {
                    self.perf_hist.remove(0);
                }
                self.perf = Some(p);
            }
            "hw_sample" => {
                let mut h = Hw { ts, ..Default::default() };
                if let Some(g) = v.get("gpu") {
                    h.gpu_name = g.get("name").and_then(|z| z.as_str()).unwrap_or("").to_string();
                    h.util_pct = g.get("util_pct").and_then(|z| z.as_f64());
                    h.mem_used_mib = g.get("mem_used_mib").and_then(|z| z.as_f64());
                    h.temp_c = g.get("temp_c").and_then(|z| z.as_f64());
                    h.power_w = g.get("power_w").and_then(|z| z.as_f64());
                }
                h.load1 = v.path(&["cpu", "load1"]).and_then(|z| z.as_f64());
                h.mem_available_mib = v.path(&["mem", "available_mib"]).and_then(|z| z.as_f64());
                h.rss_kib = v.path(&["proc", "rss_kib"]).and_then(|z| z.as_i64());
                self.hw = Some(h);
            }
            "residency_estimate" => {
                self.residency_events += 1;
                let status = v.get("status").and_then(|z| z.as_str()).unwrap_or("unavailable").to_string();
                let gs = |k: &str| v.get(k).and_then(|z| z.as_str()).unwrap_or("").to_string();
                let gu = |k: &str| v.get(k).and_then(|z| z.as_i64()).unwrap_or(0).max(0) as u64;
                let tp = |k: &str| v.path(&["tiers", k]).and_then(|z| z.as_i64()).unwrap_or(0).max(0) as u64;
                let layers = v.path(&["profile", "layers"]).and_then(|z| z.as_i64())
                    .map(|x| x.max(0) as usize).filter(|x| *x > 0 && *x <= LAYER_SLOTS)
                    .unwrap_or(self.layers);
                let experts = v.path(&["profile", "experts"]).and_then(|z| z.as_i64())
                    .map(|x| x.max(0) as usize).filter(|x| *x > 0 && *x <= EXPERT_SLOTS)
                    .unwrap_or(self.experts);
                let mut cells = vec![false; layers.saturating_mul(experts)];
                let mut marked = 0usize;
                if status == "estimate" {
                    if let Some(Value::Arr(a)) = v.get("vram_cells") {
                        for id in a {
                            if let Some(x) = id.as_i64() {
                                if x >= 0 && (x as usize) < cells.len() {
                                    if !cells[x as usize] {
                                        marked += 1;
                                    }
                                    cells[x as usize] = true;
                                }
                            }
                        }
                    }
                }
                self.residency = Some(Residency {
                    status,
                    basis: gs("basis"),
                    source: gs("source"),
                    confirmed: v.get("confirmed").map(|z| matches!(z, Value::Bool(true))).unwrap_or(false),
                    policy: gs("policy"),
                    slots: gu("slots"),
                    vram: tp("vram"),
                    ram: tp("ram"),
                    ssd: tp("ssd"),
                    layers,
                    experts,
                    cells,
                    reason: gs("reason"),
                });
                self.res_epoch += 1;
                let _ = marked;
            }
            "telemetry_status" => {
                let mut s = Status {
                    ts,
                    uptime_s: v.get("uptime_s").and_then(|z| z.as_f64()).unwrap_or(0.0),
                    events_total: v.get("events_total").and_then(|z| z.as_i64()).unwrap_or(0),
                    sessions_total: v.get("sessions_total").and_then(|z| z.as_i64()).unwrap_or(0),
                    events_per_s: v.path(&["rates", "events_per_s"]).and_then(|z| z.as_f64()).unwrap_or(0.0),
                    records_per_s: v.path(&["rates", "records_per_s"]).and_then(|z| z.as_f64()).unwrap_or(0.0),
                    bytes_per_s: v.path(&["rates", "bytes_per_s"]).and_then(|z| z.as_f64()).unwrap_or(0.0),
                    subscribers: v.get("subscribers").and_then(|z| z.as_i64()).unwrap_or(0),
                    queue_cap: v.get("queue_cap").and_then(|z| z.as_i64()).unwrap_or(0),
                    drops_total: v.get("drops_total").and_then(|z| z.as_i64()).unwrap_or(0),
                    skipped_bytes_total: v.get("skipped_bytes_total").and_then(|z| z.as_i64()).unwrap_or(0),
                    skipped_since_last: v.get("skipped_bytes_since_last_event").and_then(|z| z.as_i64()).unwrap_or(0),
                    resyncs_total: v.get("resyncs_total").and_then(|z| z.as_i64()).unwrap_or(0),
                    discarded_passes: v.get("discarded_passes").and_then(|z| z.as_i64()).unwrap_or(0),
                    gap_bytes_total: v.get("gap_bytes_total").and_then(|z| z.as_i64()).unwrap_or(0),
                    replay_max: v.get("replay_max").and_then(|z| z.as_i64()).unwrap_or(0),
                    engine_ok: v.path(&["engine", "ok"]).and_then(|z| match z {
                        Value::Bool(b) => Some(*b),
                        _ => None,
                    }).unwrap_or(false),
                    engine_state: v.path(&["engine", "state"]).and_then(|z| z.as_str()).unwrap_or("").to_string(),
                    engine_requests_total: v.path(&["engine", "requests_total"]).and_then(|z| z.as_i64()).unwrap_or(0),
                    engine_errors: v.path(&["engine", "errors"]).and_then(|z| z.as_i64()).unwrap_or(0),
                    session_active: v.get("session_active").and_then(|z| match z {
                        Value::Bool(b) => Some(*b),
                        _ => None,
                    }).unwrap_or(false),
                    unattributed_requests: v.get("unattributed_requests").and_then(|z| z.as_i64()).unwrap_or(0),
                    trace_offset: v.path(&["trace", "offset"]).and_then(|z| z.as_i64()).unwrap_or(0),
                    trace_anchor: v.path(&["trace", "anchor"]).and_then(|z| z.as_i64()).unwrap_or(0),
                    held_records: v.path(&["trace", "held_records"]).and_then(|z| z.as_i64()).unwrap_or(0),
                    file_size: v.path(&["trace", "file_size"]).and_then(|z| z.as_i64()).unwrap_or(0),
                    db_ok: v.path(&["db", "ok"]).and_then(|z| match z {
                        Value::Bool(b) => Some(*b),
                        _ => None,
                    }).unwrap_or(false),
                    db_enabled: v.path(&["db", "enabled"]).and_then(|z| match z {
                        Value::Bool(b) => Some(*b),
                        _ => None,
                    }).unwrap_or(false),
                    db_wal_bytes: v.path(&["db", "wal_bytes"]).and_then(|z| z.as_i64()).unwrap_or(0),
                    db_max_seq: v.path(&["db", "max_seq"]).and_then(|z| z.as_i64()).unwrap_or(0),
                    db_last_commit_ms: v.path(&["db", "last_commit_ms"]).and_then(|z| z.as_f64()).unwrap_or(0.0),
                    db_error: v.path(&["db", "error"]).and_then(|z| z.as_str()).unwrap_or("").to_string(),
                    ..Default::default()
                };
                if let Some(Value::Arr(list)) = v.get("per_subscriber") {
                    for e in list {
                        s.subs.push(SubRow {
                            id: e.get("sub_id").and_then(|z| z.as_i64()).unwrap_or(-1),
                            name: e.get("id").and_then(|z| z.as_str()).unwrap_or("").to_string(),
                            peer: e.get("conn").and_then(|z| z.as_str()).unwrap_or("").to_string(),
                            since: e.get("since").and_then(|z| z.as_f64()).unwrap_or(0.0),
                            depth: e.get("depth").and_then(|z| z.as_i64()).unwrap_or(0),
                            max_depth: e.get("max_depth").and_then(|z| z.as_i64()).unwrap_or(0),
                            sent: e.get("sent").and_then(|z| z.as_i64()).unwrap_or(0),
                            dropped: e.get("dropped_droppable").and_then(|z| z.as_i64()).unwrap_or(0),
                            forced: e.get("forced_drops").and_then(|z| z.as_i64()).unwrap_or(0),
                            replayed: e.get("replayed").and_then(|z| z.as_i64()).unwrap_or(0),
                            truncated: e.get("replay_truncated").and_then(|z| z.as_i64()).unwrap_or(0),
                            bytes_out: e.get("bytes_out").and_then(|z| z.as_i64()).unwrap_or(0),
                            closed: matches!(e.get("closed"), Some(Value::Bool(true))),
                        });
                    }
                    // Our own subscriber row: the newest one that registered at
                    // or after the moment this connection was established.
                    let mut best: Option<&SubRow> = None;
                    for e in s.subs.iter() {
                        if e.since + 0.5 < self.connect_wall {
                            continue;
                        }
                        let better = match best {
                            None => true,
                            Some(b) => e.id > b.id && e.since >= b.since,
                        };
                        if better {
                            best = Some(e);
                        }
                    }
                    if let Some(e) = best {
                        s.sub_id = Some(e.id);
                        s.sub_depth = e.depth;
                        s.sub_max_depth = e.max_depth;
                        s.sub_sent = e.sent;
                        s.sub_dropped = e.dropped;
                        s.sub_forced = e.forced;
                        s.sub_replayed = e.replayed;
                        s.sub_replay_truncated = e.truncated;
                        s.sub_bytes_out = e.bytes_out;
                        self.replay_seen = e.replayed as u64;
                        if e.truncated > 0 {
                            self.replay_truncated_seen = e.truncated as u64;
                        }
                    }
                }
                self.status = Some(s);
            }
            "gap" => {
                self.gaps_seen += 1;
                let reason = v.get("reason").and_then(|z| z.as_str()).unwrap_or("unknown").to_string();
                *self.gap_reasons.entry(reason.clone()).or_insert(0) += 1;
                let bytes = v.get("bytes").and_then(|z| z.as_i64()).unwrap_or(0).max(0);
                self.gap_bytes += bytes as u64;
                self.gaps.push(GapRow {
                    ts,
                    reason,
                    bytes,
                    records_unattributed: v.get("records_unattributed").and_then(|z| z.as_i64()).unwrap_or(0),
                    detail: v.get("detail").and_then(|z| z.as_str()).unwrap_or("").to_string(),
                    session: sess.clone().unwrap_or_default(),
                });
                if self.gaps.len() > MAX_GAPS {
                    self.gaps.remove(0);
                }
            }
            _ => {}
        }
        None
    }

    /// Meta backfill from the collector's own recorded `meta` event.
    ///
    /// The collector now replays its stored `meta` line to every subscriber at
    /// connect (meta-on-connect), so this path is only a fallback for a
    /// collector old enough to predate that. Rather than invent a model name,
    /// the app reads back the very line the collector recorded and labels the
    /// result as coming from the collector store.
    pub fn backfill_meta(&mut self, line: &str, source: &str) -> bool {
        let v = match json::parse(line.as_bytes()) {
            Ok(v) => v,
            Err(_) => return false,
        };
        if v.get("type").and_then(|t| t.as_str()) != Some("meta") {
            return false;
        }
        if let Some(m) = v.path(&["engine", "model"]).and_then(|s| s.as_str()) {
            self.model = Some(m.to_string());
        }
        if let Some(ver) = v.path(&["collector", "version"]).and_then(|s| s.as_str()) {
            self.collector_version = Some(ver.to_string());
        }
        if let Some(e) = v.path(&["engine", "engine"]).and_then(|s| s.as_str()) {
            self.engine_name = Some(e.to_string());
        }
        if let Some(e) = v.get("engine") {
            self.engine_block = Some(json::render(e));
        }
        if let Some(r) = v.path(&["collector", "run_id"]).and_then(|s| s.as_str()) {
            if self.run_id.is_none() {
                self.run_id = Some(r.to_string());
            }
        }
        if let Some(l) = v.path(&["trace", "layers"]).and_then(|s| s.as_i64()) {
            if l > 0 && (l as usize) <= LAYER_SLOTS {
                self.layers = l as usize;
            }
        }
        if let Some(kk) = v.path(&["trace", "k"]).and_then(|s| s.as_i64()) {
            self.k = kk;
        }
        if let Some(hi) = v.path(&["trace", "expert_id_range"]).and_then(|a| match a {
            Value::Arr(x) => x.get(1).and_then(|z| z.as_i64()),
            _ => None,
        }) {
            if hi >= 0 && (hi as usize) < EXPERT_SLOTS {
                self.experts = hi as usize + 1;
            }
        }
        if self.meta_source.is_none() {
            self.meta_source = Some(source.to_string());
        }
        true
    }

    // ------------------------------------------------------- panel inventory
    /// Per-field inventory of every Stage-5 panel: each field is either live
    /// (with the value the panel draws) or explicitly unavailable with the
    /// reason. This is the machine-checkable half of the honesty rule.
    pub fn panels(&self) -> String {
        let mut o = String::with_capacity(16384);
        o.push_str("\"panels\":{");

        // ---- sessions
        o.push_str("\"sessions\":{\"panel\":\"SESSIONS\",\"source\":\"collector session_started / session_finished / pass_routed / expert_selected\",\"fields\":[");
        o.push_str(&format!(
            "{{\"name\":\"session_count\",\"status\":\"live\",\"value\":{}}},\
             {{\"name\":\"sessions_started\",\"status\":\"live\",\"value\":{}}},\
             {{\"name\":\"sessions_finished\",\"status\":\"live\",\"value\":{}}},",
            self.sessions.len(), self.sessions_started, self.sessions_finished
        ));
        match &self.current_session {
            Some(id) => o.push_str(&format!(
                "{{\"name\":\"current_session\",\"status\":\"live\",\"value\":\"{}\"}},",
                json::esc(id)
            )),
            None => o.push_str(
                "{\"name\":\"current_session\",\"status\":\"unavailable\",\"reason\":\"no collector session window is open (the engine is idle, or no client has routed through it)\"},",
            ),
        }
        let unattributed_sessions = self.sessions.iter().filter(|s| s.tier == "unattributed").count();
        o.push_str(&format!(
            "{{\"name\":\"sessions_by_tier\",\"status\":\"live\",\"value\":{{\"windowed\":{},\"unattributed\":{},\"unknown\":{}}}}},",
            self.sessions.iter().filter(|s| s.tier == "windowed").count(),
            unattributed_sessions,
            self.sessions.iter().filter(|s| s.tier == "unknown").count()
        ));
        o.push_str(&format!(
            "{{\"name\":\"unattributed_engine_requests\",\"status\":\"{}\",\"value\":{}}},",
            if self.status.is_some() { "live" } else { "unavailable" },
            self.status.as_ref().map(|s| s.unattributed_requests.to_string()).unwrap_or("null".into())
        ));
        o.push_str(&format!(
            "{{\"name\":\"foreign_sessions\",\"status\":\"live\",\"value\":{{\"windowed\":{},\"unattributed\":{},\"unknown\":{}}},\"note\":\"a session is foreign when its request_delta is 0 or its tier is unattributed: the engine is shared, so any client that writes trace bytes opens a window\"}}",
            self.sessions.iter().filter(|s| s.tier == "windowed").count(),
            unattributed_sessions,
            self.sessions.iter().filter(|s| s.tier == "unknown").count()
        ));
        o.push_str("],\"sessions\":[");
        for (i, s) in self.sessions.iter().enumerate() {
            if i > 0 {
                o.push(',');
            }
            o.push_str(&format!(
                "{{\"id\":\"{}\",\"tier\":\"{}\",\"finished\":{},\"end_reason\":\"{}\",\
\"request_delta\":{},\"requests_before\":{},\"requests_after\":{},\
\"passes\":{},\"positions\":{},\"records\":{},\"seen_passes\":{},\"seen_positions\":{},\"seen_records\":{},\
\"duration_s\":{:.3},\"trace_bytes\":{},\"started_ts\":{:.3}}}",
                json::esc(&s.id), json::esc(&s.tier), s.finished, json::esc(&s.end_reason),
                s.request_delta, s.requests_before, s.requests_after,
                s.passes, s.positions, s.records, s.seen_passes, s.seen_positions, s.seen_records,
                s.duration_s, s.trace_bytes, s.started_ts
            ));
        }
        o.push_str("]},");

        // ---- activation
        o.push_str("\"activation\":{\"panel\":\"ACTIVATION\",\"source\":\"collector expert_selected events\",\"fields\":[");
        o.push_str(&format!(
            "{{\"name\":\"records\",\"status\":\"live\",\"value\":{}}},\
             {{\"name\":\"passes\",\"status\":\"live\",\"value\":{}}},\
             {{\"name\":\"positions\",\"status\":\"live\",\"value\":{}}},\
             {{\"name\":\"live_lamps\",\"status\":\"live\",\"value\":{}}},\
             {{\"name\":\"layers_with_activity\",\"status\":\"live\",\"value\":{}}},",
            self.records, self.passes, self.positions, self.last_frame.lamps,
            self.layer_counts.iter().filter(|c| **c > 0).count()
        ));
        let top = self.layer_counts.iter().copied().max().unwrap_or(0);
        o.push_str(&format!(
            "{{\"name\":\"layer_max_selections\",\"status\":\"live\",\"value\":{}}},\
             {{\"name\":\"selected_layer\",\"status\":\"live\",\"value\":{}}},\
             {{\"name\":\"selected_layer_selections\",\"status\":\"live\",\"value\":{}}},",
            top, self.sel_layer, self.layer_counts.get(self.sel_layer).copied().unwrap_or(0)
        ));
        // residency: the *truth* stays unavailable and says so; the estimate is
        // carried in the top-level `residency` block (status "estimate") and
        // painted on the lattice, so the panel field never pretends to be live.
        let res_reason = match &self.residency {
            Some(r) if r.status == "estimate" => format!(
                "no per-expert residency events exist (the cache pre-fills from a static profile and never evicts); the lattice tint is a labelled ESTIMATE from model structure: {} VRAM-prefix cells, {} RAM cells, {} SSD cells (slots {}), never live truth",
                r.vram, r.ram, r.ssd, r.slots
            ),
            Some(r) if !r.reason.is_empty() => format!("no residency events; estimate unavailable: {}", r.reason),
            Some(_) => "no per-expert residency events exist and no estimate was sent".to_string(),
            None => "no residency_estimate event received yet; nothing is painted".to_string(),
        };
        o.push_str(&format!(
            "{{\"name\":\"expert_residency\",\"status\":\"unavailable\",\"reason\":\"{}\"}},",
            json::esc(&res_reason)
        ));
        o.push_str(&format!(
            "\
             {{\"name\":\"routing_scores\",\"status\":\"unavailable\",\"reason\":\"serve-path weights are all 0.0 in the trace (ids only), so no score exists to display\"}},\n\
             {{\"name\":\"co_activation_matrix\",\"status\":\"live\",\"value\":{{\"records_folded\":{},\"distinct_pairs\":{}}},\"note\":\"pairs counted inside each record's real top-k set; view 6 draws them\"}},\n\
             {{\"name\":\"historical_trails\",\"status\":\"live\",\"value\":{{\"cells_with_history\":{},\"total_selections\":{}}},\"note\":\"per-cell counts with the event timestamps they arrived with (t key toggles the trail bars)\"}},\n\
             {{\"name\":\"token_text\",\"status\":\"unavailable\",\"reason\":\"the v1 stream carries pass_index and position but no decoded token text\"}},\n\
             {{\"name\":\"expert_history_replay\",\"status\":\"unavailable\",\"reason\":\"stored-session playback is not in this build: the live trail is cumulative since app start, while the collector keeps the full routing history for --replay and --export\"}}\n\
            ],\"layer_selections\":[",
            self.pairs_records, self.pairs_distinct(), self.cells_with_history(),
            self.history_totals().0
        ));
        for (i, c) in self.layer_counts.iter().enumerate() {
            if i > 0 {
                o.push(',');
            }
            o.push_str(&c.to_string());
        }
        o.push_str("],\"hot_experts\":[");
        for (i, (e, c)) in self.hot_experts(HOT_N).iter().enumerate() {
            if i > 0 {
                o.push(',');
            }
            o.push_str(&format!("{{\"expert\":{},\"count\":{}}}", e, c));
        }
        o.push_str("]},");

        // ---- perf
        o.push_str("\"perf\":{\"panel\":\"ENGINE PERF\",\"source\":\"collector perf_sample (engine /metrics poll)\",\"fields\":[");
        match &self.perf {
            Some(p) => {
                o.push_str(&format!(
                    "{{\"name\":\"engine_state\",\"status\":\"live\",\"value\":\"{}\"}},",
                    json::esc(if p.state.is_empty() { "unknown" } else { &p.state })
                ));
                match p.tok_s {
                    Some(v) => o.push_str(&format!(
                        "{{\"name\":\"tok_s\",\"status\":\"live\",\"value\":{v:.3}}},")),
                    None => o.push_str(
                        "{\"name\":\"tok_s\",\"status\":\"unavailable\",\"reason\":\"the engine's live.tok_s field was null in the most recent /metrics sample (idle or prefill)\"},",
                    ),
                }
                o.push_str(&format!(
                    "{{\"name\":\"requests_total\",\"status\":\"{}\",\"value\":{}}},",
                    if p.requests.is_some() { "live" } else { "unavailable" },
                    p.requests.map(|v| v.to_string()).unwrap_or("null".into())
                ));
                o.push_str(&format!(
                    "{{\"name\":\"prompt_tokens\",\"status\":\"live\",\"value\":{}}},\
                     {{\"name\":\"output_tokens\",\"status\":\"live\",\"value\":{}}},\
                     {{\"name\":\"drafts_offered\",\"status\":\"live\",\"value\":{}}},\
                     {{\"name\":\"drafts_accepted\",\"status\":\"live\",\"value\":{}}},\
                     {{\"name\":\"delta_requests\",\"status\":\"live\",\"value\":{}}},\
                     {{\"name\":\"delta_output_tokens\",\"status\":\"live\",\"value\":{}}},
                     {{\"name\":\"delta_drafts_offered\",\"status\":\"live\",\"value\":{}}},\
                     {{\"name\":\"sample_ts\",\"status\":\"live\",\"value\":{:.3}}},\
                     {{\"name\":\"samples_seen\",\"status\":\"live\",\"value\":{}}},",
                    p.prompt_tokens, p.output_tokens, p.drafts_offered, p.drafts_accepted,
                    p.delta_requests, p.delta_output, p.delta_drafts, p.ts, self.perf_samples
                ));
            }
            None => o.push_str(
                "{\"name\":\"engine_state\",\"status\":\"unavailable\",\"reason\":\"no perf_sample received yet\"},\
                 {\"name\":\"tok_s\",\"status\":\"unavailable\",\"reason\":\"no perf_sample received yet\"},",
            ),
        }
        o.push_str(
            "{\"name\":\"ttft_ms\",\"status\":\"unavailable\",\"reason\":\"perf_sample carries no time-to-first-token field; the collector whitemplates only live.tok_s and totals, so TTFT would have to be invented\"},\
             {\"name\":\"inter_token_latency_ms\",\"status\":\"unavailable\",\"reason\":\"no per-token timing in the v1 stream\"},\
             {\"name\":\"prompt_processing_rate\",\"status\":\"unavailable\",\"reason\":\"perf_sample carries prompt_tokens totals but no prefill rate\"},\
             {\"name\":\"request_duration\",\"status\":\"unavailable\",\"reason\":\"perf_sample carries no per-request duration\"}\
            ]},");

        // ---- stream health
        o.push_str("\"stream\":{\"panel\":\"STREAM HEALTH\",\"source\":\"collector telemetry_status / gap events + app-side accounting\",\"fields\":[");
        o.push_str(&format!(
            "{{\"name\":\"connected\",\"status\":\"live\",\"value\":{}}},\
             {{\"name\":\"connects\",\"status\":\"live\",\"value\":{}}},\
             {{\"name\":\"disconnects\",\"status\":\"live\",\"value\":{}}},\
             {{\"name\":\"connect_errors\",\"status\":\"live\",\"value\":{}}},\
             {{\"name\":\"last_seq\",\"status\":\"live\",\"value\":{}}},\
             {{\"name\":\"seq_gaps\",\"status\":\"live\",\"value\":{}}},\
             {{\"name\":\"seq_missing\",\"status\":\"live\",\"value\":{}}},\
             {{\"name\":\"events_seen\",\"status\":\"live\",\"value\":{}}},\
             {{\"name\":\"records_seen\",\"status\":\"live\",\"value\":{}}},\
             {{\"name\":\"gap_events\",\"status\":\"live\",\"value\":{}}},\
             {{\"name\":\"gap_bytes\",\"status\":\"live\",\"value\":{}}},\
             {{\"name\":\"skipped_bytes_seen\",\"status\":\"live\",\"value\":{}}},",
            self.connected, self.connects, self.disconnects, self.connect_errors,
            self.last_seq, self.seq_gaps, self.seq_missing, self.events_total, self.records,
            self.gaps_seen, self.gap_bytes, self.skipped_bytes_seen
        ));
        match &self.status {
            Some(s) => o.push_str(&format!(
                "{{\"name\":\"events_per_s\",\"status\":\"live\",\"value\":{:.2}}},\
                 {{\"name\":\"records_per_s\",\"status\":\"live\",\"value\":{:.2}}},\
                 {{\"name\":\"bytes_per_s\",\"status\":\"live\",\"value\":{:.0}}},\
                 {{\"name\":\"queue_depth_ours\",\"status\":\"live\",\"value\":{}}},\
                 {{\"name\":\"queue_cap\",\"status\":\"live\",\"value\":{}}},\
                 {{\"name\":\"drops_ours\",\"status\":\"live\",\"value\":{}}},\
                 {{\"name\":\"forced_drops_ours\",\"status\":\"live\",\"value\":{}}},\
                 {{\"name\":\"drops_total\",\"status\":\"live\",\"value\":{}}},\
                 {{\"name\":\"skipped_bytes_total\",\"status\":\"live\",\"value\":{}}},\
                 {{\"name\":\"resyncs_total\",\"status\":\"live\",\"value\":{}}},\
                 {{\"name\":\"discarded_passes\",\"status\":\"live\",\"value\":{}}},\
                 {{\"name\":\"trace_anchor\",\"status\":\"live\",\"value\":{}}},
                 {{\"name\":\"held_records\",\"status\":\"live\",\"value\":{}}},
                 {{\"name\":\"file_size\",\"status\":\"live\",\"value\":{}}},
                 {{\"name\":\"skipped_bytes_since_last_event\",\"status\":\"live\",\"value\":{}}},
                 {{\"name\":\"engine_errors\",\"status\":\"live\",\"value\":{}}},
                 {{\"name\":\"replay_max\",\"status\":\"live\",\"value\":{}}},
                 {{\"name\":\"collector_db_enabled\",\"status\":\"live\",\"value\":{}}},
                 {{\"name\":\"collector_db_max_seq\",\"status\":\"live\",\"value\":{}}},
                 {{\"name\":\"collector_db_last_commit_ms\",\"status\":\"live\",\"value\":{:.2}}},
                 {{\"name\":\"collector_db_error\",\"status\":\"live\",\"value\":\"{}\"}},
                 {{\"name\":\"replayed_ours\",\"status\":\"live\",\"value\":{}}},\
                 {{\"name\":\"replay_truncated_ours\",\"status\":\"live\",\"value\":{}}},\
                 {{\"name\":\"subscribers\",\"status\":\"live\",\"value\":{}}},\
                 {{\"name\":\"engine_requests_total\",\"status\":\"live\",\"value\":{}}},\
                 {{\"name\":\"unattributed_requests\",\"status\":\"live\",\"value\":{}}},\
                 {{\"name\":\"collector_db_ok\",\"status\":\"live\",\"value\":{}}},\
                 {{\"name\":\"collector_wal_bytes\",\"status\":\"live\",\"value\":{}}},\
                 {{\"name\":\"trace_offset\",\"status\":\"live\",\"value\":{}}},\
                 {{\"name\":\"queue_depth_per_subscriber\",\"status\":\"live\",\"value\":{}}},",
                s.events_per_s, s.records_per_s, s.bytes_per_s, s.sub_depth, s.queue_cap,
                s.sub_dropped, s.sub_forced, s.drops_total, s.skipped_bytes_total,
                s.resyncs_total, s.discarded_passes,
                s.trace_anchor, s.held_records, s.file_size, s.skipped_since_last,
                s.engine_errors, s.replay_max, s.db_enabled, s.db_max_seq, s.db_last_commit_ms,
                json::esc(&s.db_error),
                s.sub_replayed, s.sub_replay_truncated,
                s.subscribers, s.engine_requests_total, s.unattributed_requests, s.db_ok,
                s.db_wal_bytes, s.trace_offset, s.subs.len()
            )),
            None => o.push_str(
                "{\"name\":\"telemetry_status\",\"status\":\"unavailable\",\"reason\":\"no telemetry_status received yet\"},",
            ),
        }
        o.push_str(
            "{\"name\":\"storage_health_sqlite\",\"status\":\"unavailable\",\"reason\":\"the app reads only the collector socket; the collector's own DB block is reported above (db_ok / wal_bytes) but the app does not open the DB\"},\
             {\"name\":\"per_event_latency\",\"status\":\"unavailable\",\"reason\":\"the app measures frames and counters, not per-event delivery latency\"}\
            ],\"gaps\":[");
        for (i, g) in self.gaps.iter().enumerate() {
            if i > 0 {
                o.push(',');
            }
            o.push_str(&format!(
                "{{\"ts\":{:.3},\"reason\":\"{}\",\"bytes\":{},\"records_unattributed\":{},\"session\":\"{}\",\"detail\":\"{}\"}}",
                g.ts, json::esc(&g.reason), g.bytes, g.records_unattributed,
                json::esc(&g.session), json::esc(&g.detail)
            ));
        }
        o.push_str("],\"subscribers\":[");
        if let Some(s) = &self.status {
            for (i, r) in s.subs.iter().enumerate() {
                if i > 0 {
                    o.push(',');
                }
                o.push_str(&format!(
                    "{{\"sub_id\":{},\"name\":\"{}\",\"peer\":\"{}\",\"depth\":{},\"max_depth\":{},\"sent\":{},\"dropped_droppable\":{},\"forced_drops\":{},\"replayed\":{},\"replay_truncated\":{},\"bytes_out\":{},\"closed\":{}}}",
                    r.id, json::esc(&r.name), json::esc(&r.peer), r.depth, r.max_depth, r.sent,
                    r.dropped, r.forced, r.replayed, r.truncated, r.bytes_out, r.closed
                ));
            }
        }
        o.push_str("]},");

        // ---- hardware
        o.push_str("\"hardware\":{\"panel\":\"HARDWARE\",\"source\":\"collector hw_sample (nvidia-smi + /proc)\",\"fields\":[");
        match &self.hw {
            Some(h) => o.push_str(&format!(
                "{{\"name\":\"gpu_name\",\"status\":\"{}\",\"value\":\"{}\"}},\
                 {{\"name\":\"gpu_util_pct\",\"status\":\"{}\",\"value\":{}}},\
                 {{\"name\":\"gpu_mem_used_mib\",\"status\":\"{}\",\"value\":{}}},\
                 {{\"name\":\"gpu_temp_c\",\"status\":\"{}\",\"value\":{}}},\
                 {{\"name\":\"gpu_power_w\",\"status\":\"{}\",\"value\":{}}},\
                 {{\"name\":\"load1\",\"status\":\"{}\",\"value\":{}}},\
                 {{\"name\":\"mem_available_mib\",\"status\":\"{}\",\"value\":{}}},\
                 {{\"name\":\"collector_rss_kib\",\"status\":\"{}\",\"value\":{}}},",
                if h.gpu_name.is_empty() { "unavailable" } else { "live" }, json::esc(&h.gpu_name),
                live_opt(h.util_pct), fmt_opt(h.util_pct),
                live_opt(h.mem_used_mib), fmt_opt(h.mem_used_mib),
                live_opt(h.temp_c), fmt_opt(h.temp_c),
                live_opt(h.power_w), fmt_opt(h.power_w),
                live_opt(h.load1), fmt_opt(h.load1),
                live_opt(h.mem_available_mib), fmt_opt(h.mem_available_mib),
                if h.rss_kib.is_some() { "live" } else { "unavailable" },
                h.rss_kib.map(|v| v.to_string()).unwrap_or("null".into())
            )),
            None => o.push_str(
                "{\"name\":\"gpu\",\"status\":\"unavailable\",\"reason\":\"no hw_sample received yet\"},",
            ),
        }
        o.push_str(
            "{\"name\":\"nvme_storage\",\"status\":\"unavailable\",\"reason\":\"hw_sample carries no disk/NVMe fields\"},\
             {\"name\":\"model_residency_bytes\",\"status\":\"unavailable\",\"reason\":\"no per-tier byte residency exists; the lattice carries a labelled estimate of cells, not bytes\"}\
            ]},");

        // ---- model / geometry
        o.push_str("\"model\":{\"panel\":\"HEADER\",\"fields\":[");
        o.push_str(&format!(
            "{{\"name\":\"model\",\"status\":\"{}\",\"value\":{}}},\
             {{\"name\":\"meta_source\",\"status\":\"{}\",\"value\":{}}},\
             {{\"name\":\"collector_version\",\"status\":\"{}\",\"value\":{}}},\
             {{\"name\":\"schema\",\"status\":\"{}\",\"value\":{}}},\
             {{\"name\":\"run_id\",\"status\":\"{}\",\"value\":{}}},\
             {{\"name\":\"engine_version\",\"status\":\"{}\",\"value\":{}}},\
             {{\"name\":\"layers\",\"status\":\"live\",\"value\":{}}},\
             {{\"name\":\"experts\",\"status\":\"live\",\"value\":{}}},\
             {{\"name\":\"top_k\",\"status\":\"{}\",\"value\":{}}},",
            if self.model.is_some() { "live" } else { "unavailable" }, opt_str(&self.model),
            if self.meta_source.is_some() { "live" } else { "unavailable" }, opt_str(&self.meta_source),
            if self.collector_version.is_some() { "live" } else { "unavailable" }, opt_str(&self.collector_version),
            if self.schema.is_some() { "live" } else { "unavailable" },
            self.schema.map(|v| v.to_string()).unwrap_or("null".into()),
            if self.run_id.is_some() { "live" } else { "unavailable" }, opt_str(&self.run_id),
            if self.engine_name.is_some() { "live" } else { "unavailable" }, opt_str(&self.engine_name),
            self.layers, self.experts,
            if self.k > 0 { "live" } else { "unavailable" }, self.k
        ));
        o.push_str(
            "{\"name\":\"model_architecture_detail\",\"status\":\"unavailable\",\"reason\":\"meta carries layer count, top-k and the expert id range only — not the per-layer structure (PLE/QSA/GDN/shared experts)\"}\
            ],\"engine_block\":");
        o.push_str(self.engine_block.as_deref().unwrap_or("null"));
        o.push_str("}");

        o.push_str("},\"unavailable\":[");
        o.push_str(
            "{\"field\":\"expert_residency_truth\",\"owner\":\"engine instrumentation\",\"reason\":\"no per-expert residency truth exists (static profile prefill, no eviction); the lattice paints a labelled ESTIMATE from model structure and says so\"},\n
             {\"field\":\"routing_scores_weights\",\"owner\":\"engine\",\"reason\":\"serve-path trace weights are all 0.0 (ids only)\"},\
             {\"field\":\"token_text\",\"owner\":\"engine\",\"reason\":\"no token text in the v1 stream\"},\
             {\"field\":\"ttft_and_inter_token_latency\",\"owner\":\"collector\",\"reason\":\"perf_sample exposes live.tok_s and totals, no latency fields\"},\
             {\"field\":\"prompt_processing_rate\",\"owner\":\"collector\",\"reason\":\"no prefill rate in perf_sample\"},\
             {\"field\":\"nvme_ssd_metrics\",\"owner\":\"collector\",\"reason\":\"hw_sample has no disk fields\"}\
            ]");
        o
    }

    // ---------------------------------------------------------------- dump
    pub fn dump(&self) -> String {
        let mut o = String::with_capacity(16384);
        o.push('{');
        o.push_str(&format!("\"pid\":{},\"socket\":\"{}\",\"uptime_s\":{:.3},",
            self.pid, json::esc(&self.socket), crate::now_unix() - self.started_wall));
        o.push_str(&format!("\"connected\":{},\"connects\":{},\"disconnects\":{},\"connect_errors\":{},\"connected_since\":{:.3},\"last_disconnect_reason\":\"{}\",\"reconnects_with_replay\":{},",
            self.connected, self.connects, self.disconnects, self.connect_errors,
            self.connected_since, json::esc(&self.last_disconnect_reason), self.reconnects_with_replay));
        o.push_str(&format!("\"view\":{},\"sel_layer\":{},\"sel_pinned\":{},", self.view, self.sel_layer, self.sel_pinned));
        o.push_str("\"placement\":{");
        o.push_str(&format!("\"requested\":\"{}\",\"monitor\":\"{}\",\"monitor_id\":{},\"x\":{},\"y\":{},\"w\":{},\"h\":{},\"fullscreen\":{},\"verified\":{},\"note\":\"{}\"",
            json::esc(&self.placement.requested), json::esc(&self.placement.monitor), self.placement.monitor_id,
            self.placement.x, self.placement.y, self.placement.w, self.placement.h,
            self.placement.fullscreen, self.placement.verified, json::esc(&self.placement.note)));
        o.push_str("},");
        o.push_str("\"meta\":{");
        o.push_str(&format!("\"model\":{},", opt_str(&self.model)));
        o.push_str(&format!("\"collector_version\":{},", opt_str(&self.collector_version)));
        o.push_str(&format!("\"schema\":{},", self.schema.map(|v| v.to_string()).unwrap_or("null".into())));
        o.push_str(&format!("\"run_id\":{},", opt_str(&self.run_id)));
        o.push_str(&format!("\"engine_version\":{},", opt_str(&self.engine_name)));
        o.push_str(&format!("\"meta_source\":{},", opt_str(&self.meta_source)));
        o.push_str(&format!("\"meta_events\":{},", self.meta_events));
        o.push_str(&format!("\"layers\":{},\"experts\":{},\"k\":{}", self.layers, self.experts, self.k));
        o.push_str("},");
        o.push_str(&format!("\"events_total\":{},\"events_ignored_dup\":{},", self.events_total, self.events_ignored_dup));
        o.push_str("\"events_by_type\":{");
        let mut keys: Vec<&String> = self.by_type.keys().collect();
        keys.sort();
        for (i, k) in keys.iter().enumerate() {
            if i > 0 {
                o.push(',');
            }
            o.push_str(&format!("\"{}\":{}", json::esc(k), self.by_type[*k]));
        }
        o.push_str("},");
        o.push_str(&format!("\"passes\":{},\"positions\":{},\"records\":{},", self.passes, self.positions, self.records));
        o.push_str(&format!("\"sessions_started\":{},\"sessions_finished\":{},\"sessions_total_seen\":{},",
            self.sessions_started, self.sessions_finished, self.sessions.len()));
        o.push_str(&format!("\"current_session\":{},", self.current_session.as_ref().map(|s| format!("\"{}\"", json::esc(s))).unwrap_or("null".into())));
        o.push_str(&format!("\"last_seq\":{},\"seq_gaps\":{},\"seq_missing\":{},\"seq_rebaselines\":{},",
            self.last_seq, self.seq_gaps, self.seq_missing, self.seq_rebaselines));
        o.push_str(&format!("\"gaps_seen\":{},\"gap_bytes\":{},\"skipped_bytes_seen\":{},\"replay_seen\":{},\"replay_truncated_seen\":{},",
            self.gaps_seen, self.gap_bytes, self.skipped_bytes_seen, self.replay_seen, self.replay_truncated_seen));
        o.push_str("\"gap_reasons\":{");
        let mut gk: Vec<&String> = self.gap_reasons.keys().collect();
        gk.sort();
        for (i, k) in gk.iter().enumerate() {
            if i > 0 {
                o.push(',');
            }
            o.push_str(&format!("\"{}\":{}", json::esc(k), self.gap_reasons[*k]));
        }
        o.push_str("},");

        // sessions
        o.push_str("\"sessions\":[");
        for (i, s) in self.sessions.iter().enumerate() {
            if i > 0 {
                o.push(',');
            }
            o.push_str(&format!(
                "{{\"id\":\"{}\",\"tier\":\"{}\",\"finished\":{},\"end_reason\":\"{}\",\"started_ts\":{:.3},\"duration_s\":{:.3},\
\"requests_before\":{},\"requests_after\":{},\"request_delta\":{},\
\"passes\":{},\"positions\":{},\"records\":{},\"trace_bytes\":{},\"first_seq\":{},\
\"seen_passes\":{},\"seen_positions\":{},\"seen_records\":{}}}",
                json::esc(&s.id), json::esc(&s.tier), s.finished, json::esc(&s.end_reason), s.started_ts, s.duration_s,
                s.requests_before, s.requests_after, s.request_delta,
                s.passes, s.positions, s.records, s.trace_bytes, s.first_seq,
                s.seen_passes, s.seen_positions, s.seen_records
            ));
        }
        o.push_str("],");

        // latest samples
        match &self.perf {
            Some(p) => o.push_str(&format!(
                "\"perf\":{{\"ts\":{:.3},\"engine_ok\":{},\"error\":\"{}\",\"baseline\":{},\"state\":\"{}\",\"tok_s\":{},\
\"requests\":{},\"prompt_tokens\":{},\"output_tokens\":{},\"drafts_offered\":{},\"drafts_accepted\":{},\
\"delta_requests\":{},\"delta_prompt_tokens\":{},\"delta_output_tokens\":{},\"samples\":{}}},",
                p.ts, p.engine_ok, json::esc(&p.error), p.baseline, json::esc(&p.state),
                fmt_opt(p.tok_s), p.requests.map(|v| v.to_string()).unwrap_or("null".into()),
                p.prompt_tokens, p.output_tokens, p.drafts_offered, p.drafts_accepted,
                p.delta_requests, p.delta_prompt, p.delta_output, self.perf_samples)),
            None => o.push_str("\"perf\":null,"),
        }
        match &self.hw {
            Some(h) => o.push_str(&format!(
                "\"hw\":{{\"ts\":{:.3},\"gpu_name\":\"{}\",\"util_pct\":{},\"mem_used_mib\":{},\"temp_c\":{},\"power_w\":{},\"load1\":{},\"mem_available_mib\":{},\"rss_kib\":{}}},",
                h.ts, json::esc(&h.gpu_name), fmt_opt(h.util_pct), fmt_opt(h.mem_used_mib), fmt_opt(h.temp_c),
                fmt_opt(h.power_w), fmt_opt(h.load1), fmt_opt(h.mem_available_mib),
                h.rss_kib.map(|v| v.to_string()).unwrap_or("null".into()))),
            None => o.push_str("\"hw\":null,"),
        }
        match &self.status {
            Some(s) => o.push_str(&format!(
                "\"status\":{{\"ts\":{:.3},\"uptime_s\":{:.1},\"events_per_s\":{:.2},\"records_per_s\":{:.2},\"bytes_per_s\":{:.0},\
\"subscribers\":{},\"queue_cap\":{},\"drops_total\":{},\"skipped_bytes_total\":{},\"resyncs_total\":{},\"discarded_passes\":{},\
\"gap_bytes_total\":{},\"engine_ok\":{},\"engine_state\":\"{}\",\"engine_requests_total\":{},\"session_active\":{},\"unattributed_requests\":{},\
\"events_total_collector\":{},\"sessions_total_collector\":{},\"held_records\":{},\"trace_offset\":{},\"file_size\":{},\"replay_max\":{},\
\"our_sub\":{{\"sub_id\":{},\"depth\":{},\"max_depth\":{},\"sent\":{},\"dropped_droppable\":{},\"forced_drops\":{},\"replayed\":{},\"replay_truncated\":{},\"bytes_out\":{}}},\
\"subscribers_detail\":[{}]}},",
                s.ts, s.uptime_s, s.events_per_s, s.records_per_s, s.bytes_per_s,
                s.subscribers, s.queue_cap, s.drops_total, s.skipped_bytes_total, s.resyncs_total, s.discarded_passes,
                s.gap_bytes_total, s.engine_ok, json::esc(&s.engine_state), s.engine_requests_total, s.session_active,
                s.unattributed_requests, s.events_total, s.sessions_total, s.held_records, s.trace_offset, s.file_size, s.replay_max,
                s.sub_id.map(|v| v.to_string()).unwrap_or("null".into()), s.sub_depth, s.sub_max_depth, s.sub_sent,
                s.sub_dropped, s.sub_forced, s.sub_replayed, s.sub_replay_truncated, s.sub_bytes_out,
                s.subs.iter().map(|r| format!(
                    "{{\"sub_id\":{},\"name\":\"{}\",\"depth\":{},\"sent\":{},\"dropped_droppable\":{},\"forced_drops\":{},\"replayed\":{},\"replay_truncated\":{},\"closed\":{}}}",
                    r.id, json::esc(&r.name), r.depth, r.sent, r.dropped, r.forced, r.replayed, r.truncated, r.closed
                )).collect::<Vec<_>>().join(","))),
            None => o.push_str("\"status\":null,"),
        }

        o.push_str("\"layer_activation_totals\":[");
        for (i, c) in self.layer_counts.iter().enumerate() {
            if i > 0 {
                o.push(',');
            }
            o.push_str(&c.to_string());
        }
        o.push_str("],\"expert_hot\":[");
        for (i, (e, c)) in self.hot_experts(8).iter().enumerate() {
            if i > 0 {
                o.push(',');
            }
            o.push_str(&format!("{{\"expert\":{},\"count\":{}}}", e, c));
        }
        o.push_str("],\"perf_hist\":[");
        let start = self.perf_hist.len().saturating_sub(120);
        for (i, (ts, tok)) in self.perf_hist[start..].iter().enumerate() {
            if i > 0 {
                o.push(',');
            }
            o.push_str(&format!("[{:.1},{}]", ts, fmt_opt(*tok)));
        }
        o.push_str("],");

        o.push_str(&self.panels());
        o.push(',');

        // ------------------------------------------------ Stage 6: theme + interaction
        //
        // Written as plain concatenation rather than one big format!: every JSON
        // brace here is structural, and a format! string would need them doubled
        // (a brace-count mistake there silently produced invalid JSON once).
        o.push_str(&crate::render::theme_json());
        o.push(',');
        o.push_str("\"settings\":{\"rain\":");
        o.push_str(if self.rain { "true" } else { "false" });
        o.push_str(",\"rain_density\":");
        o.push_str(&format!("{:.2}", self.rain_density));
        o.push_str(",\"rain_speed\":");
        o.push_str(&format!("{:.2}", self.rain_speed));
        o.push_str(",\"glow\":");
        o.push_str(&format!("{:.2}", self.glow));
        o.push_str(",\"wash_alpha\":");
        o.push_str(&format!("{:.2}", crate::render::WASH_ALPHA));
        o.push_str(",\"bg_opacity\":");
        o.push_str(&format!("{:.2}", crate::render::BG_OPACITY));
        o.push_str(",\"trails\":");
        o.push_str(if self.trails { "true" } else { "false" });
        o.push_str(",\"motion\":\"");
        o.push_str(if self.motion_reduced { "reduced" } else { "full" });
        o.push_str("\",\"banner\":{\"enabled\":");
        o.push_str(if self.banner.enabled { "true" } else { "false" });
        o.push_str(&format!(",\"interval_s\":{:.1},\"phase\":\"{}\",\"phase_id\":{},\"progress\":{:.3},\"cycles\":{},\"text\":\"FOLLOW THE WHITE RABBIT\"}}",
            self.banner.interval_s, self.banner.phase_name(), self.banner.phase,
            self.banner.progress, self.banner.cycles));
        o.push_str(",\"fps_ceiling\":60,\"keys\":\"+/- zoom · w/a/s/d pan · 0 reset · t trails · 1-6 views · b banner · r rain · j/k layer\"},");

        let (r0, r1, r2, r3) = self.view_rect;
        o.push_str(&format!("\"viewport\":{{\"zoom\":{:.3},\"pan_x\":{:.2},\"pan_y\":{:.2},\"min_zoom\":{},\"max_zoom\":{},\"rect\":{{\"x\":{:.2},\"y\":{:.2},\"w\":{:.2},\"h\":{:.2}}},\"cell\":{:.3},\"cell_h\":{:.3},\"visible_cells\":{},\"layers\":{},\"experts\":{},\"lattice_panel\":{{\"w\":{:.1},\"h\":{:.1}}}}},",
            self.zoom, self.pan_x, self.pan_y, MIN_ZOOM, MAX_ZOOM, r0, r1, r2, r3,
            self.view_cell, self.view_cell_h, self.visible_cells, self.layers, self.experts,
            r2 / self.zoom.max(0.001), r3 / self.zoom.max(0.001)));

        let (hist_total, hist_first, hist_last) = self.history_totals();
        o.push_str("\"trails\":{\"enabled\":");
        o.push_str(if self.trails { "true" } else { "false" });
        o.push_str(&format!(",\"cells_with_history\":{},\"total_selections\":{},\"first_event_ts\":{:.3},\"last_event_ts\":{:.3},\"span_s\":{:.1}",
            self.cells_with_history(), hist_total, hist_first, hist_last,
            (hist_last - hist_first).max(0.0)));
        o.push_str(",\"mode\":\"cumulative since app start · per-cell counts and the collector timestamps of the events that produced them\"},");

        o.push_str(&format!("\"coactivation\":{{\"records_folded\":{},\"pairs_total\":{},\"distinct_pairs\":{},\"top\":[",
            self.pairs_records, self.pairs_total(), self.pairs_distinct()));
        for (i, (a, b, c)) in self.top_pairs(20).iter().enumerate() {
            if i > 0 {
                o.push(',');
            }
            o.push_str(&format!("{{\"a\":{},\"b\":{},\"count\":{}}}", a, b, c));
        }
        o.push_str("],\"source\":\"each expert_selected event carries the whole top-k set for one layer+position, so every pair inside it co-occurred; pairs are counted expert-major over all layers\"},");

        o.push_str(&format!("\"effects\":{{\"activation_bursts_total\":{},\"pulses_total\":{},",
            self.bursts_total, self.pulses_total));
        o.push_str("\"source\":\"one burst and one pulse per real expert_selected record (activation-linked rain intensification, §8.3); nothing is spawned per frame and nothing without an event\"},");

        o.push_str("\"input_selftest\":{\"enabled\":");
        o.push_str(if self.selftest_enabled { "true" } else { "false" });
        o.push_str(&format!(",\"source\":\"{}\",\"commands_applied\":{},\"errors\":{},\"last\":\"{}\",\"last_ts\":{:.3}",
            json::esc(&self.selftest_source), self.selftest_commands, self.selftest_errors,
            json::esc(&self.selftest_last), self.selftest_last_ts));
        o.push_str(",\"note\":\"labelled INPUT SELF-TEST: a control file that injects the same commands the keyboard does (zoom/pan/view/trails). It drives UI state only and never produces telemetry\"},\n");

        o.push_str(&format!("\"rain\":{},\"frames\":{},\"redraw_requests\":{},\"repaints_unchanged\":{},\"wakes_skipped\":{},\"requested_frames\":{},\"pending_redraw\":{},\"frames_change\":{},\"frames_anim\":{},\"frames_resize\":{},\"frames_spurious\":{},\"last_frame\":{{\"quads\":{},\"glyphs\":{},\"lamps\":{},\"streams\":{},\"trail_quads\":{},\"pulse_quads\":{},\"banner_quads\":{},\"visible_cells\":{},\"rain_families\":{},\"res_cells\":{},\"res_len\":{},\"res_epoch_seen\":{}}}",
            self.rain, self.frames, self.redraw_requests, self.repaints_unchanged, self.wakes_skipped,
            self.requested_frames, self.pending_redraw,
            self.frames_change, self.frames_anim, self.frames_resize, self.frames_spurious,
            self.last_frame.quads, self.last_frame.glyphs, self.last_frame.lamps, self.last_frame.streams,
            self.last_frame.trail_quads, self.last_frame.pulse_quads, self.last_frame.banner_quads,
            self.last_frame.visible_cells, self.last_frame.families,
            self.last_frame.res_cells, self.last_frame.res_len, self.last_frame.res_epoch_seen));
        let (rstatus, rbasis, rconf, rslots, rvram, rram, rssd, rreason, rlayers, rexperts, rpolicy, rsource) =
            match &self.residency {
                Some(r) => (r.status.clone(), r.basis.clone(), r.confirmed, r.slots, r.vram, r.ram, r.ssd,
                            r.reason.clone(), r.layers, r.experts, r.policy.clone(), r.source.clone()),
                None => ("none".into(), String::new(), false, 0, 0, 0, 0,
                         "no residency_estimate event received".into(), self.layers, self.experts,
                         String::new(), String::new()),
            };
        o.push_str(&format!(
            ",\"residency\":{{\"events\":{},\"status\":\"{}\",\"basis\":\"{}\",\"confirmed\":{},\"slots\":{},\"vram_cells\":{},\"ram_cells\":{},\"ssd_cells\":{},\"layers\":{},\"experts\":{},\"painted\":{},\"epoch\":{},\"policy\":\"{}\",\"source\":\"{}\",\"reason\":\"{}\"}}",
            self.residency_events, json::esc(&rstatus), json::esc(&rbasis), rconf, rslots,
            rvram, rram, rssd, rlayers, rexperts, self.last_frame.res_cells, self.res_epoch,
            json::esc(&rpolicy), json::esc(&rsource), json::esc(&rreason)
        ));
        let hist: Vec<String> = self.frame_hist.iter().map(|v| v.to_string()).collect();
        o.push_str(&format!(
            ",\"frame_ms\":{{\"samples\":{},\"bucket_ms\":5,\"p50\":{:.1},\"p95\":{:.1},\"p99\":{:.1},\"max\":{:.1},\"hist\":[{}]}}",
            self.frame_samples, self.frame_pct(0.50), self.frame_pct(0.95), self.frame_pct(0.99),
            self.frame_max_ms, hist.join(",")
        ));
        o.push('}');
        o
    }
}

fn live_opt(v: Option<f64>) -> &'static str {
    if v.is_some() { "live" } else { "unavailable" }
}

fn opt_str(v: &Option<String>) -> String {
    match v {
        Some(s) => format!("\"{}\"", json::esc(s)),
        None => "null".into(),
    }
}

fn fmt_opt(v: Option<f64>) -> String {
    match v {
        Some(x) => format!("{x:.3}"),
        None => "null".into(),
    }
}
