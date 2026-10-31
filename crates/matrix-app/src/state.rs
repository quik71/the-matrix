// In-app state: everything the app knows comes from collector v1 events on the
// unix socket. The app never reads the engine, the trace, or the collector's
// SQLite file — if a field is not in the event stream it is reported as
// unavailable instead of being invented.

use std::collections::HashMap;

use crate::json::{self, Value};

pub const EXPERT_SLOTS: usize = 512;
pub const LAYER_SLOTS: usize = 48;
pub const MAX_SESSIONS: usize = 512;

#[derive(Clone)]
pub struct Session {
    pub id: String,
    pub tier: String,
    pub started_ts: f64,
    pub finished: bool,
    pub end_reason: String,
    pub requests_before: i64,
    pub requests_after: i64,
    pub request_delta: i64,
    pub passes: i64,
    pub positions: i64,
    pub records: i64,
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
            finished: false,
            end_reason: String::new(),
            requests_before: -1,
            requests_after: -1,
            request_delta: -1,
            passes: -1,
            positions: -1,
            records: -1,
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
    pub resyncs_total: i64,
    pub discarded_passes: i64,
    pub gap_bytes_total: i64,
    pub engine_ok: bool,
    pub engine_state: String,
    pub engine_requests_total: i64,
    pub session_active: bool,
    pub unattributed_requests: i64,
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

pub struct State {
    pub pid: u32,
    pub socket: String,
    pub started_wall: f64,

    // connection
    pub connected: bool,
    pub connects: u64,
    pub disconnects: u64,
    pub connect_errors: u64,
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
    pub engine_name: Option<String>, // meta.engine.engine (version) — never the API key
    pub meta_raw_engine_state: Option<String>,
    /// Where the model geometry came from: "socket" when the collector's own
    /// `meta` event arrived live, "collector-store" when it was read back from
    /// the collector's recorded meta row (see render/main note), None if the app
    /// has no meta at all — in which case the UI says unavailable.
    pub meta_source: Option<String>,

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
    pub skipped_bytes_seen: u64,
    pub replay_seen: u64,
    pub replay_truncated_seen: u64,

    // latest samples
    pub perf: Option<Perf>,
    pub hw: Option<Hw>,
    pub status: Option<Status>,

    // per-(layer,expert) activation counters, cumulative since app start
    pub layer_counts: Vec<u64>,
    pub expert_counts: Vec<u64>,

    // render facts (proves present-on-demand behaviour)
    pub frames: u64,
    pub redraw_requests: u64,
    pub last_frame: crate::render::FrameStats,
    pub rain: bool,
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
            engine_name: None,
            meta_raw_engine_state: None,
            meta_source: None,
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
            skipped_bytes_seen: 0,
            replay_seen: 0,
            replay_truncated_seen: 0,
            perf: None,
            hw: None,
            status: None,
            layer_counts: vec![0; LAYER_SLOTS],
            expert_counts: vec![0; EXPERT_SLOTS],
            frames: 0,
            redraw_requests: 0,
            last_frame: Default::default(),
            rain,
        }
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
            if let Some(l) = v.path(&["trace", "layers"]).and_then(|s| s.as_i64()) {
                if l > 0 && (l as usize) <= LAYER_SLOTS {
                    self.layers = l as usize;
                }
            }
            if let Some(hi) = v.path(&["trace", "expert_id_range"]).and_then(|a| match a {
                Value::Arr(x) => x.get(1).and_then(|z| z.as_i64()),
                _ => None,
            }) {
                if hi >= 0 && (hi as usize) < EXPERT_SLOTS {
                    self.experts = hi as usize + 1;
                }
            }
        }
        if seq >= 0 {
            if self.last_seq >= 0 && seq <= self.last_seq {
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
                    let (tier, reason, rb, ra, delta, passes, positions, records) = (
                        v.get("tier").and_then(|z| z.as_str()).unwrap_or("unknown").to_string(),
                        v.get("end_reason").and_then(|z| z.as_str()).unwrap_or("").to_string(),
                        v.get("requests_before").and_then(|z| z.as_i64()).unwrap_or(-1),
                        v.get("requests_after").and_then(|z| z.as_i64()).unwrap_or(-1),
                        v.get("request_delta").and_then(|z| z.as_i64()).unwrap_or(-1),
                        v.get("passes").and_then(|z| z.as_i64()).unwrap_or(-1),
                        v.get("positions").and_then(|z| z.as_i64()).unwrap_or(-1),
                        v.get("records").and_then(|z| z.as_i64()).unwrap_or(-1),
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
                self.perf = Some(Perf {
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
                });
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
                self.hw = Some(h);
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
                    resyncs_total: v.get("resyncs_total").and_then(|z| z.as_i64()).unwrap_or(0),
                    discarded_passes: v.get("discarded_passes").and_then(|z| z.as_i64()).unwrap_or(0),
                    gap_bytes_total: v.get("gap_bytes_total").and_then(|z| z.as_i64()).unwrap_or(0),
                    engine_ok: v.path(&["engine", "ok"]).and_then(|z| match z {
                        Value::Bool(b) => Some(*b),
                        _ => None,
                    }).unwrap_or(false),
                    engine_state: v.path(&["engine", "state"]).and_then(|z| z.as_str()).unwrap_or("").to_string(),
                    engine_requests_total: v.path(&["engine", "requests_total"]).and_then(|z| z.as_i64()).unwrap_or(0),
                    session_active: v.get("session_active").and_then(|z| match z {
                        Value::Bool(b) => Some(*b),
                        _ => None,
                    }).unwrap_or(false),
                    unattributed_requests: v.get("unattributed_requests").and_then(|z| z.as_i64()).unwrap_or(0),
                    ..Default::default()
                };
                // Our own subscriber row: the newest one that registered at or
                // after the moment this connection was established.
                if let Some(Value::Arr(list)) = v.get("per_subscriber") {
                    let mut best: Option<(&Value, i64, f64)> = None;
                    for e in list {
                        let since = e.get("since").and_then(|z| z.as_f64()).unwrap_or(0.0);
                        let id = e.get("sub_id").and_then(|z| z.as_i64()).unwrap_or(-1);
                        if since + 0.5 < self.connect_wall {
                            continue;
                        }
                        let better = match best {
                            None => true,
                            Some((_, bid, bsince)) => id > bid && since >= bsince,
                        };
                        if better {
                            best = Some((e, id, since));
                        }
                    }
                    if let Some((e, id, _)) = best {
                        s.sub_id = Some(id);
                        s.sub_depth = e.get("depth").and_then(|z| z.as_i64()).unwrap_or(0);
                        s.sub_max_depth = e.get("max_depth").and_then(|z| z.as_i64()).unwrap_or(0);
                        s.sub_sent = e.get("sent").and_then(|z| z.as_i64()).unwrap_or(0);
                        s.sub_dropped = e.get("dropped_droppable").and_then(|z| z.as_i64()).unwrap_or(0);
                        s.sub_forced = e.get("forced_drops").and_then(|z| z.as_i64()).unwrap_or(0);
                        s.sub_replayed = e.get("replayed").and_then(|z| z.as_i64()).unwrap_or(0);
                        s.sub_replay_truncated = e.get("replay_truncated").and_then(|z| z.as_i64()).unwrap_or(0);
                        s.sub_bytes_out = e.get("bytes_out").and_then(|z| z.as_i64()).unwrap_or(0);
                        self.replay_seen = s.sub_replayed as u64;
                        if s.sub_replay_truncated > 0 {
                            self.replay_truncated_seen = s.sub_replay_truncated as u64;
                        }
                    }
                }
                self.status = Some(s);
            }
            "gap" => {
                self.gaps_seen += 1;
                let reason = v.get("reason").and_then(|z| z.as_str()).unwrap_or("unknown");
                *self.gap_reasons.entry(reason.to_string()).or_insert(0) += 1;
                self.gap_bytes += v.get("bytes").and_then(|z| z.as_i64()).unwrap_or(0).max(0) as u64;
            }
            _ => {}
        }
        None
    }

    /// Meta backfill from the collector's own recorded `meta` event.
    ///
    /// The collector emits `meta` exactly once, immediately after binding its
    /// socket and before any subscriber can be registered, so a client that
    /// attaches later never receives it and its replay window is per-session
    /// only. Rather than invent a model name, the app can read back the very
    /// line the collector recorded (same JSON, same reader) and labels the
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

    // ---------------------------------------------------------------- dump
    pub fn dump(&self) -> String {
        let mut o = String::with_capacity(8192);
        o.push('{');
        o.push_str(&format!("\"pid\":{},\"socket\":\"{}\",\"uptime_s\":{:.3},",
            self.pid, json::esc(&self.socket), crate::now_unix() - self.started_wall));
        o.push_str(&format!("\"connected\":{},\"connects\":{},\"disconnects\":{},\"connect_errors\":{},\"connected_since\":{:.3},\"last_disconnect_reason\":\"{}\",\"reconnects_with_replay\":{},",
            self.connected, self.connects, self.disconnects, self.connect_errors,
            self.connected_since, json::esc(&self.last_disconnect_reason), self.reconnects_with_replay));
        o.push_str("\"meta\":{");
        o.push_str(&format!("\"model\":{},", opt_str(&self.model)));
        o.push_str(&format!("\"collector_version\":{},", opt_str(&self.collector_version)));
        o.push_str(&format!("\"schema\":{},", self.schema.map(|v| v.to_string()).unwrap_or("null".into())));
        o.push_str(&format!("\"run_id\":{},", opt_str(&self.run_id)));
        o.push_str(&format!("\"engine_version\":{},", opt_str(&self.engine_name)));
        o.push_str(&format!("\"meta_source\":{},", opt_str(&self.meta_source)));
        o.push_str(&format!("\"layers\":{},\"experts\":{}", self.layers, self.experts));
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
        o.push_str(&format!("\"last_seq\":{},\"seq_gaps\":{},\"seq_missing\":{},\"seq_rebaselines\":{},", self.last_seq, self.seq_gaps, self.seq_missing, self.seq_rebaselines));
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
                "{{\"id\":\"{}\",\"tier\":\"{}\",\"finished\":{},\"end_reason\":\"{}\",\"started_ts\":{:.3},\
\"requests_before\":{},\"requests_after\":{},\"request_delta\":{},\
\"passes\":{},\"positions\":{},\"records\":{},\
\"seen_passes\":{},\"seen_positions\":{},\"seen_records\":{}}}",
                json::esc(&s.id), json::esc(&s.tier), s.finished, json::esc(&s.end_reason), s.started_ts,
                s.requests_before, s.requests_after, s.request_delta,
                s.passes, s.positions, s.records,
                s.seen_passes, s.seen_positions, s.seen_records
            ));
        }
        o.push_str("],");

        // latest samples
        match &self.perf {
            Some(p) => o.push_str(&format!(
                "\"perf\":{{\"ts\":{:.3},\"engine_ok\":{},\"error\":\"{}\",\"baseline\":{},\"state\":\"{}\",\"tok_s\":{},\
\"requests\":{},\"prompt_tokens\":{},\"output_tokens\":{},\"drafts_offered\":{},\"drafts_accepted\":{}}},",
                p.ts, p.engine_ok, json::esc(&p.error), p.baseline, json::esc(&p.state),
                fmt_opt(p.tok_s), p.requests.map(|v| v.to_string()).unwrap_or("null".into()),
                p.prompt_tokens, p.output_tokens, p.drafts_offered, p.drafts_accepted)),
            None => o.push_str("\"perf\":null,"),
        }
        match &self.hw {
            Some(h) => o.push_str(&format!(
                "\"hw\":{{\"ts\":{:.3},\"gpu_name\":\"{}\",\"util_pct\":{},\"mem_used_mib\":{},\"temp_c\":{},\"power_w\":{},\"load1\":{},\"mem_available_mib\":{}}},",
                h.ts, json::esc(&h.gpu_name), fmt_opt(h.util_pct), fmt_opt(h.mem_used_mib), fmt_opt(h.temp_c),
                fmt_opt(h.power_w), fmt_opt(h.load1), fmt_opt(h.mem_available_mib))),
            None => o.push_str("\"hw\":null,"),
        }
        match &self.status {
            Some(s) => o.push_str(&format!(
                "\"status\":{{\"ts\":{:.3},\"uptime_s\":{:.1},\"events_per_s\":{:.2},\"records_per_s\":{:.2},\"bytes_per_s\":{:.0},\
\"subscribers\":{},\"queue_cap\":{},\"drops_total\":{},\"skipped_bytes_total\":{},\"resyncs_total\":{},\"discarded_passes\":{},\
\"gap_bytes_total\":{},\"engine_ok\":{},\"engine_state\":\"{}\",\"engine_requests_total\":{},\"session_active\":{},\"unattributed_requests\":{},\
\"events_total_collector\":{},\"sessions_total_collector\":{},\
\"our_sub\":{{\"sub_id\":{},\"depth\":{},\"max_depth\":{},\"sent\":{},\"dropped_droppable\":{},\"forced_drops\":{},\"replayed\":{},\"replay_truncated\":{},\"bytes_out\":{}}}}},",
                s.ts, s.uptime_s, s.events_per_s, s.records_per_s, s.bytes_per_s,
                s.subscribers, s.queue_cap, s.drops_total, s.skipped_bytes_total, s.resyncs_total, s.discarded_passes,
                s.gap_bytes_total, s.engine_ok, json::esc(&s.engine_state), s.engine_requests_total, s.session_active,
                s.unattributed_requests, s.events_total, s.sessions_total,
                s.sub_id.map(|v| v.to_string()).unwrap_or("null".into()), s.sub_depth, s.sub_max_depth, s.sub_sent,
                s.sub_dropped, s.sub_forced, s.sub_replayed, s.sub_replay_truncated, s.sub_bytes_out)),
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
        let mut hot: Vec<(usize, u64)> = self.expert_counts.iter().copied().enumerate().collect();
        hot.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        for (i, (e, c)) in hot.iter().take(8).enumerate() {
            if i > 0 {
                o.push(',');
            }
            o.push_str(&format!("{{\"expert\":{},\"count\":{}}}", e, c));
        }
        o.push_str("],");

        o.push_str(&format!("\"rain\":{},\"frames\":{},\"redraw_requests\":{},\"last_frame\":{{\"quads\":{},\"glyphs\":{},\"lamps\":{},\"streams\":{}}}",
            self.rain, self.frames, self.redraw_requests,
            self.last_frame.quads, self.last_frame.glyphs, self.last_frame.lamps, self.last_frame.streams));
        o.push('}');
        o
    }
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
