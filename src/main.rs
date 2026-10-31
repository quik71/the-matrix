// matrix-collect: tail the Strata engine's binary routing trace and prove
// per-pass expert-routing events are real, independent of the client.
//
// Record layout (little-endian, verified against moe_telemetry/trace.py):
//   i32 layer (0..47), i32 k (1..64), k * i32 expert_id, k * f32 weight
// Records are LAYER-MAJOR inside one forward pass: layer 0..47 ascending,
// each layer repeated P (positions) times. A pass is 48*P records; a pass
// boundary is where the layer id decreases. std only, no external crates.

use std::collections::BTreeMap;
use std::env;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::process;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const EXPECTED_LAYERS: i32 = 48;
const MAX_EXPERTS: i32 = 512;
const MAX_K: i32 = 64;
const MAX_PASS_RECORDS: usize = 48 * 1024;
const MAX_CHUNK: usize = 4 * 1024 * 1024; // per-read cap: memory stays bounded
const MAX_BUF: usize = 8 * 1024 * 1024;
const RESTART_MAX_BYTES: u64 = 4096;

static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn handle_signal(_sig: i32) {
    STOP.store(true, Ordering::SeqCst);
}

unsafe extern "C" {
    fn signal(sig: i32, handler: extern "C" fn(i32)) -> usize;
}

struct Rec {
    layer: i32,
    k: i32,
    ids: Vec<i32>,
    weights: Vec<f32>,
    off: u64,
}

struct Collector {
    path: String,
    offset: u64,   // next file byte to read
    buf: Vec<u8>,  // bytes read but not yet consumed
    buf_off: u64,  // file offset of buf[0]
    anchor: u64,   // end of the last emitted pass

    cur: Vec<Rec>,
    last_layer: i32,

    records_seen: u64,
    records_in_passes: u64,
    passes: u64,
    discarded: u64,
    resyncs: u64,
    skipped_bytes: u64,
    rewinds: u64,
    resets: u64,
    bytes_read: u64,

    min_id: i32,
    max_id: i32,
    ids_valid: bool,
    all_layers_complete: bool,
    k_min: i32,
    k_max: i32,
    w_min: f32,
    w_max: f32,
    w_nonzero: u64,
    p_min: usize,
    p_max: usize,
    passes_p1: u64,
    passes_multi: u64,
    total_positions: u64,
}

fn now_unix() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

fn rd_i32(b: &[u8], o: usize) -> i32 {
    i32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

fn rd_f32(b: &[u8], o: usize) -> f32 {
    f32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

impl Collector {
    fn new(path: String) -> Collector {
        Collector {
            path,
            offset: 0,
            buf: Vec::with_capacity(1 << 20),
            buf_off: 0,
            anchor: 0,
            cur: Vec::new(),
            last_layer: -1,
            records_seen: 0,
            records_in_passes: 0,
            passes: 0,
            discarded: 0,
            resyncs: 0,
            skipped_bytes: 0,
            rewinds: 0,
            resets: 0,
            bytes_read: 0,
            min_id: i32::MAX,
            max_id: i32::MIN,
            ids_valid: true,
            all_layers_complete: true,
            k_min: i32::MAX,
            k_max: i32::MIN,
            w_min: f32::INFINITY,
            w_max: f32::NEG_INFINITY,
            w_nonzero: 0,
            p_min: usize::MAX,
            p_max: 0,
            passes_p1: 0,
            passes_multi: 0,
            total_positions: 0,
        }
    }

    // ------------------------------------------------ resync (cold-start safe)
    /// First offset >= start that looks like a record boundary: valid header,
    /// the record's ids inside 0..MAX_EXPERTS, and a valid header right after it.
    /// Scans BYTE-WISE: a record is 88 bytes at k=10, so a 4-byte stride from an
    /// offset whose parity differs never lands on a boundary.
    fn find_resync(&self, start: usize) -> Option<usize> {
        let n = self.buf.len();
        let mut j = start + 1;
        let limit = n; // scan the whole buffer: a cold start can land in a long gap
        while j + 16 <= limit {
            let layer = rd_i32(&self.buf, j);
            let k = rd_i32(&self.buf, j + 4);
            if layer >= 0 && layer < EXPECTED_LAYERS && k > 0 && k <= MAX_K {
                let need = 8 + 8 * k as usize;
                if j + need + 8 <= n {
                    let nid = rd_i32(&self.buf, j + 8);
                    let l2 = rd_i32(&self.buf, j + need);
                    let k2 = rd_i32(&self.buf, j + need + 4);
                    if nid >= 0 && nid < MAX_EXPERTS && k2 == k && l2 >= 0 && l2 < EXPECTED_LAYERS {
                        // a third consecutive header is much stronger evidence
                        let need2 = 8 + 8 * k2 as usize;
                        if j + need + need2 > n || {
                            let l3 = rd_i32(&self.buf, j + need + need2);
                            l3 >= 0 && l3 < EXPECTED_LAYERS
                        } {
                            return Some(j);
                        }
                    }
                }
            }
            j += 1;
        }
        None
    }

    fn parse_available(&mut self) {
        let mut i = 0usize;
        loop {
            let n = self.buf.len();
            if i + 8 > n {
                break;
            }
            let layer = rd_i32(&self.buf, i);
            let k = rd_i32(&self.buf, i + 4);
            let hdr_ok = layer >= 0 && layer < EXPECTED_LAYERS && k > 0 && k <= MAX_K;
            if !hdr_ok {
                if let Some(j) = self.find_resync(i) {
                    self.resyncs += 1;
                    self.skipped_bytes += (j - i) as u64;
                    i = j;
                    continue;
                } else if n - i > (1 << 20) {
                    let drop = n - i - 1024;
                    self.skipped_bytes += drop as u64;
                    i += drop;
                    continue;
                } else {
                    break;
                }
            }
            let need = 8 + 8 * k as usize;
            if i + need > n {
                break; // partial record: wait for the next read
            }
            let ids: Vec<i32> = (0..k as usize).map(|x| rd_i32(&self.buf, i + 8 + 4 * x)).collect();
            let weights: Vec<f32> = (0..k as usize)
                .map(|x| rd_f32(&self.buf, i + 8 + 4 * k as usize + 4 * x))
                .collect();
            let off = self.buf_off + i as u64;
            i += need;

            self.records_seen += 1;
            if layer < self.last_layer {
                self.flush_pass();
            }
            self.last_layer = layer;
            self.cur.push(Rec { layer, k, ids, weights, off });
            if self.cur.len() > MAX_PASS_RECORDS {
                self.drop_pass("oversize");
            }
        }
        if i > 0 {
            self.buf.drain(..i);
            self.buf_off += i as u64;
        }
    }

    fn pass_shape(&self) -> Option<(usize, usize)> {
        let n = self.cur.len();
        if n == 0 || n % EXPECTED_LAYERS as usize != 0 {
            return None;
        }
        let p = n / EXPECTED_LAYERS as usize;
        let mut counts: BTreeMap<i32, usize> = BTreeMap::new();
        for r in &self.cur {
            *counts.entry(r.layer).or_insert(0) += 1;
        }
        if counts.len() != EXPECTED_LAYERS as usize {
            return None;
        }
        if counts.values().any(|&v| v != p) {
            return None;
        }
        if self.cur.last().map(|r| r.layer) != Some(EXPECTED_LAYERS - 1) {
            return None;
        }
        Some((counts.len(), p))
    }

    fn drop_pass(&mut self, why: &str) {
        let recs = std::mem::replace(&mut self.cur, Vec::new());
        self.last_layer = -1;
        if recs.is_empty() {
            return;
        }
        self.discarded += 1;
        let mut layers: BTreeMap<i32, usize> = BTreeMap::new();
        for r in &recs {
            *layers.entry(r.layer).or_insert(0) += 1;
        }
        println!(
            "[discard] reason={} records={} distinct_layers={} p={}",
            why,
            recs.len(),
            layers.len(),
            if recs.len() % 48 == 0 { recs.len() / 48 } else { 0 }
        );
    }

    fn flush_pass(&mut self) {
        if self.cur.is_empty() {
            return;
        }
        let shape = self.pass_shape();
        let (nlayers, p) = match shape {
            Some(s) => s,
            None => {
                self.drop_pass("irregular");
                return;
            }
        };
        let recs = std::mem::replace(&mut self.cur, Vec::new());
        self.last_layer = -1;

        let mut pmin = i32::MAX;
        let mut pmax = i32::MIN;
        let mut pw_min = f32::INFINITY;
        let mut pw_max = f32::NEG_INFINITY;
        let mut pnz = 0u64;
        let mut pk_min = i32::MAX;
        let mut pk_max = i32::MIN;
        let mut ids_ok = true;
        for r in &recs {
            pk_min = pk_min.min(r.k);
            pk_max = pk_max.max(r.k);
            for &id in &r.ids {
                pmin = pmin.min(id);
                pmax = pmax.max(id);
                if id < 0 || id >= MAX_EXPERTS {
                    ids_ok = false;
                }
            }
            for &w in &r.weights {
                if w != w {
                    ids_ok = false;
                }
                pw_min = pw_min.min(w);
                pw_max = pw_max.max(w);
                if w != 0.0 {
                    pnz += 1;
                }
            }
        }

        self.passes += 1;
        self.records_in_passes += recs.len() as u64;
        self.total_positions += p as u64;
        self.pass_id_min_max(pmin, pmax);
        self.ids_valid &= ids_ok;
        self.all_layers_complete &= nlayers == EXPECTED_LAYERS as usize;
        self.k_min = self.k_min.min(pk_min);
        self.k_max = self.k_max.max(pk_max);
        self.w_min = self.w_min.min(pw_min);
        self.w_max = self.w_max.max(pw_max);
        self.w_nonzero += pnz;
        self.p_min = self.p_min.min(p);
        self.p_max = self.p_max.max(p);
        if p == 1 {
            self.passes_p1 += 1;
        } else {
            self.passes_multi += 1;
        }
        let last = &recs[recs.len() - 1];
        self.anchor = last.off + 8 + 8 * last.k as u64;

        println!(
            "[pass {}] off={} layers={} p={} recs={} k={} ids={}..{} w=[{},{}] nonzero={} ids_ok={}",
            self.passes,
            recs[0].off,
            nlayers,
            p,
            recs.len(),
            pk_max,
            pmin,
            pmax,
            pw_min,
            pw_max,
            pnz,
            ids_ok
        );
    }

    fn pass_id_min_max(&mut self, lo: i32, hi: i32) {
        self.min_id = self.min_id.min(lo);
        self.max_id = self.max_id.max(hi);
    }

    // ---------------------------------------------------------------- tailing
    fn poll(&mut self) {
        let size = match fs::metadata(&self.path) {
            Ok(m) => m.len(),
            Err(_) => return,
        };
        if size < self.offset {
            let gap = self.offset - size;
            if size < RESTART_MAX_BYTES || gap > (1u64 << 20).max(self.offset / 2) {
                self.resets += 1;
                println!("[trace] truncation -> new engine session (size={})", size);
                self.offset = size;
                self.anchor = size;
                self.buf.clear();
                self.buf_off = size;
                self.cur.clear();
                self.last_layer = -1;
            } else {
                self.rewinds += 1;
                println!("[trace] rewind {} -> {} ({} bytes)", self.offset, size, gap);
                self.buf.clear();
                self.cur.clear();
                self.last_layer = -1;
                self.offset = if self.anchor <= size { self.anchor } else { size };
                self.buf_off = self.offset;
            }
        }
        // Drain the backlog in bounded chunks; the buffer never exceeds MAX_CHUNK + carry.
        let mut progressed = false;
        let mut budget = 64u32;
        let mut cur_size = size;
        while budget > 0 && cur_size > self.offset {
            let want = ((cur_size - self.offset) as usize).min(MAX_CHUNK);
            let mut chunk = vec![0u8; want];
            let got = match File::open(&self.path).and_then(|mut f| {
                f.seek(SeekFrom::Start(self.offset))?;
                let mut read = 0usize;
                while read < want {
                    match f.read(&mut chunk[read..]) {
                        Ok(0) => break,
                        Ok(n) => read += n,
                        Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(e) => return Err(e),
                    }
                }
                Ok(read)
            }) {
                Ok(g) => g,
                Err(_) => break,
            };
            if got == 0 {
                break;
            }
            chunk.truncate(got);
            self.bytes_read += got as u64;
            self.offset += got as u64;
            if self.buf.is_empty() {
                self.buf_off = self.offset - got as u64;
            }
            self.buf.extend_from_slice(&chunk);
            if self.buf.len() > MAX_BUF {
                let drop = self.buf.len() - MAX_BUF / 2;
                self.skipped_bytes += drop as u64;
                self.buf.drain(..drop);
                self.buf_off += drop as u64;
            }
            self.parse_available();
            progressed = true;
            budget -= 1;
            cur_size = fs::metadata(&self.path).map(|m| m.len()).unwrap_or(self.offset);
        }
        if !progressed && self.pass_shape().is_some() {
            // No new bytes and what we hold is a complete pass: emit it. Any
            // following record starts at layer 0, which is also < 47, so this
            // cannot shear a pass that is still being written.
            self.flush_pass();
        }
    }

    // ---------------------------------------------------------------- output
    fn stats_json(&self, t0: Instant) -> String {
        format!(
            concat!(
                "{{\"event\":\"stats\",\"ts\":{:.3},\"pid\":{},\"offset\":{},\"anchor\":{},",
                "\"bytes_read\":{},\"records_seen\":{},\"records_in_passes\":{},",
                "\"passes\":{},\"discarded\":{},\"resyncs\":{},\"skipped_bytes\":{},",
                "\"rewinds\":{},\"resets\":{},\"passes_p1\":{},\"passes_multi\":{},",
                "\"p_min\":{},\"p_max\":{},\"positions\":{},",
                "\"ids\":{{\"min\":{},\"max\":{},\"valid\":{}}},",
                "\"k\":{{\"min\":{},\"max\":{}}},",
                "\"weights\":{{\"min\":{},\"max\":{},\"nonzero\":{}}},",
                "\"layers_complete\":{},\"elapsed_s\":{:.1}}}"
            ),
            now_unix(),
            process::id(),
            self.offset,
            self.anchor,
            self.bytes_read,
            self.records_seen,
            self.records_in_passes,
            self.passes,
            self.discarded,
            self.resyncs,
            self.skipped_bytes,
            self.rewinds,
            self.resets,
            self.passes_p1,
            self.passes_multi,
            if self.p_min == usize::MAX { 0 } else { self.p_min },
            self.p_max,
            self.total_positions,
            if self.min_id == i32::MAX { 0 } else { self.min_id },
            if self.max_id == i32::MIN { 0 } else { self.max_id },
            self.ids_valid,
            if self.k_min == i32::MAX { 0 } else { self.k_min },
            if self.k_max == i32::MIN { 0 } else { self.k_max },
            if self.w_min.is_infinite() { 0.0 } else { self.w_min },
            if self.w_max.is_infinite() { 0.0 } else { self.w_max },
            self.w_nonzero,
            self.all_layers_complete,
            t0.elapsed().as_secs_f64()
        )
    }
}

fn main() {
    let args: Vec<String> = env::args().collect();
    let mut path = "/home/main/strata-0.2.4.2/data/routing-trace.bin".to_string();
    let mut interval_ms = 150u64;
    let mut start_override: Option<u64> = None;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--path" => {
                if i + 1 < args.len() {
                    path = args[i + 1].clone();
                    i += 2;
                } else {
                    i += 1;
                }
            }
            "--interval-ms" => {
                if i + 1 < args.len() {
                    interval_ms = args[i + 1].parse().unwrap_or(150);
                    i += 2;
                } else {
                    i += 1;
                }
            }
            "--offset" => {
                if i + 1 < args.len() {
                    start_override = args[i + 1].parse().ok();
                    i += 2;
                } else {
                    i += 1;
                }
            }
            _ => i += 1,
        }
    }

    unsafe {
        signal(15, handle_signal); // SIGTERM
        signal(2, handle_signal);  // SIGINT
    }

    let mut c = Collector::new(path.clone());
    let size = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    let start = start_override.unwrap_or(size); // default: current EOF
    c.offset = start;
    c.buf_off = start;
    c.anchor = start;
    println!(
        "{{\"event\":\"start\",\"ts\":{:.3},\"pid\":{},\"path\":\"{}\",\"start_offset\":{},\"interval_ms\":{}}}",
        now_unix(),
        process::id(),
        path,
        start,
        interval_ms
    );

    let t0 = Instant::now();
    let mut last_stats = Instant::now();
    while !STOP.load(Ordering::SeqCst) {
        c.poll();
        if last_stats.elapsed() >= Duration::from_secs(2) {
            println!("{}", c.stats_json(t0));
            last_stats = Instant::now();
        }
        thread::sleep(Duration::from_millis(interval_ms));
    }

    // Final sweep: grab any last bytes, emit a complete held pass, count leftovers.
    c.poll();
    if c.pass_shape().is_some() {
        c.flush_pass();
    }
    if !c.cur.is_empty() {
        c.drop_pass("shutdown-partial");
    }
    println!("{}", c.stats_json(t0));
    println!(
        "FINAL {{\"passes\":{},\"records_in_passes\":{},\"records_seen\":{},\"discarded\":{},\"positions\":{},\"ids\":{{\"min\":{},\"max\":{},\"valid\":{}}},\"layers_complete\":{},\"weights\":{{\"min\":{},\"max\":{},\"nonzero\":{}}},\"k_max\":{},\"passes_p1\":{},\"passes_multi\":{}}}",
        c.passes,
        c.records_in_passes,
        c.records_seen,
        c.discarded,
        c.total_positions,
        if c.min_id == i32::MAX { 0 } else { c.min_id },
        if c.max_id == i32::MIN { 0 } else { c.max_id },
        c.ids_valid,
        c.all_layers_complete,
        if c.w_min.is_infinite() { 0.0 } else { c.w_min },
        if c.w_max.is_infinite() { 0.0 } else { c.w_max },
        c.w_nonzero,
        if c.k_max == i32::MIN { 0 } else { c.k_max },
        c.passes_p1,
        c.passes_multi
    );
    process::exit(0);
}
