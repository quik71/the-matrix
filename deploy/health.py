#!/usr/bin/env python3
"""Liveness/health check for a THE MATRIX pair (collector + app).

Reads the app's ``--stats`` dump (the app is the only component that can prove
*end-to-end* delivery: connected, meta, residency painted, drops), optionally
waits for a fresh dump, and prints one JSON object. Exit 0 only when every
assertion below holds.

usage: health.py [--prefix DIR] [--wait SECONDS] [--min-frames N]

Assertions (all must be true):
  connected            app->collector socket is up
  meta                 a meta event arrived over the socket (meta_source=socket)
  residency.painted    equals residency.slots (the estimate was actually painted)
  forced_drops         == 0 for our subscriber (drops beyond the counted ones)
  frames               > 0 (the window is really presenting)
"""
import argparse
import json
import os
import sys
import time


def load(path):
    try:
        with open(path) as f:
            return json.load(f)
    except Exception:
        return None


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--prefix", default=os.path.expanduser("~/the-matrix"))
    ap.add_argument("--stats", default="")
    ap.add_argument("--wait", type=float, default=0.0)
    ap.add_argument("--min-frames", type=int, default=1)
    ap.add_argument("--allow-wait-out", action="store_true",
                    help="report the last dump instead of failing when --wait expires")
    a = ap.parse_args()
    stats_path = a.stats or os.path.join(a.prefix, "run", "live-stats.json")

    deadline = time.time() + a.wait
    d = None
    fresh = False
    while True:
        d = load(stats_path)
        if d:
            age = time.time() - os.path.getmtime(stats_path)
            # A dump written within the last 5 s belongs to a live process.
            fresh = age <= 5.0
        if d and fresh:
            break
        if time.time() >= deadline:
            break
        time.sleep(0.25)

    r = {"stats_path": stats_path, "stats_exists": bool(d), "stats_fresh": fresh}
    if not d:
        r["ok"] = False
        r["why"] = "no stats dump (app not running?)"
        print(json.dumps(r))
        return 1

    meta = d.get("meta") or {}
    res = d.get("residency") or {}
    status = d.get("status") or {}
    our = status.get("our_sub") or {}
    r.update({
        "pid": d.get("pid"),
        "uptime_s": d.get("uptime_s"),
        "frames": d.get("frames"),
        "connected": d.get("connected"),
        "connects": d.get("connects"),
        "reconnects_with_replay": d.get("reconnects_with_replay"),
        "meta_source": meta.get("meta_source"),
        "meta_model": meta.get("model"),
        "meta_run_id": meta.get("run_id"),
        "residency_status": res.get("status"),
        "residency_slots": res.get("slots"),
        "residency_painted": res.get("painted"),
        "drops_total": status.get("drops_total"),
        "dropped_ours": our.get("dropped_droppable"),
        "forced_ours": our.get("forced_drops"),
        "replay_seen": d.get("replay_seen"),
        "placement_verified": (d.get("placement") or {}).get("verified"),
        "events_total": d.get("events_total"),
        "last_seq": d.get("last_seq"),
    })
    checks = {
        "connected": d.get("connected") is True,
        "fresh": fresh,
        "meta_over_socket": bool(meta) and meta.get("meta_source") == "socket",
        "residency_painted": (res.get("status") == "estimate"
                              and res.get("painted") == res.get("slots")
                              and (res.get("painted") or 0) > 0),
        "no_forced_drops": our.get("forced_drops") == 0,
        "no_uncounted_drops": (our.get("dropped_droppable") or 0) <= (status.get("drops_total") or 0),
        "frames": (d.get("frames") or 0) >= a.min_frames,
    }
    r["checks"] = checks
    r["ok"] = all(checks.values())
    r["why"] = ",".join(k for k, v in checks.items() if not v) or ""
    print(json.dumps(r))
    return 0 if r["ok"] else 1


if __name__ == "__main__":
    sys.exit(main())
