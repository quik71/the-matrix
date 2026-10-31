// Collector client: one thread that owns the unix socket, reconnects forever,
// and hands raw NDJSON lines to the UI thread. It never invents, filters or
// reorders anything: the UI thread sees exactly the bytes the collector wrote.

use std::io::{BufRead, BufReader};
use std::os::unix::net::UnixStream;
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

pub enum Msg {
    Line(String),
    Connected { wall: f64 },
    Disconnected { reason: String, wall: f64 },
    ConnectError { err: String, wall: f64 },
}

/// Wake budget: the reader may hand the UI thread one wake-up per 4 ms while a
/// stream is flowing, so a 1000 ev/s burst costs ~250 event-loop wakes/s
/// instead of 1000, and a silent socket costs none at all.
const WAKE_MIN_INTERVAL: Duration = Duration::from_millis(4);

pub fn spawn(
    socket: String,
    tx: Sender<Msg>,
    proxy: Arc<dyn Fn() + Send + Sync>,
    stop: Arc<AtomicBool>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut last_err = String::new();
        // An app that is already showing "not connected" must not be woken by
        // every retry: only a state change (or a different error) is news.
        let mut announced_down = false;
        while !stop.load(Ordering::SeqCst) {
            match UnixStream::connect(&socket) {
                Ok(stream) => {
                    announced_down = false;
                    last_err.clear();
                    let wall = crate::now_unix();
                    if tx.send(Msg::Connected { wall }).is_err() {
                        return;
                    }
                    proxy();
                    // One wake budget per connection: the loop below coalesces a
                    // burst into at most one UI wake every 4 ms.
                    let mut last_wake = Instant::now() - WAKE_MIN_INTERVAL;
                    let reader = BufReader::with_capacity(1 << 16, stream);
                    let mut reason = "eof".to_string();
                    for line in reader.lines() {
                        match line {
                            Ok(l) => {
                                if l.is_empty() {
                                    continue;
                                }
                                if tx.send(Msg::Line(l)).is_err() {
                                    return;
                                }
                                if last_wake.elapsed() >= WAKE_MIN_INTERVAL {
                                    proxy();
                                    last_wake = Instant::now();
                                }
                            }
                            Err(e) => {
                                reason = format!("read error: {e}");
                                break;
                            }
                        }
                    }
                    if stop.load(Ordering::SeqCst) {
                        return;
                    }
                    if tx
                        .send(Msg::Disconnected {
                            reason: reason.clone(),
                            wall: crate::now_unix(),
                        })
                        .is_err()
                    {
                        return;
                    }
                    proxy();
                }
                Err(e) => {
                    let err = format!("{e}");
                    // One wake per failure episode, not per retry: while the
                    // "not connected" state is already on screen the app stays
                    // in ControlFlow::Wait and does not redraw at all.
                    if !announced_down || err != last_err {
                        last_err = err.clone();
                        announced_down = true;
                        if tx
                            .send(Msg::ConnectError {
                                err,
                                wall: crate::now_unix(),
                            })
                            .is_err()
                        {
                            return;
                        }
                        proxy();
                    }
                }
            }
            // back off before reconnecting; the app shows "not connected"
            // honestly for this whole interval
            // Reconnect fast (50 ms): a fresh collector emits its `meta` event
            // once, right after binding the socket, so the app has to be on the
            // socket within that window to see the model geometry live.
            let mut slept = 0u64;
            while slept < 50 && !stop.load(Ordering::SeqCst) {
                thread::sleep(Duration::from_millis(10));
                slept += 10;
            }
        }
    })
}
