// matrix-collect (Stage 3): normalized MoE routing event collector.
//
//   trace tail  ->  pass assembly  ->  NDJSON events v1  ->  unix socket fan-out
//                                                         \-> SQLite (WAL)
//
// Session windows are bounded by the engine's own /metrics `totals.requests`
// counter (exactly one increment per completed request). Anything that cannot be
// attributed to such a window is marked `unattributed`; anything that had to be
// skipped (bytes or events) is counted and surfaced — this collector never
// invents events and never drops silently.

mod db;
mod json;
mod net;
mod trace;

use db::{PassRow, Store};
use net::Sub;
use std::env;
use std::io::Write;
use std::process;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use trace::Tailer;

const VERSION: &str = env!("CARGO_PKG_VERSION");
const SCHEMA_V: u32 = 1;

static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn handle_signal(_sig: i32) {
    STOP.store(true, Ordering::SeqCst);
}

unsafe extern "C" {
    fn signal(sig: i32, handler: extern "C" fn(i32)) -> usize;
}

pub fn now_unix() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

// ------------------------------------------------------------------- config
#[derive(Clone)]
struct Cfg {
    path: String,
    socket: String,
    db: String,
    engine_host: String,
    engine_port: u16,
    engine_conf: String,
    interval_ms: u64,
    no_persist: bool,
    retention_days: i64,
    max_db_mb: i64,
    replay: Option<String>,
    queue_cap: usize,
    replay_max: usize,
    metrics_ms: u64,
    status_ms: u64,
    hw_ms: u64,
    start_offset: Option<u64>,
    quiet_ms: u64,
    expert_profile: String,
    event_line_hours: f64,
    prune: bool,
    export: Option<String>,
    vacuum: bool,
}

impl Default for Cfg {
    fn default() -> Cfg {
        Cfg {
            path: "/home/main/strata-0.2.4.2/data/routing-trace.bin".into(),
            socket: "/home/main/the-matrix/run/collector.sock".into(),
            db: "/home/main/the-matrix/data/telemetry.sqlite".into(),
            engine_host: "127.0.0.1".into(),
            engine_port: 8080,
            engine_conf: "/home/main/strata-0.2.4.2/strata-iq3_s.json".into(),
            interval_ms: 150,
            no_persist: false,
            retention_days: 1,
            max_db_mb: 25000,
            replay: None,
            queue_cap: 4096,
            replay_max: 5000,
            metrics_ms: 1000,
            status_ms: 2000,
            hw_ms: 5000,
            start_offset: None,
            quiet_ms: 600,
            expert_profile: "/home/main/strata-0.2.4.2/data/expert-profile.bin".into(),
            event_line_hours: 1.0,
            prune: false,
            export: None,
            vacuum: false,
        }
    }
}

fn parse_args() -> Result<Cfg, String> {
    let mut c = Cfg::default();
    let a: Vec<String> = env::args().collect();
    let mut i = 1;
    while i < a.len() {
        let need = |i: usize| -> Result<String, String> {
            a.get(i + 1)
                .cloned()
                .ok_or_else(|| format!("{} needs a value", a[i]))
        };
        match a[i].as_str() {
            "--path" => {
                c.path = need(i)?;
                i += 2;
            }
            "--socket" => {
                c.socket = need(i)?;
                i += 2;
            }
            "--db" => {
                c.db = need(i)?;
                i += 2;
            }
            "--engine" => {
                let v = need(i)?;
                let (h, p) = v.rsplit_once(':').ok_or("--engine host:port")?;
                c.engine_host = h.to_string();
                c.engine_port = p.parse().map_err(|_| "--engine port")?;
                i += 2;
            }
            "--engine-conf" => {
                c.engine_conf = need(i)?;
                i += 2;
            }
            "--interval-ms" => {
                c.interval_ms = need(i)?.parse().map_err(|_| "--interval-ms")?;
                i += 2;
            }
            "--queue-cap" => {
                c.queue_cap = need(i)?.parse().map_err(|_| "--queue-cap")?;
                i += 2;
            }
            "--replay-max" => {
                c.replay_max = need(i)?.parse().map_err(|_| "--replay-max")?;
                i += 2;
            }
            "--metrics-ms" => {
                c.metrics_ms = need(i)?.parse().map_err(|_| "--metrics-ms")?;
                i += 2;
            }
            "--status-ms" => {
                c.status_ms = need(i)?.parse().map_err(|_| "--status-ms")?;
                i += 2;
            }
            "--hw-ms" => {
                c.hw_ms = need(i)?.parse().map_err(|_| "--hw-ms")?;
                i += 2;
            }
            "--quiet-ms" => {
                c.quiet_ms = need(i)?.parse().map_err(|_| "--quiet-ms")?;
                i += 2;
            }
            "--max-db-mb" => {
                c.max_db_mb = need(i)?.parse().map_err(|_| "--max-db-mb")?;
                i += 2;
            }
            "--retention-days" => {
                c.retention_days = need(i)?.parse().map_err(|_| "--retention-days")?;
                i += 2;
            }
            "--offset" => {
                c.start_offset = Some(need(i)?.parse().map_err(|_| "--offset")?);
                i += 2;
            }
            "--no-persist" => {
                c.no_persist = true;
                i += 1;
            }
            "--replay" => {
                c.replay = Some(need(i)?);
                i += 2;
            }
            "--expert-profile" => {
                c.expert_profile = need(i)?;
                i += 2;
            }
            "--event-line-hours" => {
                c.event_line_hours = need(i)?.parse().map_err(|_| "--event-line-hours")?;
                i += 2;
            }
            "--prune" => {
                c.prune = true;
                i += 1;
            }
            "--vacuum" => {
                c.vacuum = true;
                i += 1;
            }
            "--export" => {
                c.export = Some(need(i)?);
                i += 2;
            }
            "--help" | "-h" => {
                println!(
                    "matrix-collect {VERSION}\n\
                     --path <trace>          routing-trace.bin to tail\n\
                     --socket <path>         unix socket (default run/collector.sock)\n\
                     --db <path>             sqlite database\n\
                     --engine <host:port>    engine /metrics endpoint\n\
                     --engine-conf <path>    engine config (api_key read at runtime, never logged)\n\
                     --interval-ms N         tail poll interval (150)\n\
                     --metrics-ms N          /metrics poll interval (1000)\n\
                     --status-ms N           telemetry_status interval (2000)\n\
                     --hw-ms N               hw_sample interval (5000)\n\
                     --queue-cap N           per-subscriber queue bound (4096)\n\
                     --replay-max N          reconnect replay bound in events (5000)\n\
                     --retention-days N      real ceiling: passes/sessions/routing/expert lines/perf/
                     events older than N days are deleted (1); meta/gap/residency kept;
                     0 disables the time window (the size cap still bounds the store)
                     --max-db-mb N           hard size cap in MB (25000), applied after expiry:
                     oldest sessions, then routing/events, are evicted until it fits
                     --no-persist            run without SQLite\n\
                     --replay <session>      re-emit a stored session as NDJSON and exit\n\
                     --offset N              explicit start offset (default: persisted cursor, else EOF)"
                );
                process::exit(0);
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    Ok(c)
}

// ------------------------------------------------------------------ metrics
#[derive(Clone, Default)]
struct Metrics {
    requests: i64,
    prompt_tokens: i64,
    output_tokens: i64,
    drafts_offered: i64,
    drafts_accepted: i64,
    state: String,
    tok_s: Option<f64>,
    raw: Option<String>,
    ok: bool,
    err: String,
    ts: f64,
}

fn fetch_metrics(cfg: &Cfg, key: &str) -> Metrics {
    let body = match net::http_get(&cfg.engine_host, cfg.engine_port, "/metrics", key) {
        Ok(b) => b,
        Err(e) => {
            return Metrics {
                ok: false,
                err: e,
                ts: now_unix(),
                ..Default::default()
            }
        }
    };
    let v = match json::parse(body.as_bytes()) {
        Ok(v) => v,
        Err(e) => {
            return Metrics {
                ok: false,
                err: format!("json: {e}"),
                ts: now_unix(),
                ..Default::default()
            }
        }
    };
    let g = |p: &[&str]| v.path(p).and_then(|x| x.as_i64()).unwrap_or(0);
    Metrics {
        requests: g(&["totals", "requests"]),
        prompt_tokens: g(&["totals", "prompt_tokens"]),
        output_tokens: g(&["totals", "output_tokens"]),
        drafts_offered: g(&["totals", "drafts_offered"]),
        drafts_accepted: g(&["totals", "drafts_accepted"]),
        state: v
            .path(&["live", "state"])
            .and_then(|x| x.as_str())
            .unwrap_or("?")
            .to_string(),
        tok_s: v.path(&["live", "tok_s"]).and_then(|x| x.as_f64()),
        raw: Some(body),
        ok: true,
        err: String::new(),
        ts: now_unix(),
    }
}

fn read_engine_key(path: &str) -> Result<String, String> {
    let raw = std::fs::read(path).map_err(|e| format!("read engine conf: {e}"))?;
    let v = json::parse(&raw).map_err(|e| format!("engine conf json: {e}"))?;
    v.get("api_key")
        .and_then(|x| x.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| "engine conf has no api_key".to_string())
}

fn engine_meta(path: &str) -> String {
    // Whitelisted, non-secret engine config for the meta event.
    let mut out = String::from("{");
    if let Ok(raw) = std::fs::read(path) {
        if let Ok(v) = json::parse(&raw) {
            let mut first = true;
            for k in [
                "model_name",
                "port",
                "gpu",
                "gpus_asked",
                "reasoning_budget_tokens",
                "vision",
                "host",
            ] {
                if let Some(x) = v.get(k) {
                    if !first {
                        out.push(',');
                    }
                    first = false;
                    out.push_str(&format!("\"{}\":{}", k, json::render(x)));
                }
            }
        }
    }
    out.push('}');
    out
}

// ------------------------------------------------------------------- window
struct Window {
    id: String,
    start_ts: f64,
    req_before: i64,
    passes: u64,
    records: u64,
    positions: u64,
    byte_start: u64,
    byte_end: u64,
    first_seq: i64,
    last_seq: i64,
    last_activity: Instant,
    pending: Option<(i64, i64, Instant)>, // req_after, delta, detected_at
}

// --------------------------------------------------------------------- core
struct Core {
    cfg: Cfg,
    tailer: Tailer,
    store: Option<Store>,
    seq: i64,
    run_id: String,
    subs: Arc<Mutex<Vec<Arc<Sub>>>>,
    metrics: Metrics,
    req_total_last: i64,
    win: Option<Window>,
    win_count: u64,
    events_total: u64,
    sessions_total: u64,
    unattributed_requests: u64,
    gap_bytes_total: u64,
    skipped_emitted: u64,
    resync_emitted: u64,
    db_ok: bool,
    db_err: String,
    last_commit_ms: f64,
    t0: Instant,
    last_metrics: Instant,
    last_status: Instant,
    last_hw: Instant,
    recovery_active: bool,
    recovery_gaps: u64,
    recovery_skips: u64,
    recovery_passes: u64,
    recovery_bytes: u64,
    last_rates: (Instant, u64, u64),
    event_rate: f64,
    record_rate: f64,
    byte_rate: f64,
    engine_errors: u64,
    reg_rx: std::sync::mpsc::Receiver<Arc<Sub>>,
    last_finished: Option<String>,
    /// The exact `meta` line this run emitted. Every subscriber that connects
    /// later gets this line as the first entry of its replay preload, so a late
    /// client can learn the model geometry without reading the DB or guessing.
    /// Additive to the v1 contract: the line is byte-identical to the one in the
    /// store, and a subscriber that was already attached never sees a second one.
    meta_line: Option<String>,
    /// Same contract for the one-shot residency ESTIMATE: the line is recorded
    /// once and handed to every later subscriber, labelled estimate, so a late
    /// client can paint the structural tint without reading the DB.
    res_line: Option<String>,
    /// JSON fragment describing the retention cut applied at startup (days,
    /// cutoff, replay floor, expired counts). Emitted in every
    /// `telemetry_status` so a client can say *why* its replay got shorter.
    retention_json: String,
}

impl Core {
    fn emit(&mut self, kind: &'static str, session: Option<&str>, droppable: bool, body: &str) -> i64 {
        self.seq += 1;
        let seq = self.seq;
        let ts = now_unix();
        let mut line = String::with_capacity(256 + body.len());
        line.push_str(&format!(
            "{{\"v\":{},\"seq\":{},\"ts\":{:.3},\"type\":\"{}\"",
            SCHEMA_V, seq, ts, kind
        ));
        if let Some(s) = session {
            line.push_str(&format!(",\"session\":\"{}\"", json::esc(s)));
        }
        line.push_str(body);
        line.push('}');
        if kind == "meta" {
            self.meta_line = Some(line.clone());
        }
        if kind == "residency_estimate" {
            self.res_line = Some(line.clone());
        }
        self.events_total += 1;
        {
            let subs = self.subs.lock().unwrap_or_else(|p| p.into_inner());
            for s in subs.iter() {
                s.push(seq, line.clone(), droppable);
            }
        }
        if let Some(st) = self.store.as_mut() {
            if let Err(e) = st.insert_event(seq, session, ts, kind, &line) {
                self.db_ok = false;
                self.db_err = e;
            }
        }
        seq
    }

    fn open_window(&mut self, byte_start: u64) {
        if self.win.is_some() {
            return;
        }
        self.win_count += 1;
        let id = format!(
            "s{}-{}-w{}",
            self.req_total_last,
            (now_unix() * 1000.0) as i64,
            self.win_count
        );
        let ts = now_unix();
        let first_seq = self.seq + 1;
        self.win = Some(Window {
            id: id.clone(),
            start_ts: ts,
            req_before: self.req_total_last,
            passes: 0,
            records: 0,
            positions: 0,
            byte_start,
            byte_end: byte_start,
            first_seq,
            last_seq: first_seq,
            last_activity: Instant::now(),
            pending: None,
        });
        self.sessions_total += 1;
        let body = format!(
            ",\"requests_before\":{},\"tier_hint\":\"windowed\",\"trace_byte_start\":{},\"run_id\":\"{}\",\"engine_state\":\"{}\"",
            self.req_total_last, byte_start, json::esc(&self.run_id), json::esc(&self.metrics.state)
        );
        self.emit("session_started", Some(&id), false, &body);
        if let Some(st) = self.store.as_mut() {
            if let Err(e) = st.session_start(&id, &self.run_id, ts, self.req_total_last, "windowed", byte_start, first_seq) {
                self.db_ok = false;
                self.db_err = e;
            }
        }
    }

    fn finish_window(&mut self, end_reason: &str, req_after: i64, delta: i64, tier: &'static str) {
        let w = match self.win.take() {
            Some(w) => w,
            None => return,
        };
        self.last_finished = Some(w.id.clone());
        let ts = now_unix();
        let dur = ts - w.start_ts;
        let body = format!(
            ",\"requests_before\":{},\"requests_after\":{},\"request_delta\":{},\"tier\":\"{}\",\"end_reason\":\"{}\",\"passes\":{},\"records\":{},\"positions\":{},\"trace_byte_start\":{},\"trace_byte_end\":{},\"first_seq\":{},\"last_seq\":{},\"duration_s\":{:.3}",
            w.req_before, req_after, delta, tier, end_reason, w.passes, w.records, w.positions,
            w.byte_start, w.byte_end, w.first_seq, w.last_seq, dur
        );
        self.emit("session_finished", Some(&w.id), false, &body);
        if let Some(st) = self.store.as_mut() {
            let r = st
                .session_touch(&w.id, w.passes, w.records, w.positions, w.byte_end, w.last_seq)
                .and_then(|_| st.session_finish(&w.id, ts, req_after, delta, tier, end_reason));
            if let Err(e) = r {
                self.db_ok = false;
                self.db_err = e;
            }
        }
        println!(
            "{{\"log\":\"session_finished\",\"session\":\"{}\",\"tier\":\"{}\",\"end_reason\":\"{}\",\"passes\":{},\"records\":{},\"positions\":{},\"request_delta\":{}}}",
            w.id, tier, end_reason, w.passes, w.records, w.positions, delta
        );
        let _ = std::io::stdout().flush();
    }

    fn emit_gap(
        &mut self,
        reason: &str,
        session: Option<&str>,
        byte_start: Option<u64>,
        byte_end: Option<u64>,
        bytes: u64,
        records_unattributed: u64,
        detail: &str,
    ) {
        self.gap_bytes_total += bytes;
        let body = format!(
            ",\"reason\":\"{}\",\"byte_start\":{},\"byte_end\":{},\"bytes\":{},\"records_unattributed\":{},\"detail\":\"{}\"",
            json::esc(reason),
            byte_start.map(|v| v.to_string()).unwrap_or_else(|| "null".into()),
            byte_end.map(|v| v.to_string()).unwrap_or_else(|| "null".into()),
            bytes,
            records_unattributed,
            json::esc(detail)
        );
        let ts = now_unix();
        self.emit("gap", session, false, &body);
        if let Some(st) = self.store.as_mut() {
            if let Err(e) = st.insert_gap(
                ts,
                session,
                reason,
                byte_start,
                byte_end,
                bytes,
                records_unattributed,
                detail,
            ) {
                self.db_ok = false;
                self.db_err = e;
            }
        }
        println!(
            "{{\"log\":\"gap\",\"reason\":\"{}\",\"bytes\":{},\"detail\":\"{}\"}}",
            json::esc(reason),
            bytes,
            json::esc(detail)
        );
        let _ = std::io::stdout().flush();
    }

    fn poll_metrics(&mut self, key: &str) {
        let m = fetch_metrics(&self.cfg, key);
        if !m.ok {
            self.engine_errors += 1;
            self.metrics = Metrics {
                ok: false,
                err: m.err.clone(),
                ts: m.ts,
                ..Default::default()
            };
            let sess = self.win.as_ref().map(|w| w.id.clone());
            self.emit(
                "perf_sample",
                sess.as_deref(),
                false,
                &format!(",\"engine_ok\":false,\"error\":\"{}\"", json::esc(&m.err)),
            );
            return;
        }
        // First successful poll only sets the baseline: the engine has been
        // running for hours, so its lifetime counters must never be mistaken
        // for activity during this collector run.
        if self.req_total_last < 0 {
            self.req_total_last = m.requests;
            let body = format!(
                ",\"engine_ok\":true,\"baseline\":true,\"totals\":{{\"requests\":{},\"prompt_tokens\":{},\"output_tokens\":{},\"drafts_offered\":{},\"drafts_accepted\":{}}},\"live\":{{\"state\":\"{}\",\"tok_s\":{}}}",
                m.requests,
                m.prompt_tokens,
                m.output_tokens,
                m.drafts_offered,
                m.drafts_accepted,
                json::esc(&m.state),
                m.tok_s.map(|v| format!("{v:.3}")).unwrap_or_else(|| "null".into())
            );
            let sessid = self.win.as_ref().map(|w| w.id.clone());
            self.emit("perf_sample", sessid.as_deref(), false, &body);
            if let Some(st) = self.store.as_mut() {
                if let Err(e) = st.insert_perf(
                    sessid.as_deref(),
                    m.ts,
                    m.requests,
                    m.prompt_tokens,
                    m.output_tokens,
                    m.drafts_offered,
                    m.drafts_accepted,
                    Some(m.state.as_str()),
                    m.tok_s,
                ) {
                    self.db_ok = false;
                    self.db_err = e;
                }
            }
            self.metrics = m;
            return;
        }
        let d_req = m.requests - self.req_total_last;
        let prev = self.metrics.clone();
        let d_out = m.output_tokens - prev.output_tokens;
        let body = format!(
            ",\"engine_ok\":true,\"totals\":{{\"requests\":{},\"prompt_tokens\":{},\"output_tokens\":{},\"drafts_offered\":{},\"drafts_accepted\":{}}},\"delta\":{{\"requests\":{},\"output_tokens\":{},\"prompt_tokens\":{},\"drafts_offered\":{}}},\"live\":{{\"state\":\"{}\",\"tok_s\":{}}}",
            m.requests,
            m.prompt_tokens,
            m.output_tokens,
            m.drafts_offered,
            m.drafts_accepted,
            d_req,
            d_out,
            m.prompt_tokens - prev.prompt_tokens,
            m.drafts_offered - prev.drafts_offered,
            json::esc(&m.state),
            m.tok_s.map(|v| format!("{v:.3}")).unwrap_or_else(|| "null".into())
        );
        let sessid = self.win.as_ref().map(|w| w.id.clone());
        self.emit("perf_sample", sessid.as_deref(), false, &body);
        if let Some(st) = self.store.as_mut() {
            if let Err(e) = st.insert_perf(
                sessid.as_deref(),
                m.ts,
                m.requests,
                m.prompt_tokens,
                m.output_tokens,
                m.drafts_offered,
                m.drafts_accepted,
                Some(m.state.as_str()),
                m.tok_s,
            ) {
                self.db_ok = false;
                self.db_err = e;
            }
        }
        // ---- session window bookkeeping, driven by the engine counter
        if m.requests < self.req_total_last {
            self.emit_gap(
                "engine_counter_reset",
                None,
                None,
                None,
                0,
                0,
                &format!(
                    "engine totals.requests {} -> {} (engine restart); window attribution restarts",
                    self.req_total_last, m.requests
                ),
            );
            if self.win.is_some() {
                self.finish_window("engine_counter_reset", m.requests, 0, "unattributed");
            }
            self.req_total_last = m.requests;
        } else if d_req > 0 {
            self.req_total_last = m.requests;
            if let Some(w) = self.win.as_mut() {
                if w.pending.is_none() {
                    w.pending = Some((m.requests, d_req, Instant::now()));
                } else if let Some(p) = w.pending.as_mut() {
                    p.1 += d_req;
                    p.0 = m.requests;
                }
            } else {
                // Requests the engine served while we saw no routing activity:
                // counted, never turned into an invented session.
                self.unattributed_requests += d_req as u64;
            }
        }
        self.metrics = m;
    }

    fn poll_hw(&mut self) {
        let mut gpu_util = None;
        let mut gpu_mem = None;
        let mut gpu_temp = None;
        let mut gpu_power = None;
        let mut gpu_name = String::new();
        if let Ok(out) = process::Command::new("nvidia-smi")
            .args([
                "--query-gpu=name,utilization.gpu,memory.used,memory.total,temperature.gpu,power.draw",
                "--format=csv,noheader,nounits",
            ])
            .output()
        {
            if out.status.success() {
                let s = String::from_utf8_lossy(&out.stdout);
                if let Some(line) = s.lines().next() {
                    let f: Vec<&str> = line.split(',').map(|x| x.trim()).collect();
                    if f.len() >= 6 {
                        gpu_name = f[0].to_string();
                        gpu_util = f[1].parse::<f64>().ok();
                        gpu_mem = f[2].parse::<i64>().ok();
                        gpu_temp = f[4].parse::<f64>().ok();
                        gpu_power = f[5].parse::<f64>().ok();
                    }
                }
            }
        }
        let load1 = std::fs::read_to_string("/proc/loadavg")
            .ok()
            .and_then(|s| s.split_whitespace().next().and_then(|x| x.parse::<f64>().ok()));
        let mem_avail = std::fs::read_to_string("/proc/meminfo").ok().and_then(|s| {
            s.lines().find(|l| l.starts_with("MemAvailable:")).and_then(|l| {
                l.split_whitespace().nth(1).and_then(|v| v.parse::<i64>().ok())
            })
        });
        let rss_kib = std::fs::read_to_string("/proc/self/status").ok().and_then(|s| {
            s.lines().find(|l| l.starts_with("VmRSS:")).and_then(|l| {
                l.split_whitespace().nth(1).and_then(|v| v.parse::<i64>().ok())
            })
        });
        let mut body = String::from(",\"source\":\"nvidia-smi+/proc\"");
        body.push_str(",\"gpu\":{");
        let mut first = true;
        let push = |k: &str, v: String, b: &mut String, f: &mut bool| {
            if !*f {
                b.push(',');
            }
            *f = false;
            b.push_str(&format!("\"{k}\":{v}"));
        };
        if !gpu_name.is_empty() {
            push("name", format!("\"{}\"", json::esc(&gpu_name)), &mut body, &mut first);
        }
        if let Some(v) = gpu_util {
            push("util_pct", format!("{v}"), &mut body, &mut first);
        }
        if let Some(v) = gpu_mem {
            push("mem_used_mib", format!("{v}"), &mut body, &mut first);
        }
        if let Some(v) = gpu_temp {
            push("temp_c", format!("{v}"), &mut body, &mut first);
        }
        if let Some(v) = gpu_power {
            push("power_w", format!("{v}"), &mut body, &mut first);
        }
        body.push('}');
        body.push_str(",\"cpu\":{");
        if let Some(v) = load1 {
            body.push_str(&format!("\"load1\":{v}"));
        }
        body.push('}');
        body.push_str(",\"mem\":{");
        if let Some(v) = mem_avail {
            body.push_str(&format!("\"available_mib\":{}", v / 1024));
        }
        body.push('}');
        body.push_str(",\"proc\":{");
        if let Some(v) = rss_kib {
            body.push_str(&format!("\"rss_kib\":{v}"));
        }
        body.push('}');
        let ts = now_unix();
        self.emit("hw_sample", None, false, &body);
        if let Some(st) = self.store.as_mut() {
            if let Err(e) = st.insert_hw(ts, gpu_util, gpu_mem, gpu_temp, gpu_power, load1, mem_avail.map(|v| v / 1024)) {
                self.db_ok = false;
                self.db_err = e;
            }
        }
    }

    fn subscriber_snapshot(&self) -> String {
        let subs = self.subs.lock().unwrap_or_else(|p| p.into_inner());
        let mut parts: Vec<String> = Vec::new();
        for s in subs.iter() {
            let (depth, drops, forced, sent) = s.depth_drops();
            let (max_depth, replayed, trunc, bytes_out, closed, peer, since) = {
                let (m, _) = &*s.st;
                let g = m.lock().unwrap_or_else(|p| p.into_inner());
                (
                    g.max_depth,
                    g.replayed,
                    g.replay_truncated,
                    g.bytes_out,
                    g.closed,
                    g.peer.clone(),
                    g.registered_ts,
                )
            };
            parts.push(format!(
                "{{\"id\":\"{}\",\"conn\":\"{}\",\"sub_id\":{},\"since\":{:.3},\"depth\":{},\"max_depth\":{},\"sent\":{},\"dropped_droppable\":{},\"forced_drops\":{},\"replayed\":{},\"replay_truncated\":{},\"bytes_out\":{},\"closed\":{}}}",
                s.name, json::esc(&peer), s.id, since, depth, max_depth, sent, drops, forced, replayed, trunc, bytes_out, closed
            ));
        }
        format!("[{}]", parts.join(","))
    }

    fn emit_status(&mut self) {
        let now = Instant::now();
        let (t_last, e_last, b_last) = self.last_rates;
        let dt = now.duration_since(t_last).as_secs_f64().max(1e-3);
        self.event_rate = ((self.events_total - e_last) as f64) / dt;
        self.record_rate = ((self.tailer.records_in_passes - b_last) as f64) / dt;
        self.byte_rate = ((self.tailer.bytes_read) as f64) / self.t0.elapsed().as_secs_f64().max(1e-3);
        self.last_rates = (now, self.events_total, self.tailer.records_in_passes);

        let per_sub = self.subscriber_snapshot();
        let nsubs = {
            let subs = self.subs.lock().unwrap_or_else(|p| p.into_inner());
            subs.iter()
                .filter(|s| {
                    let (m, _) = &*s.st;
                    !m.lock().unwrap_or_else(|p| p.into_inner()).closed
                })
                .count()
        };
        // drops are reported per subscriber; the total is their sum
        let mut drops_total = 0u64;
        {
            let subs = self.subs.lock().unwrap_or_else(|p| p.into_inner());
            for s in subs.iter() {
                let (_, d, f, _) = s.depth_drops();
                drops_total += d + f;
            }
        }
        let db_block = match self.store.as_ref() {
            Some(st) => format!(
                "{{\"ok\":{},\"enabled\":true,\"wal_bytes\":{},\"max_seq\":{},\"last_commit_ms\":{:.2},\"error\":\"{}\"}}",
                self.db_ok,
                st.wal_bytes(),
                st.max_seq(),
                self.last_commit_ms,
                json::esc(&self.db_err)
            ),
            None => "{\"ok\":false,\"enabled\":false,\"error\":\"--no-persist\"}".to_string(),
        };
        let body = format!(
            ",\"run_id\":\"{}\",\"pid\":{},\"uptime_s\":{:.1},\"events_total\":{},\"sessions_total\":{},\"rates\":{{\"events_per_s\":{:.1},\"records_per_s\":{:.1},\"bytes_per_s\":{:.0}}},\"subscribers\":{},\"per_subscriber\":{},\"queue_cap\":{},\"drops_total\":{},\"skipped_bytes_total\":{},\"skipped_bytes_since_last_event\":{},\"resyncs_total\":{},\"discarded_passes\":{},\"trace\":{{\"offset\":{},\"anchor\":{},\"held_records\":{},\"file_size\":{},\"path\":\"{}\"}},\"gap_bytes_total\":{},\"db\":{},\"engine\":{{\"ok\":{},\"requests_total\":{},\"state\":\"{}\",\"errors\":{}}},\"session_active\":{},\"unattributed_requests\":{},\"replay_max\":{},\"retention\":{}",
            json::esc(&self.run_id),
            process::id(),
            self.t0.elapsed().as_secs_f64(),
            self.events_total,
            self.sessions_total,
            self.event_rate,
            self.record_rate,
            self.byte_rate,
            nsubs,
            per_sub,
            self.cfg.queue_cap,
            drops_total,
            self.tailer.skipped_bytes,
            self.skipped_emitted,
            self.tailer.resyncs,
            self.tailer.discarded,
            self.tailer.cursor_offset(),
            self.tailer.anchor(),
            self.tailer.held_records(),
            self.tailer.file_size(),
            json::esc(&self.cfg.path),
            self.gap_bytes_total,
            db_block,
            self.metrics.ok,
            self.metrics.requests,
            json::esc(&self.metrics.state),
            self.engine_errors,
            match self.win.as_ref() {
                Some(w) => format!("\"{}\"", json::esc(&w.id)),
                None => "null".into(),
            },
            self.unattributed_requests,
            self.cfg.replay_max,
            self.retention_json
        );
        let sessid = self.win.as_ref().map(|w| w.id.clone());
        self.emit("telemetry_status", sessid.as_deref(), false, &body);
        if let Some(st) = self.store.as_mut() {
            let qmax = {
                let subs = self.subs.lock().unwrap_or_else(|p| p.into_inner());
                subs.iter().map(|s| s.depth_drops().0 as i64).max().unwrap_or(0)
            };
            if let Err(e) = st.insert_status(
                now_unix(),
                self.events_total as i64,
                nsubs as i64,
                drops_total as i64,
                self.tailer.skipped_bytes as i64,
                self.db_ok,
                qmax,
            ) {
                self.db_ok = false;
                self.db_err = e;
            }
        }
    }

    fn handle_passes(&mut self, passes: Vec<trace::Pass>) {
        for p in passes {
            self.open_window(p.start_off);
            let wid = self.win.as_ref().unwrap().id.clone();
            let pass_index = {
                let w = self.win.as_mut().unwrap();
                w.passes
            };
            let skipped_since = self.tailer.skipped_bytes - self.skipped_emitted;
            self.skipped_emitted = self.tailer.skipped_bytes;
            let resyncs_since = self.tailer.resyncs - self.resync_emitted;
            self.resync_emitted = self.tailer.resyncs;
            let k = p.recs[0].k;
            let body = format!(
                ",\"pass_index\":{},\"byte_start\":{},\"byte_end\":{},\"positions_count\":{},\"records\":{},\"layers\":{},\"k\":{},\"skipped_bytes_since_last\":{},\"resyncs_since_last\":{}",
                pass_index,
                p.start_off,
                p.end_off,
                p.positions,
                p.recs.len(),
                trace::EXPECTED_LAYERS,
                k,
                skipped_since,
                resyncs_since
            );
            let pseq = self.emit("pass_routed", Some(&wid), false, &body);
            {
                let w = self.win.as_mut().unwrap();
                w.passes += 1;
                w.records += p.recs.len() as u64;
                w.positions += p.positions as u64;
                w.byte_end = p.end_off;
                w.last_seq = pseq;
                w.last_activity = Instant::now();
                if self.recovery_active {
                    self.recovery_passes += 1;
                }
            }
            if let Some(st) = self.store.as_mut() {
                let r = st.insert_pass(&PassRow {
                    session: &wid,
                    pass_index,
                    byte_start: p.start_off,
                    byte_end: p.end_off,
                    records: p.recs.len() as u64,
                    positions: p.positions as u64,
                    layers: trace::EXPECTED_LAYERS as u64,
                    k: k as i64,
                    skipped_bytes: skipped_since,
                    resyncs: resyncs_since,
                    seq: pseq,
                });
                if let Err(e) = r {
                    self.db_ok = false;
                    self.db_err = e;
                }
            }
            for (idx, r) in p.recs.iter().enumerate() {
                let position = (idx / trace::EXPECTED_LAYERS as usize) as u64;
                let ids = r
                    .ids
                    .iter()
                    .map(|v| v.to_string())
                    .collect::<Vec<_>>()
                    .join(",");
                let ws = r
                    .weights
                    .iter()
                    .map(|v| format!("{v:?}"))
                    .collect::<Vec<_>>()
                    .join(",");
                let body = format!(
                    ",\"pass_index\":{},\"position\":{},\"layer\":{},\"k\":{},\"expert_ids\":[{}],\"weights\":[{}],\"byte_offset\":{},\"byte_len\":{}",
                    pass_index,
                    position,
                    r.layer,
                    r.k,
                    ids,
                    ws,
                    r.off,
                    8 + 8 * r.k as u64
                );
                // expert_selected is the only droppable type (bounded queues).
                let eseq = self.emit("expert_selected", Some(&wid), true, &body);
                if let Some(st) = self.store.as_mut() {
                    if let Err(e) = st.insert_routing(
                        &wid,
                        pass_index,
                        position,
                        r.layer,
                        r.k,
                        &r.ids,
                        &r.weights,
                        r.off,
                        eseq,
                        now_unix(),
                    ) {
                        self.db_ok = false;
                        self.db_err = e;
                    }
                }
            }
            {
                let w = self.win.as_mut().unwrap();
                w.last_seq = self.seq;
            }
        }
    }

    /// Drop subscribers that disconnected more than a minute ago. Their final
    /// counters were already reported in at least one telemetry_status.
    fn prune_subs(&mut self) {
        let mut subs = self.subs.lock().unwrap_or_else(|p| p.into_inner());
        subs.retain(|s| {
            let (m, _) = &*s.st;
            let g = m.lock().unwrap_or_else(|p| p.into_inner());
            match g.closed_at {
                Some(t) => t.elapsed() < Duration::from_secs(60),
                None => true,
            }
        });
    }

    /// Build the replay preload for subscribers that connected since the last
    /// loop iteration. Runs on the producer thread, which is the only writer, so
    /// the snapshot is exactly "everything emitted so far" — no event can fall
    /// between the replay and the live stream.
    fn handle_registrations(&mut self) {
        let mut batch: Vec<Arc<Sub>> = Vec::new();
        while let Ok(s) = self.reg_rx.try_recv() {
            batch.push(s);
        }
        for sub in batch {
            let sess = self
                .win
                .as_ref()
                .map(|w| w.id.clone())
                .or_else(|| self.last_finished.clone());
            let mut lines: Vec<String> = Vec::new();
            let mut floor: i64 = 0;
            let mut trunc: u64 = 0;
            if let (Some(st), Some(s)) = (self.store.as_ref(), sess.as_ref()) {
                if let Ok((pairs, total)) = st.tail(s, self.cfg.replay_max) {
                    floor = pairs.last().map(|(q, _)| *q).unwrap_or(0);
                    trunc = total.saturating_sub(pairs.len() as u64);
                    lines = pairs.into_iter().map(|(_, l)| l).collect();
                }
            }
            // meta-on-connect (additive): a subscriber that attaches after the
            // collector started never saw the startup `meta` event. Hand it the
            // stored line first — exactly the bytes that were recorded, counted
            // in this subscriber's replayed total.
            // A subscriber can attach in the window between socket accept and
            // the startup `meta`/`residency` emits. Fall back to the stored line
            // so the preload is never empty just because we were early.
            if self.meta_line.is_none() {
                let m = self.store.as_ref().and_then(|st| st.last_event_of_kind("meta"));
                if let Some(m) = m {
                    self.meta_line = Some(m);
                }
            }
            if self.res_line.is_none() {
                let r = self.store.as_ref().and_then(|st| st.last_event_of_kind("residency_estimate"));
                if let Some(r) = r {
                    self.res_line = Some(r);
                }
            }
            if let Some(ml) = self.meta_line.clone() {
                let already = lines.iter().any(|l| l.contains("\"type\":\"meta\""));
                if !already {
                    lines.insert(0, ml);
                    eprintln!("subscriber {}: meta-on-connect ({} bytes)", sub.name, lines[0].len());
                }
            }
            if let Some(rl) = self.res_line.clone() {
                let already = lines.iter().any(|l| l.contains("residency_estimate"));
                if !already {
                    let at = if lines.first().map(|l| l.contains("\"type\":\"meta\"")).unwrap_or(false) { 1 } else { 0 };
                    lines.insert(at, rl);
                }
            }
            // Gaps that happened before this subscriber attached (a restart, a
            // discarded pass). Additive like meta-on-connect: the bytes are the
            // recorded ones, and a fresh subscriber sees at most 8 of them.
            if let Some(st) = self.store.as_ref() {
                if let Ok(gaps) = st.recent_gaps(8) {
                    let mut at = 1usize.min(lines.len());
                    for g in gaps {
                        if lines.iter().any(|l| l == &g) {
                            continue;
                        }
                        lines.insert(at, g);
                        at += 1;
                    }
                    if at > 1 {
                        eprintln!("subscriber {}: {} recent gap event(s) on connect", sub.name, at - 1);
                    }
                }
            }
            if !lines.is_empty() || trunc > 0 {
                eprintln!(
                    "subscriber {}: preload {} line(s) of {:?}, truncated {}, floor {}",
                    sub.name,
                    lines.len(),
                    sess,
                    trunc,
                    floor
                );
            }
            sub.set_preload(lines, floor, trunc);
        }
    }

    fn checkpoint(&mut self) {
        let off = self.tailer.cursor_offset();
        let anchor = self.tailer.anchor();
        let ts = now_unix();
        // Keep the session aggregates current in the same transaction as the
        // pass rows, so a kill -9 leaves an honest session row for the window
        // that was in flight instead of zeros.
        let win = self
            .win
            .as_ref()
            .map(|w| (w.id.clone(), w.passes, w.records, w.positions, w.byte_end, w.last_seq));
        if let Some(st) = self.store.as_mut() {
            let _ = st.set_cursor(&self.cfg.path, off, anchor, &self.run_id, ts);
            if let Some((id, p, r, pos, be, ls)) = win {
                let _ = st.session_touch(&id, p, r, pos, be, ls);
            }
        }
    }
}


/// `size_cap` fragment for the maintenance output (escape-free by construction).
fn size_cap_json(cap: &db::SizeEvict) -> String {
    let mut o = String::with_capacity(256);
    let kv = |o: &mut String, k: &str, v: &str, first: bool| {
        if !first {
            o.push(',');
        }
        o.push('"');
        o.push_str(k);
        o.push('"');
        o.push(':');
        o.push_str(v);
    };
    o.push('{');
    kv(&mut o, "cap_bytes", &cap.cap_bytes.to_string(), true);
    kv(&mut o, "evicted", &cap.evicted.to_string(), false);
    kv(&mut o, "sessions_evicted", &cap.sessions.to_string(), false);
    kv(&mut o, "routing_evicted", &cap.routing.to_string(), false);
    kv(&mut o, "passes_evicted", &cap.passes.to_string(), false);
    kv(&mut o, "events_evicted", &cap.events.to_string(), false);
    kv(&mut o, "bytes_before", &cap.bytes_before.to_string(), false);
    kv(&mut o, "bytes_after", &cap.bytes_after.to_string(), false);
    kv(&mut o, "vacuumed", if cap.vacuumed { "true" } else { "false" }, false);
    kv(&mut o, "oldest_ts_after",
       &cap.oldest_ts.map(|v| format!("{v:.3}")).unwrap_or_else(|| "null".into()), false);
    kv(&mut o, "floor_seq_after",
       &cap.floor_seq.map(|v| v.to_string()).unwrap_or_else(|| "null".into()), false);
    o.push('}');
    o
}

/// Build the `retention` JSON fragment advertised in every telemetry_status.
/// Written with push/push_str so the escapes stay readable: this block is the
/// honesty machinery for both policies (time window and size cap).
fn retention_json_of(pr: &db::Pruned, cfg: &Cfg, cap: &db::SizeEvict) -> String {
    let mut o = String::with_capacity(512);
    let kv = |o: &mut String, k: &str, v: &str, first: bool| {
        if !first {
            o.push(',');
        }
        o.push('"');
        o.push_str(k);
        o.push('"');
        o.push(':');
        o.push_str(v);
    };
    o.push('{');
    kv(&mut o, "days", &cfg.retention_days.to_string(), true);
    kv(&mut o, "cutoff_ts", &format!("{:.3}", pr.cutoff_ts), false);
    kv(&mut o, "floor_seq",
       &pr.floor_seq.map(|v| v.to_string()).unwrap_or_else(|| "null".into()), false);
    kv(&mut o, "oldest_ts",
       &pr.oldest_ts.map(|v| format!("{v:.3}")).unwrap_or_else(|| "null".into()), false);
    kv(&mut o, "expired_events", &pr.events.to_string(), false);
    kv(&mut o, "expired_sessions", &pr.sessions.to_string(), false);
    kv(&mut o, "expired_routing", &pr.routing.to_string(), false);
    kv(&mut o, "max_db_mb", &cfg.max_db_mb.to_string(), false);
    kv(&mut o, "size_cap_bytes", &cap.cap_bytes.to_string(), false);
    kv(&mut o, "size_evicted_sessions", &cap.sessions.to_string(), false);
    kv(&mut o, "size_evicted_events", &cap.events.to_string(), false);
    kv(&mut o, "size_evicted_routing", &cap.routing.to_string(), false);
    kv(&mut o, "size_evicted_oldest_ts",
       &cap.oldest_ts.map(|v| format!("{v:.3}")).unwrap_or_else(|| "null".into()), false);
    kv(&mut o, "db_bytes", &cap.bytes_after.max(pr.bytes_after).to_string(), false);
    // `--retention-days 0` disables the window (see Store::prune): replay is
    // then bounded only by the size cap, and saying so is the honest report.
    kv(&mut o, "replay_bounded_by_retention", if cfg.retention_days > 0 { "true" } else { "false" }, false);
    // strings must be quoted, unlike the numeric kv() values above
    o.push(',');
    o.push('"');
    o.push_str("note");
    o.push('"');
    o.push(':');
    o.push('"');
    o.push_str(&format!(
        "{} budgeted to {} MB; history shortened by time expiry{}; run --export to keep longer",
        if cfg.retention_days > 0 {
            format!("replay and export only see rows inside {} day(s) and", cfg.retention_days)
        } else {
            "time expiry disabled (--retention-days 0): replay sees all stored rows,".to_string()
        },
        cfg.max_db_mb,
        if cap.evicted > 0 { " and by the size cap" } else { "" }
    ));
    o.push('"');
    o.push('}');
    o
}

// ------------------------------------------------- Stage 7: offline maintenance
fn run_maintenance(cfg: &Cfg) -> ! {
    let mut parts: Vec<String> = Vec::new();
    if let Some(dir) = cfg.export.as_ref() {
        let st = match Store::open_readonly(&cfg.db) {
            Ok(s) => s,
            Err(e) => {
                println!("{{\"ok\":false,\"mode\":\"export\",\"error\":\"{}\"}}", json::esc(&e));
                process::exit(2);
            }
        };
        match st.export(dir) {
            Ok(ex) => parts.push(format!(
                "\"export\":{{\"ok\":true,\"dir\":\"{}\",\"events_lines\":{},\"reconstructed\":{},\"logical_events\":{},\"identity_equal\":{},\"sessions\":{},\"passes\":{},\"routing\":{},\"perf\":{},\"hw\":{},\"status\":{},\"gaps\":{},\"summary\":\"{}/summary.json\",\"reconstruction\":\"{}\"}}",
                json::esc(dir),
                ex.events_lines, ex.reconstructed, ex.logical_events, ex.identity_equal,
                ex.sessions, ex.passes, ex.routing, ex.perf, ex.hw, ex.status, ex.gaps,
                dir,
                if st.has_ts { "routing.ts/byte_len" } else { "unavailable (schema v1: no routing.ts)" }
            )),
            Err(e) => {
                println!("{{\"ok\":false,\"mode\":\"export\",\"error\":\"{}\"}}", json::esc(&e));
                process::exit(2);
            }
        }
    }
    if cfg.prune {
        let mut st = match Store::open(&cfg.db) {
            Ok(s) => s,
            Err(e) => {
                println!("{{\"ok\":false,\"mode\":\"prune\",\"error\":\"{}\"}}", json::esc(&e));
                process::exit(2);
            }
        };
        let before = st.counts();
        let before_b = st.db_bytes();
        let before_tables = st.table_bytes();
        let pr = match st.prune(cfg.retention_days, cfg.vacuum) {
            Ok(v) => v,
            Err(e) => {
                println!("{{\"ok\":false,\"mode\":\"prune\",\"error\":\"{}\"}}", json::esc(&e));
                process::exit(2);
            }
        };
        let comp = match st.compact_event_lines(cfg.event_line_hours, cfg.vacuum) {
            Ok(c) => c,
            Err(e) => {
                println!("{{\"ok\":false,\"mode\":\"prune\",\"error\":\"{}\"}}", json::esc(&e));
                process::exit(2);
            }
        };
        let cap = match st.enforce_size_cap((cfg.max_db_mb.max(0) as u64) * 1024 * 1024) {
            Ok(v) => v,
            Err(e) => {
                println!("{{\"ok\":false,\"mode\":\"prune\",\"error\":\"{}\"}}", json::esc(&e));
                process::exit(2);
            }
        };
        let after = st.counts();
        let after_b = st.db_bytes();
        let after_tables = st.table_bytes();
        let integrity = st.check_integrity();
        let tb = |v: &[(String, u64)]| -> String {
            let mut m = String::from("{");
            let mut first = true;
            for (n, b) in v {
                if !first {
                    m.push(',');
                }
                first = false;
                m.push_str(&format!("\"{}\":{}", json::esc(n), b));
            }
            m.push('}');
            m
        };
        parts.push(format!(
            "\"prune\":{{\"ok\":true,\"db\":\"{}\",\"retention_days\":{},\"event_line_hours\":{},\"vacuum\":{},\
\"hw_pruned\":{},\"status_pruned\":{},\"perf_pruned\":{},\"routing_pruned\":{},\"passes_pruned\":{},\"sessions_pruned\":{},\"events_pruned_other\":{},\"floor_seq\":{},\"freelist_bytes\":{},\"vacuumed\":{},\n
\"event_lines_compacted\":{},\"sessions_compacted\":{},\"sessions_skipped\":{},\"cutoff_ts\":{:.3},\
\"counts_before\":{{\"events\":{},\"expert_events\":{},\"sessions\":{},\"passes\":{},\"routing\":{},\"perf\":{},\"hw\":{},\"status\":{},\"gaps\":{}}},\
\"counts_after\":{{\"events\":{},\"expert_events\":{},\"sessions\":{},\"passes\":{},\"routing\":{},\"perf\":{},\"hw\":{},\"status\":{},\"gaps\":{}}},\
\"db_bytes_before\":{},\"db_bytes_after\":{},\"integrity\":\"{}\",\"table_bytes_before\":{},\"table_bytes_after\":{}}}",
            json::esc(&cfg.db), cfg.retention_days, cfg.event_line_hours, cfg.vacuum,
            pr.hw, pr.status, pr.perf, pr.routing, pr.passes, pr.sessions, pr.events,
            pr.floor_seq.map(|v| v.to_string()).unwrap_or_else(|| "null".into()),
            pr.freelist_bytes, pr.vacuumed,
            comp.lines_removed, comp.sessions, comp.sessions_skipped, comp.cutoff_ts,
            before.events, before.expert_events, before.sessions, before.passes, before.routing,
            before.perf, before.hw, before.status, before.gaps,
            after.events, after.expert_events, after.sessions, after.passes, after.routing,
            after.perf, after.hw, after.status, after.gaps,
            before_b, after_b, json::esc(&integrity), tb(&before_tables), tb(&after_tables)
        ));
        parts.push(format!("\"size_cap\":{}", size_cap_json(&cap)));
    }
    println!("{{\"tool\":\"matrix-collect maintenance\",\"version\":\"{}\",{}}}", VERSION, parts.join(","));
    let _ = std::io::stdout().flush();
    process::exit(0);
}

// ------------------------------------------- Stage 7: labelled residency estimate
fn residency_estimate(path: &str, slots: Option<i64>) -> String {
    let raw = match std::fs::read(path) {
        Ok(r) => r,
        Err(e) => {
            return format!(
                ",\"status\":\"unavailable\",\"basis\":\"estimate\",\"confirmed\":false,\"reason\":\"expert profile unreadable ({}); no per-expert residency exists in the v1 stream, so nothing is painted\"",
                json::esc(&format!("{e}"))
            )
        }
    };
    if raw.len() < 24 || &raw[0..4] != b"STRP" {
        return ",\"status\":\"unavailable\",\"basis\":\"estimate\",\"confirmed\":false,\"reason\":\"expert profile has no STRP header; the estimate is not invented\"".into();
    }
    let u32at = |o: usize| -> u64 { u32::from_le_bytes([raw[o], raw[o + 1], raw[o + 2], raw[o + 3]]) as u64 };
    let u16at = |o: usize| -> u64 { u16::from_le_bytes([raw[o], raw[o + 1]]) as u64 };
    let version = u32at(4);
    let layers = u32at(8);
    let experts = u32at(12);
    let ranked = u32at(20);
    // The ranked list is `n_ranked` records of u16 layer + u16 expert (4 bytes
    // each, hottest first); the file then carries a parallel array of u32
    // counts (24 + 24576*4 + 24576*4 = 196632 bytes on the shipped profile).
    // Same layout the legacy reader verified independently.
    let pairs = ((raw.len() - 24) / 4) as u64;
    let n = ranked.min(pairs);
    let slots = match slots {
        Some(s) if s > 0 => s as u64,
        _ => {
            return format!(
                ",\"status\":\"unavailable\",\"basis\":\"estimate\",\"confirmed\":false,\"profile\":{{\"path\":\"{}\",\"version\":{},\"layers\":{},\"experts\":{},\"ranked\":{}}},\"reason\":\"the engine's /metrics did not report engine.expert_slots, so the VRAM/RAM boundary cannot be placed\"",
                json::esc(path), version, layers, experts, n
            )
        }
    };
    let vram = slots.min(n);
    let mut cells = String::with_capacity((vram as usize) * 7);
    let mut counted = 0u64;
    for i in 0..vram {
        let o = 24 + (i as usize) * 4;
        let l = u16at(o);
        let e = u16at(o + 2);
        if l >= layers || e >= experts {
            continue;
        }
        if counted > 0 {
            cells.push(',');
        }
        cells.push_str(&(l * experts + e).to_string());
        counted += 1;
    }
    format!(
        ",\"status\":\"estimate\",\"basis\":\"estimate\",\"confirmed\":false,\"label\":\"residency_estimate\",\
\"source\":\"model structure: expert-profile.bin pairs ranked by routing frequency, prefix of length engine.expert_slots from the engine's own /metrics\",\
\"policy\":\"static profile-ordered, no eviction\",\
\"profile\":{{\"path\":\"{}\",\"version\":{},\"layers\":{},\"experts\":{},\"ranked\":{},\"pairs_read\":{}}},\
\"slots\":{},\"slots_source\":\"engine /metrics engine.expert_slots (auto-sized per boot)\",\
\"tiers\":{{\"vram\":{},\"ram\":{},\"ssd\":0}},\
\"ssd_note\":\"no per-expert SSD residency exists: the pinned RAM arena holds the whole expert corpus at startup; residual SSD/page-cache reads are per-request counters (file_blobs/file_mb), not residency\",\
\"index_base\":{},\"vram_cells\":[{}],\
\"note\":\"ESTIMATE from model structure, never live truth: the engine emits no per-expert residency events and its cache never evicts; aggregate /metrics cache numbers are not per-expert and are not used here\"",
        json::esc(path), version, layers, experts, n, pairs, slots, vram, n - vram, experts, cells
    )
}

// --------------------------------------------------------------------- main
fn main() {
    let cfg = match parse_args() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("argument error: {e}");
            process::exit(2);
        }
    };

    // ---- offline maintenance modes: no tailing, no socket, no writes to a
    // live database (export is strictly read-only; prune is meant for fixtures
    // or for a stopped collector)
    if cfg.prune || cfg.export.is_some() {
        run_maintenance(&cfg);
    }

    // ---- replay mode: no tailing, no socket, just stored NDJSON
    if let Some(sess) = cfg.replay.clone() {
        let store = match Store::open(&cfg.db) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("replay: {e}");
                process::exit(2);
            }
        };
        match store.replay_lines(&sess) {
            Ok(lines) => {
                if lines.is_empty() {
                    eprintln!("replay: no events for session {sess}");
                    process::exit(3);
                }
                let out = std::io::stdout();
                let mut lk = out.lock();
                for l in lines {
                    if writeln!(lk, "{l}").is_err() {
                        process::exit(4);
                    }
                }
                let _ = lk.flush();
                process::exit(0);
            }
            Err(e) => {
                eprintln!("replay: {e}");
                process::exit(2);
            }
        }
    }

    unsafe {
        signal(15, handle_signal); // SIGTERM
        signal(2, handle_signal); // SIGINT
    }

    let key = match read_engine_key(&cfg.engine_conf) {
        Ok(k) => k,
        Err(e) => {
            eprintln!("warning: {e}; /metrics will be unauthenticated");
            String::new()
        }
    };

    let run_id = format!(
        "r{}-{}",
        process::id(),
        (now_unix() * 1000.0) as i64
    );

    // ---- store
    let mut retention_json_out = String::from(
        "{\"days\":0,\"floor_seq\":null,\"oldest_ts\":null,\"expired_events\":0,\"expired_sessions\":0,\"replay_bounded_by_retention\":false,\"note\":\"--no-persist: nothing is stored, so nothing expires\"}",
    );
    let mut expired_events = 0usize;
    let mut expired_sessions = 0usize;
    let mut cap_out = db::SizeEvict::default();
    let mut store = if cfg.no_persist {
        None
    } else {
        match Store::open(&cfg.db) {
            Ok(mut s) => {
                let pr = s.prune(cfg.retention_days, false).unwrap_or_default();
                let mut comp = db::Compact::default();
                if cfg.event_line_hours > 0.0 {
                    match s.compact_event_lines(cfg.event_line_hours, false) {
                        Ok(c) => comp = c,
                        Err(e) => eprintln!("warning: event-line compaction failed: {e}"),
                    }
                }
                // Pages freed by the cut are only returned to the filesystem by
                // a VACUUM (this schema is auto_vacuum=NONE). Do it at startup,
                // once, and only when there is real space to win back.
                let mut vac = (0u64, 0u64);
                if pr.freelist_bytes >= db::VACUUM_THRESHOLD {
                    let before = s.db_bytes();
                    if s.vacuum().is_ok() {
                        vac = (before, s.db_bytes());
                    }
                }
                if pr.events + pr.sessions + pr.routing + pr.passes + pr.hw + pr.status + pr.perf > 0
                    || comp.lines_removed > 0
                {
                    println!(
                        "{{\"log\":\"retention\",\"days\":{},\"cutoff_ts\":{:.3},\"hw_pruned\":{},\"status_pruned\":{},\"perf_pruned\":{},\"routing_pruned\":{},\"passes_pruned\":{},\"sessions_pruned\":{},\"events_pruned\":{},\"floor_seq\":{},\"oldest_ts\":{},\"freelist_bytes\":{},\"vacuumed\":{},\"db_bytes_before\":{},\"db_bytes_after\":{},\"event_line_hours\":{},\"event_lines_compacted\":{},\"sessions_compacted\":{},\"sessions_skipped\":{}}}",
                        cfg.retention_days, pr.cutoff_ts, pr.hw, pr.status, pr.perf, pr.routing,
                        pr.passes, pr.sessions, pr.events,
                        pr.floor_seq.map(|v| v.to_string()).unwrap_or_else(|| "null".into()),
                        pr.oldest_ts.map(|v| format!("{v:.3}")).unwrap_or_else(|| "null".into()),
                        pr.freelist_bytes, vac.0 > 0, pr.bytes_before,
                        if vac.0 > 0 { vac.1 } else { pr.bytes_after },
                        cfg.event_line_hours, comp.lines_removed, comp.sessions, comp.sessions_skipped
                    );
                }
                // Order matters: time expiry first, then the size cap only if
                // the store is still over budget.
                let cap = s
                    .enforce_size_cap((cfg.max_db_mb.max(0) as u64) * 1024 * 1024)
                    .unwrap_or_default();
                cap_out = cap.clone();
                let retention_json = retention_json_of(&pr, &cfg, &cap);
                retention_json_out = retention_json;
                expired_events = pr.events + cap.events;
                expired_sessions = pr.sessions + cap.sessions;
                Some(s)
            }
            Err(e) => {
                eprintln!("db error: {e}");
                process::exit(5);
            }
        }
    };

    // ---- recover cursor / crash state
    let file_size = std::fs::metadata(&cfg.path).map(|m| m.len()).unwrap_or(0);
    let mut start_off = cfg.start_offset;
    let mut start_anchor = start_off.unwrap_or(file_size);
    let mut prev_cursor: Option<(String, u64, u64)> = None;
    if let Some(st) = store.as_ref() {
        let c = st.get_cursor();
        let last_run_unclean = st
            .conn
            .query_row(
                "SELECT end_reason FROM runs ORDER BY started_ts DESC LIMIT 1",
                [],
                |r| r.get::<_, Option<String>>(0),
            )
            .ok()
            .flatten()
            .is_none();
        if let Some((p, off, anchor)) = c.clone() {
            if p == cfg.path && start_off.is_none() {
                start_off = Some(off);
                start_anchor = anchor;
                prev_cursor = Some(c.unwrap());
            }
            if last_run_unclean && p == cfg.path {
                // Unfinished sessions from the crashed run: say so, don't guess.
                let mut ids: Vec<String> = Vec::new();
                if let Ok(mut q) = st
                    .conn
                    .prepare("SELECT id FROM sessions WHERE finished=0")
                {
                    if let Ok(it) = q.query_map([], |r| r.get::<_, String>(0)) {
                        for x in it.flatten() {
                            ids.push(x);
                        }
                    }
                }
                for id in &ids {
                    let _ = st.conn.execute(
                        "UPDATE sessions SET finished=1, end_reason='collector_crash', tier='unattributed' WHERE id=?1",
                        rusqlite::params![id],
                    );
                }
                if !ids.is_empty() {
                    eprintln!(
                        "recovered: {} unfinished session(s) from previous run marked collector_crash",
                        ids.len()
                    );
                }
            }
        }
    }
    let start = start_off.unwrap_or(file_size);

    // ---- tailer
    let mut tailer = Tailer::new(cfg.path.clone());
    tailer.resume_at(start, start_anchor);

    // ---- socket
    let listener = match net::bind_listener(&cfg.socket) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("socket error: {e}");
            process::exit(6);
        }
    };

    let subs: Arc<Mutex<Vec<Arc<Sub>>>> = Arc::new(Mutex::new(Vec::new()));
    let stop_flag = Arc::new(AtomicBool::new(false));
    let sub_seq = Arc::new(AtomicU64::new(1));
    // Subscribers are registered here and their replay preload is built by the
    // producer thread (the only DB writer), so the replay snapshot can never
    // miss an event that was emitted before the connection was accepted.
    let (reg_tx, reg_rx) = std::sync::mpsc::channel::<Arc<Sub>>();

    // Acceptor: one thread per subscriber.
    {
        let subs = subs.clone();
        let stop = stop_flag.clone();
        let queue_cap = cfg.queue_cap;
        let sub_seq = sub_seq.clone();
        let reg_tx = reg_tx.clone();
        thread::spawn(move || {
            let mut peer_idx = 0u64;
            while !stop.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _addr)) => {
                        peer_idx += 1;
                        let id = sub_seq.fetch_add(1, Ordering::SeqCst);
                        let name = format!("c{id}");
                        let ts = now_unix();
                        let sub = Arc::new(Sub::new(
                            id,
                            name.clone(),
                            format!("unix#{peer_idx}"),
                            queue_cap,
                            ts,
                        ));
                        subs.lock()
                            .unwrap_or_else(|p| p.into_inner())
                            .push(sub.clone());
                        let _ = reg_tx.send(sub.clone());
                        let stop2 = stop.clone();
                        thread::spawn(move || net::serve(stream, sub, stop2));
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(50));
                    }
                    Err(_) => thread::sleep(Duration::from_millis(50)),
                }
            }
        });
    }

    let mut core = Core {
        cfg: cfg.clone(),
        tailer,
        store: store.take(),
        seq: 0,
        run_id: run_id.clone(),
        subs: subs.clone(),
        metrics: Metrics::default(),
        req_total_last: -1,
        win: None,
        win_count: 0,
        events_total: 0,
        sessions_total: 0,
        unattributed_requests: 0,
        gap_bytes_total: 0,
        skipped_emitted: 0,
        resync_emitted: 0,
        db_ok: true,
        db_err: String::new(),
        last_commit_ms: 0.0,
        t0: Instant::now(),
        last_metrics: Instant::now() - Duration::from_secs(3600),
        last_status: Instant::now(),
        last_hw: Instant::now() - Duration::from_secs(3600),
        recovery_active: prev_cursor.is_some(),
        recovery_gaps: 0,
        recovery_skips: 0,
        recovery_passes: 0,
        recovery_bytes: 0,
        last_rates: (Instant::now(), 0, 0),
        event_rate: 0.0,
        record_rate: 0.0,
        byte_rate: 0.0,
        engine_errors: 0,
        reg_rx,
        last_finished: None,
        meta_line: None,
        res_line: None,
        retention_json: retention_json_out.clone(),
    };

    // If the retention cut removed rows, say so on the stream: subscribers that
    // reconnect will start at a higher seq than they last saw, and that jump is
    // a retention floor, not a collector bug. Emitted before the socket serves.
    if cap_out.evicted > 0 {
        let body = format!(
            ",\"reason\":\"retention_size_evict\",\"bytes\":0,\"records_unattributed\":0,\"detail\":\"{} session(s), {} routing row(s) and {} event(s) evicted oldest-first to fit the {} MB cap; replay floor is seq {}\"",
            cap_out.sessions,
            cap_out.routing,
            cap_out.events,
            cfg.max_db_mb,
            cap_out.floor_seq.map(|v| v.to_string()).unwrap_or_else(|| "null".into())
        );
        core.emit("gap", None, false, &body);
    }
    if expired_events + expired_sessions > 0 {
        let body = format!(
            ",\"reason\":\"retention_expiry\",\"bytes\":0,\"records_unattributed\":0,\"detail\":\"{} event(s) and {} session(s) older than {} day(s) were deleted; replay floor is seq {}\"",
            expired_events,
            expired_sessions,
            cfg.retention_days,
            core.store.as_ref().and_then(|st| st.retention_floor().0).map(|v| v.to_string()).unwrap_or_else(|| "null".into())
        );
        core.emit("gap", None, false, &body);
    }

    // Resume seq from storage so seq stays monotonic across restarts.
    if let Some(st) = core.store.as_ref() {
        core.seq = st.max_seq();
    }

    // First metrics poll establishes the counter baseline BEFORE any window can
    // open, otherwise the first delta would be attributed to the wrong window.
    // Retry briefly: if the engine is unreachable the baseline is taken by the
    // first successful poll in the main loop instead (req_total_last stays < 0).
    for _ in 0..3 {
        core.poll_metrics(&key);
        if core.metrics.ok {
            break;
        }
        thread::sleep(Duration::from_millis(300));
    }

    if let Some(st) = core.store.as_mut() {
        let argv: Vec<String> = env::args().collect();
        if let Err(e) = st.insert_run(&run_id, now_unix(), process::id(), &argv.join(" "), start) {
            core.db_ok = false;
            core.db_err = e;
        }
    }

    // ---- meta event (model geometry from the engine's own /metrics)
    let engine_json = match core.metrics.raw.as_ref() {
        Some(raw) => match json::parse(raw.as_bytes()) {
            Ok(v) => match v.get("engine") {
                Some(e) => json::render(e),
                None => "null".into(),
            },
            Err(_) => "null".into(),
        },
        None => "null".into(),
    };
    let mode = if prev_cursor.is_some() { "resume" } else { "cold" };
    let body = format!(
        ",\"collector\":{{\"version\":\"{}\",\"schema\":{},\"pid\":{},\"run_id\":\"{}\",\"mode\":\"{}\",\"argv\":\"{}\"}},\"engine\":{},\"engine_conf\":{},\"trace\":{{\"path\":\"{}\",\"layers\":{},\"k\":{},\"expert_id_range\":[0,{}],\"record_layout\":\"i32 layer, i32 k, k*i32 expert_id, k*f32 weight (LE)\",\"start_offset\":{},\"file_size\":{},\"resume_from\":{}}},\"transport\":{{\"socket\":\"{}\",\"queue_cap\":{},\"replay_max\":{}}},\"storage\":{{\"db\":\"{}\",\"enabled\":{},\"retention_days\":{},\"journal\":\"WAL\"}}",
        VERSION,
        SCHEMA_V,
        process::id(),
        json::esc(&run_id),
        mode,
        json::esc(&env::args().collect::<Vec<_>>().join(" ")),
        engine_json,
        engine_meta(&cfg.engine_conf),
        json::esc(&cfg.path),
        trace::EXPECTED_LAYERS,
        10,
        trace::MAX_EXPERTS - 1,
        start,
        file_size,
        prev_cursor.as_ref().map(|c| c.1).unwrap_or(start),
        json::esc(&cfg.socket),
        cfg.queue_cap,
        cfg.replay_max,
        json::esc(&cfg.db),
        !cfg.no_persist,
        cfg.retention_days
    );
    core.emit("meta", None, false, &body);

    // ---- residency estimate: STRUCTURE, not truth.
    // The engine exposes no per-expert residency: its cache pre-fills from a
    // static profile and never evicts, and /metrics reports aggregate slot and
    // byte counts only. What can be said honestly is derived from the model's
    // own STRP profile (pairs ranked by routing frequency) plus the engine's
    // own `expert_slots`: the first `slots` ranked pairs are the VRAM set, the
    // rest live in the pinned RAM arena. That is model structure, so every
    // field is labelled `estimate` and `confirmed:false`; the app must never
    // paint it as live truth.
    let slots = core
        .metrics
        .raw
        .as_ref()
        .and_then(|raw| json::parse(raw.as_bytes()).ok())
        .and_then(|v| v.path(&["engine", "expert_slots"]).and_then(|x| x.as_i64()));
    let res_body = residency_estimate(&cfg.expert_profile, slots);
    core.emit("residency_estimate", None, false, &res_body);
    println!("{{\"log\":\"residency_estimate\",\"status\":\"{}\"}}",
        if res_body.contains("\"status\":\"estimate\"") { "estimate" } else { "unavailable" });
    let _ = std::io::stdout().flush();

    // ---- restart / crash gap accounting
    if let Some((_, off, anchor)) = prev_cursor.clone() {
        let resumed_bytes = file_size.saturating_sub(off);
        core.recovery_bytes = resumed_bytes;
        let detail = format!(
            "resumed from persisted cursor offset {} (anchor {}); {} bytes available from cursor to EOF will be re-read, no events were invented",
            off, anchor, resumed_bytes
        );
        core.emit_gap("collector_restart", None, Some(off), Some(file_size), 0, 0, &detail);
    }

    println!(
        "{{\"log\":\"start\",\"run_id\":\"{}\",\"pid\":{},\"mode\":\"{}\",\"path\":\"{}\",\"socket\":\"{}\",\"db\":\"{}\",\"start_offset\":{},\"file_size\":{},\"schema\":{}}}",
        json::esc(&run_id),
        process::id(),
        mode,
        json::esc(&cfg.path),
        json::esc(&cfg.socket),
        json::esc(&cfg.db),
        start,
        file_size,
        SCHEMA_V
    );
    let _ = std::io::stdout().flush();

    // ------------------------------------------------------------- main loop
    let mut passes: Vec<trace::Pass> = Vec::new();
    let mut gaps: Vec<trace::TraceGap> = Vec::new();
    let mut final_reason = "shutdown";

    while !STOP.load(Ordering::SeqCst) {
        let loop_start = Instant::now();
        if let Some(st) = core.store.as_mut() {
            if let Err(e) = st.begin() {
                core.db_ok = false;
                core.db_err = e;
            }
        }

        // Replay preloads for connections accepted since the last iteration.
        core.handle_registrations();

        if core.last_metrics.elapsed() >= Duration::from_millis(cfg.metrics_ms) {
            core.poll_metrics(&key);
            core.last_metrics = Instant::now();
        }
        if core.last_hw.elapsed() >= Duration::from_millis(cfg.hw_ms) {
            core.poll_hw();
            core.last_hw = Instant::now();
        }

        passes.clear();
        gaps.clear();
        core.tailer.poll(&mut passes, &mut gaps);
        for g in gaps.drain(..) {
            let sess = core.win.as_ref().map(|w| w.id.clone());
            core.emit_gap(
                g.reason,
                sess.as_deref(),
                Some(g.byte_start),
                Some(g.byte_end),
                g.bytes,
                g.bytes / 88,
                &g.detail,
            );
            if core.recovery_active {
                core.recovery_gaps += 1;
                core.recovery_skips += g.bytes;
            }
        }
        if !passes.is_empty() {
            let batch = std::mem::take(&mut passes);
            core.handle_passes(batch);
            if core.recovery_active && core.recovery_passes > 0 {
                core.recovery_active = false;
            }
        }

        // ---- window close: counter delta seen AND trace went quiet
        let mut do_finish: Option<(String, i64, i64, &'static str)> = None;
        let mut idle_close = false;
        if let Some(w) = core.win.as_mut() {
            if let Some((after, delta, at)) = w.pending {
                let quiet = w.last_activity.elapsed() >= Duration::from_millis(cfg.quiet_ms);
                let stuck = at.elapsed() >= Duration::from_secs(8);
                if quiet || stuck {
                    do_finish = Some(("counter".to_string(), after, delta, "windowed"));
                }
            } else if w.last_activity.elapsed() >= Duration::from_secs(30) {
                idle_close = true;
            }
        }
        if let Some((_why, after, delta, tier)) = do_finish {
            core.finish_window("counter", after, delta, tier);
            // a second request may have completed while we settled: if so, the
            // next pass opens a fresh window and its own delta is accounted then.
        } else if idle_close {
            let after = core.metrics.requests;
            core.finish_window("idle_timeout", after, 0, "unattributed");
        }

        if core.last_status.elapsed() >= Duration::from_millis(cfg.status_ms) {
            core.emit_status();
            core.prune_subs();
            core.last_status = Instant::now();
        }

        if !cfg.no_persist {
            core.checkpoint();
            let t = Instant::now();
            if let Some(st) = core.store.as_mut() {
                match st.commit() {
                    Ok(()) => {
                        core.db_ok = true;
                        core.last_commit_ms = t.elapsed().as_secs_f64() * 1000.0;
                    }
                    Err(e) => {
                        core.db_ok = false;
                        core.db_err = e;
                    }
                }
            }
        }

        let spent = loop_start.elapsed();
        let iv = Duration::from_millis(cfg.interval_ms);
        if spent < iv {
            // sleep in slices so SIGTERM is honoured quickly
            let deadline = Instant::now() + (iv - spent);
            while Instant::now() < deadline && !STOP.load(Ordering::SeqCst) {
                thread::sleep(Duration::from_millis(20));
            }
        }
    }

    // ------------------------------------------------------------ shutdown
    core.store.as_mut().map(|s| s.begin());
    passes.clear();
    gaps.clear();
    core.tailer.poll(&mut passes, &mut gaps);
    for g in gaps.drain(..) {
        let sess = core.win.as_ref().map(|w| w.id.clone());
        core.emit_gap(
            g.reason,
            sess.as_deref(),
            Some(g.byte_start),
            Some(g.byte_end),
            g.bytes,
            g.bytes / 88,
            &g.detail,
        );
    }
    if !passes.is_empty() {
        let batch = std::mem::take(&mut passes);
        core.handle_passes(batch);
    }
    if core.tailer.held_records() > 0 {
        let detail = format!(
            "{} record(s) of an incomplete pass held at shutdown; cursor stays at their first byte so the next run re-reads and completes the pass",
            core.tailer.held_records()
        );
        let off = core.tailer.cursor_offset();
        core.emit_gap("shutdown_held_pass", None, Some(off), None, 0, 0, &detail);
        final_reason = "shutdown_held_pass";
    }
    let last_req = core.metrics.requests;
    if core.win.is_some() {
        core.finish_window("shutdown", last_req, 0, "unattributed");
    }
    core.emit_status();
    core.checkpoint();
    if let Some(st) = core.store.as_mut() {
        let _ = st.end_run(&run_id, core.tailer.cursor_offset(), final_reason);
        match st.commit() {
            Ok(()) => {}
            Err(e) => eprintln!("final commit: {e}"),
        }
    }

    let final_line = format!(
        "{{\"log\":\"final\",\"run_id\":\"{}\",\"reason\":\"{}\",\"events\":{},\"sessions\":{},\"passes\":{},\"records\":{},\"positions\":{},\"skipped_bytes\":{},\"resyncs\":{},\"discarded\":{},\"gap_bytes\":{},\"cursor_offset\":{},\"anchor\":{},\"db_ok\":{},\"recovery_skips\":{}}}",
        json::esc(&run_id),
        final_reason,
        core.events_total,
        core.sessions_total,
        core.tailer.passes,
        core.tailer.records_in_passes,
        core.tailer.positions_total,
        core.tailer.skipped_bytes,
        core.tailer.resyncs,
        core.tailer.discarded,
        core.gap_bytes_total,
        core.tailer.cursor_offset(),
        core.tailer.anchor(),
        core.db_ok,
        core.recovery_skips
    );
    println!("{final_line}");
    let _ = std::io::stdout().flush();

    stop_flag.store(true, Ordering::SeqCst);
    // give subscriber threads a moment to flush what is already queued
    thread::sleep(Duration::from_millis(250));
    let _ = std::fs::remove_file(&cfg.socket);
    process::exit(0);
}
