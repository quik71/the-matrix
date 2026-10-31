// Tail the Strata engine's binary routing trace and turn it into complete passes.
//
// Record layout (little-endian, verified against moe_telemetry/trace.py and the
// Stage 2 PoC):
//   i32 layer (0..47), i32 k (1..64), k * i32 expert_id, k * f32 weight
// Records are LAYER-MAJOR inside one forward pass: layer 0..47 ascending, each
// layer repeated P (positions) times. A pass is 48*P records; a pass boundary is
// where the layer id decreases. The file contains ZERO HOLES: any run of bytes
// that does not parse is a skip, and every skipped byte is counted
// (skipped_bytes / skipped_since_last) and surfaced - never silently dropped.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};

pub const EXPECTED_LAYERS: i32 = 48;
pub const MAX_EXPERTS: i32 = 512;
pub const MAX_K: i32 = 64;
pub const MAX_PASS_RECORDS: usize = 48 * 1024;
pub const MAX_CHUNK: usize = 4 * 1024 * 1024; // per-read cap: memory stays bounded
pub const MAX_BUF: usize = 8 * 1024 * 1024;
pub const RESTART_MAX_BYTES: u64 = 4096;

#[derive(Clone, Debug)]
pub struct Rec {
    pub layer: i32,
    pub k: i32,
    pub ids: Vec<i32>,
    pub weights: Vec<f32>,
    pub off: u64,
}

#[derive(Clone, Debug)]
pub struct Pass {
    pub start_off: u64,
    pub end_off: u64,
    pub positions: usize,
    pub recs: Vec<Rec>,
}

#[derive(Clone, Debug)]
pub struct TraceGap {
    pub reason: &'static str,
    pub byte_start: u64,
    pub byte_end: u64,
    pub bytes: u64,
    pub detail: String,
}

pub struct Tailer {
    pub path: String,
    offset: u64, // next file byte to read
    buf: Vec<u8>, // bytes read but not yet consumed
    buf_off: u64, // file offset of buf[0]
    anchor: u64,  // end of the last emitted pass

    cur: Vec<Rec>,
    last_layer: i32,

    pub records_seen: u64,
    pub records_in_passes: u64,
    pub passes: u64,
    pub discarded: u64,
    pub resyncs: u64,
    pub skipped_bytes: u64,
    pub rewinds: u64,
    pub resets: u64,
    pub bytes_read: u64,
    pub positions_total: u64,
}

fn rd_i32(b: &[u8], o: usize) -> i32 {
    i32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

fn rd_f32(b: &[u8], o: usize) -> f32 {
    f32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

impl Tailer {
    pub fn new(path: String) -> Tailer {
        Tailer {
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
            positions_total: 0,
        }
    }

    /// Start tailing at `off` (default cold start = current EOF). `anchor` is the
    /// end of the last pass that reached durable storage; it bounds the rewind.
    pub fn resume_at(&mut self, off: u64, anchor: u64) {
        self.offset = off;
        self.buf_off = off;
        self.anchor = anchor;
        self.buf.clear();
        self.cur.clear();
        self.last_layer = -1;
    }

    pub fn file_size(&self) -> u64 {
        fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0)
    }

    /// First offset that must be re-read after a crash: the start of the pass we
    /// were holding, or the front of the unconsumed buffer when no pass is held.
    pub fn cursor_offset(&self) -> u64 {
        if let Some(r) = self.cur.first() {
            r.off
        } else {
            self.buf_off
        }
    }

    pub fn anchor(&self) -> u64 {
        self.anchor
    }

    pub fn held_records(&self) -> usize {
        self.cur.len()
    }

    fn find_resync(&self, start: usize) -> Option<usize> {
        let n = self.buf.len();
        let mut j = start + 1;
        while j + 16 <= n {
            let layer = rd_i32(&self.buf, j);
            let k = rd_i32(&self.buf, j + 4);
            if layer >= 0 && layer < EXPECTED_LAYERS && k > 0 && k <= MAX_K {
                let need = 8 + 8 * k as usize;
                if j + need + 8 <= n {
                    let nid = rd_i32(&self.buf, j + 8);
                    let l2 = rd_i32(&self.buf, j + need);
                    let k2 = rd_i32(&self.buf, j + need + 4);
                    if nid >= 0 && nid < MAX_EXPERTS && k2 == k && l2 >= 0 && l2 < EXPECTED_LAYERS {
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

    fn parse_available(&mut self, out: &mut Vec<Pass>, gaps: &mut Vec<TraceGap>) {
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
                    gaps.push(TraceGap {
                        reason: "resync_skip",
                        byte_start: self.buf_off + i as u64,
                        byte_end: self.buf_off + j as u64,
                        bytes: (j - i) as u64,
                        detail: "unparsable bytes skipped until next valid record header".into(),
                    });
                    i = j;
                    continue;
                } else if n - i > (1 << 20) {
                    let drop = n - i - 1024;
                    self.skipped_bytes += drop as u64;
                    gaps.push(TraceGap {
                        reason: "unknown_region",
                        byte_start: self.buf_off + i as u64,
                        byte_end: self.buf_off + i as u64 + drop as u64,
                        bytes: drop as u64,
                        detail: "no resync point inside 1 MiB; region skipped and counted".into(),
                    });
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
            let ids: Vec<i32> = (0..k as usize)
                .map(|x| rd_i32(&self.buf, i + 8 + 4 * x))
                .collect();
            let weights: Vec<f32> = (0..k as usize)
                .map(|x| rd_f32(&self.buf, i + 8 + 4 * k as usize + 4 * x))
                .collect();
            let off = self.buf_off + i as u64;
            i += need;

            self.records_seen += 1;
            if layer < self.last_layer {
                self.flush_pass(out, gaps);
            }
            self.last_layer = layer;
            self.cur.push(Rec {
                layer,
                k,
                ids,
                weights,
                off,
            });
            if self.cur.len() > MAX_PASS_RECORDS {
                self.drop_pass("oversize", gaps);
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

    fn drop_pass(&mut self, why: &'static str, gaps: &mut Vec<TraceGap>) {
        let recs = std::mem::replace(&mut self.cur, Vec::new());
        self.last_layer = -1;
        if recs.is_empty() {
            return;
        }
        self.discarded += 1;
        let start = recs[0].off;
        let last = recs.last().unwrap();
        let end = last.off + 8 + 8 * last.k as u64;
        gaps.push(TraceGap {
            reason: "discarded_pass",
            byte_start: start,
            byte_end: end,
            bytes: end.saturating_sub(start),
            detail: format!("{} records dropped ({})", recs.len(), why),
        });
    }

    fn flush_pass(&mut self, out: &mut Vec<Pass>, gaps: &mut Vec<TraceGap>) {
        if self.cur.is_empty() {
            return;
        }
        let (nlayers, p) = match self.pass_shape() {
            Some(s) => s,
            None => {
                self.drop_pass("irregular", gaps);
                return;
            }
        };
        debug_assert_eq!(nlayers, EXPECTED_LAYERS as usize);
        let recs = std::mem::replace(&mut self.cur, Vec::new());
        self.last_layer = -1;
        let last = recs.last().unwrap();
        let end_off = last.off + 8 + 8 * last.k as u64;
        self.passes += 1;
        self.records_in_passes += recs.len() as u64;
        self.positions_total += p as u64;
        self.anchor = end_off;
        out.push(Pass {
            start_off: recs[0].off,
            end_off,
            positions: p,
            recs,
        });
    }

    /// One polling step. Returns the passes completed during this step; `gaps`
    /// receives any byte ranges that had to be skipped.
    pub fn poll(&mut self, out: &mut Vec<Pass>, gaps: &mut Vec<TraceGap>) {
        let size = self.file_size();
        if size < self.offset {
            let gap = self.offset - size;
            if size < RESTART_MAX_BYTES || gap > (1u64 << 20).max(self.offset / 2) {
                self.resets += 1;
                gaps.push(TraceGap {
                    reason: "trace_truncated",
                    byte_start: size,
                    byte_end: self.offset,
                    bytes: gap,
                    detail: format!("trace shrank to {} bytes: new engine session", size),
                });
                self.offset = size;
                self.anchor = size;
                self.buf.clear();
                self.buf_off = size;
                self.cur.clear();
                self.last_layer = -1;
            } else {
                self.rewinds += 1;
                gaps.push(TraceGap {
                    reason: "trace_rewind",
                    byte_start: size,
                    byte_end: self.offset,
                    bytes: gap,
                    detail: format!("trace shrank {} -> {}: rewinding to pass anchor", self.offset, size),
                });
                self.buf.clear();
                self.cur.clear();
                self.last_layer = -1;
                self.offset = if self.anchor <= size { self.anchor } else { size };
                self.buf_off = self.offset;
            }
        }
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
                gaps.push(TraceGap {
                    reason: "buffer_overflow",
                    byte_start: self.buf_off,
                    byte_end: self.buf_off + drop as u64,
                    bytes: drop as u64,
                    detail: "read buffer full: oldest bytes dropped and counted".into(),
                });
                self.buf.drain(..drop);
                self.buf_off += drop as u64;
            }
            self.parse_available(out, gaps);
            progressed = true;
            budget -= 1;
            cur_size = self.file_size();
        }
        if !progressed && self.pass_shape().is_some() {
            // Nothing new and what we hold is a complete pass: emit it. Any
            // following record starts at layer 0, which is also < 47, so this
            // cannot shear a pass that is still being written.
            self.flush_pass(out, gaps);
        }
    }
}
