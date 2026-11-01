// Transport: Unix-socket fan-out with bounded per-subscriber queues and a
// minimal HTTP/1.1 client for the engine's /metrics endpoint.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

// ------------------------------------------------------------------ HTTP GET
pub fn http_get(host: &str, port: u16, path: &str, bearer: &str) -> Result<String, String> {
    let mut s = TcpStream::connect((host, port)).map_err(|e| format!("connect: {e}"))?;
    s.set_read_timeout(Some(Duration::from_secs(5))).ok();
    s.set_write_timeout(Some(Duration::from_secs(5))).ok();
    let req = format!(
        "GET {path} HTTP/1.1\r\nHost: {host}:{port}\r\nAuthorization: Bearer {bearer}\r\nAccept: application/json\r\nConnection: close\r\n\r\n"
    );
    s.write_all(req.as_bytes()).map_err(|e| format!("write: {e}"))?;
    s.flush().ok();
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).map_err(|e| format!("read: {e}"))?;
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or("no header terminator")?;
    let head = String::from_utf8_lossy(&raw[..split]).to_string();
    let body = &raw[split + 4..];
    let status: u16 = head
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    if status != 200 {
        return Err(format!("http {status}"));
    }
    let lower = head.to_ascii_lowercase();
    if lower.contains("transfer-encoding: chunked") {
        let mut out = Vec::new();
        let mut i = 0usize;
        while i < body.len() {
            let nl = match body[i..].windows(2).position(|w| w == b"\r\n") {
                Some(p) => i + p,
                None => break,
            };
            let sz = std::str::from_utf8(&body[i..nl])
                .ok()
                .and_then(|s| usize::from_str_radix(s.trim(), 16).ok())
                .ok_or("bad chunk size")?;
            if sz == 0 {
                break;
            }
            let start = nl + 2;
            let end = (start + sz).min(body.len());
            out.extend_from_slice(&body[start..end]);
            i = end + 2;
        }
        return String::from_utf8(out).map_err(|e| e.to_string());
    }
    String::from_utf8(body.to_vec()).map_err(|e| e.to_string())
}

// --------------------------------------------------------- subscriber queues
#[derive(Debug)]
pub struct Item {
    pub seq: i64,
    pub line: String,
    pub droppable: bool,
}

/// What a connecting subscriber gets before the live stream: the stored tail of
/// the current session plus the seq floor that separates replay from live.
#[derive(Debug, Clone)]
pub struct Preload {
    pub lines: Vec<String>,
    pub floor: i64,
    pub truncated: u64,
}

#[derive(Debug)]
pub struct SubState {
    pub items: VecDeque<Item>,
    pub cap: usize,
    pub floor: i64,
    pub drops: u64,  // droppable events dropped under pressure
    pub forced: u64, // critical events dropped because no room (should stay 0)
    pub pruned: u64, // live events removed because replay already covered them
    pub sent: u64,
    pub max_depth: usize,
    pub closed: bool,
    pub preload: Option<Preload>,
    pub peer: String,
    pub registered_ts: f64,
    pub replayed: u64,
    pub replay_truncated: u64,
    pub bytes_out: u64,
    pub closed_at: Option<std::time::Instant>,
}

pub struct Sub {
    pub id: u64,
    pub name: String,
    pub st: Arc<(Mutex<SubState>, Condvar)>,
}

impl Sub {
    pub fn new(id: u64, name: String, peer: String, cap: usize, ts: f64) -> Sub {
        Sub {
            id,
            name,
            st: Arc::new((
                Mutex::new(SubState {
                    items: VecDeque::new(),
                    cap,
                    floor: i64::MIN,
                    drops: 0,
                    forced: 0,
                    pruned: 0,
                    sent: 0,
                    max_depth: 0,
                    closed: false,
                    preload: None,
                    peer,
                    registered_ts: ts,
                    replayed: 0,
                    replay_truncated: 0,
                    bytes_out: 0,
                    closed_at: None,
                }),
                Condvar::new(),
            )),
        }
    }

    pub fn push(&self, seq: i64, line: String, droppable: bool) {
        let (m, cv) = &*self.st;
        let mut g = match m.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        if g.closed {
            return;
        }
        if seq <= g.floor {
            g.pruned += 1;
            return;
        }
        if droppable {
            // Pressure policy: expert_selected is shed once the queue is 3/4 full
            // so a slow reader can never grow memory without bound.
            if g.items.len() >= g.cap * 3 / 4 {
                g.drops += 1;
                return;
            }
        } else {
            while g.items.len() >= g.cap {
                if let Some(pos) = g.items.iter().position(|it| it.droppable) {
                    g.items.remove(pos);
                    g.drops += 1;
                } else {
                    // Only critical events queued: never silently lose them, but
                    // record the forced drop so status can show it.
                    g.forced += 1;
                    return;
                }
            }
        }
        g.items.push_back(Item {
            seq,
            line,
            droppable,
        });
        if g.items.len() > g.max_depth {
            g.max_depth = g.items.len();
        }
        cv.notify_one();
    }

    /// Producer-side: install the replay preload and the seq floor in one step.
    /// Items already queued at or below the floor were replayed, so they are
    /// pruned as duplicates rather than delivered twice.
    pub fn set_preload(&self, lines: Vec<String>, floor: i64, truncated: u64) {
        let (m, cv) = &*self.st;
        let mut g = match m.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        g.floor = floor;
        let before = g.items.len();
        g.items.retain(|it| it.seq > floor);
        let removed = before - g.items.len();
        g.pruned += removed as u64;
        g.preload = Some(Preload {
            lines,
            floor,
            truncated,
        });
        cv.notify_all();
    }

    pub fn close(&self) {
        let (m, cv) = &*self.st;
        let mut g = match m.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        g.closed = true;
        if g.closed_at.is_none() {
            g.closed_at = Some(std::time::Instant::now());
        }
        cv.notify_all();
    }

    pub fn depth_drops(&self) -> (usize, u64, u64, u64) {
        let (m, _) = &*self.st;
        match m.lock() {
            Ok(g) => (g.items.len(), g.drops, g.forced, g.sent),
            Err(p) => {
                let g = p.into_inner();
                (g.items.len(), g.drops, g.forced, g.sent)
            }
        }
    }
}

/// Serve one accepted connection: write the producer-supplied replay preload,
/// then the live stream. Returns when the peer goes away.
pub fn serve(mut stream: UnixStream, sub: Arc<Sub>, stop: Arc<AtomicBool>) {
    stream.set_write_timeout(Some(Duration::from_secs(10))).ok();
    // Wait for the producer to fill the preload (it owns the only DB writer, so
    // its snapshot cannot race live events: nothing is lost in the handover).
    let preload = {
        let t0 = std::time::Instant::now();
        loop {
            let (m, cv) = &*sub.st;
            let mut g = match m.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            if let Some(p) = g.preload.take() {
                break p;
            }
            let _ = cv.wait_timeout(g, Duration::from_millis(100));
            if t0.elapsed() > Duration::from_secs(30) {
                // ROOT CAUSE of the 2026-11-01 22:52 "connected but meta-less"
                // incident: a cold cache + huge window made the producer's tail
                // query exceed the old 10 s; the connection continued with an
                // EMPTY preload — no meta, no residency, no gaps — and the
                // completion log is skipped for empty preloads, so the journal
                // showed nothing. Now: longer patience, and the give-up is LOUD.
                eprintln!(
                    "subscriber {}: preload NOT READY within 30 s (store busy/cold) — continuing live; meta may be absent from this stream",
                    sub.name
                );
                break Preload {
                    lines: Vec::new(),
                    floor: 0,
                    truncated: 0,
                };
            }
        }
    };
    let mut out = ReplayWriter {
        s: &mut stream,
        bytes: 0,
        failed: false,
    };
    let t_replay = std::time::Instant::now();
    for l in &preload.lines {
        out.write_line(l);
    }
    if !preload.lines.is_empty() || preload.truncated > 0 {
        eprintln!(
            "subscriber {}: replay {} lines / {} bytes in {:.1} ms (truncated {}, floor {}, failed={})",
            sub.name,
            preload.lines.len(),
            out.bytes,
            t_replay.elapsed().as_secs_f64() * 1000.0,
            preload.truncated,
            preload.floor,
            out.failed
        );
    }
    {
        let (m, _) = &*sub.st;
        if let Ok(mut g) = m.lock() {
            g.replayed = preload.lines.len() as u64;
            g.replay_truncated = preload.truncated;
        }
    }
    if out.failed {
        sub.close();
        return;
    }
    loop {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        let item = {
            let (m, cv) = &*sub.st;
            let mut g = match m.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            if g.items.is_empty() {
                if g.closed {
                    break;
                }
                let (ng, to) = match cv.wait_timeout(g, Duration::from_millis(200)) {
                    Ok(v) => v,
                    Err(p) => p.into_inner(),
                };
                g = ng;
                if g.items.is_empty() {
                    if to.timed_out() {
                        continue;
                    } else {
                        continue;
                    }
                }
            }
            let it = g.items.pop_front();
            if let Some(_) = it {
                g.sent += 1;
            }
            it
        };
        match item {
            Some(it) => {
                out.write_line(&it.line);
                if out.failed {
                    sub.close();
                    break;
                }
                {
                    let (m, _) = &*sub.st;
                    if let Ok(mut g) = m.lock() {
                        g.bytes_out = out.bytes;
                    }
                }
            }
            None => break,
        }
    }
    sub.close();
}

struct ReplayWriter<'a> {
    s: &'a mut UnixStream,
    bytes: u64,
    failed: bool,
}

impl<'a> ReplayWriter<'a> {
    fn write_line(&mut self, l: &str) {
        if self.failed {
            return;
        }
        let mut buf = Vec::with_capacity(l.len() + 1);
        buf.extend_from_slice(l.as_bytes());
        buf.push(b'\n');
        if self.s.write_all(&buf).is_err() {
            self.failed = true;
            return;
        }
        self.bytes += buf.len() as u64;
    }
}

pub fn bind_listener(path: &str) -> Result<UnixListener, String> {
    use std::os::unix::fs::PermissionsExt;
    let p = std::path::Path::new(path);
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("mkdir {}: {e}", dir.display()))?;
    }
    if p.exists() {
        let _ = std::fs::remove_file(p);
    }
    let l = UnixListener::bind(p).map_err(|e| format!("bind {path}: {e}"))?;
    let _ = std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o666));
    l.set_nonblocking(true).map_err(|e| e.to_string())?;
    Ok(l)
}
