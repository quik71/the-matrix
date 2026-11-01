#!/usr/bin/env python3
"""Liveness/health check for a THE MATRIX pair (collector + app).

Reads the app's ``--stats`` dump (the app is the only component that can prove
*end-to-end* delivery: connected, meta, residency painted, drops) and prints one
JSON object. Exit 0 only when every assertion holds.

usage: health.py [--prefix DIR] [--stats PATH] [--wait SECONDS] [--min-frames N]

--wait N means "poll up to N seconds for a HEALTHY pair". A dump that is merely
fresh but still disconnected is not health: after a restart the app writes its
first stats line within ~1 s, often before the socket connect completes, so
returning on freshness alone reports connected=false on a pair that is fine
(this bit the Stage-8 harness on its first run -- see HARNESS-NOTES).

Assertions:
  connected            app->collector socket is up
  meta_over_socket     a meta event arrived over the socket (meta_source=socket)
  residency_painted    residency.painted equals residency.slots (>0)
  no_forced_drops      forced_drops == 0 for our subscriber
  no_uncounted_drops   dropped_droppable <= collector drops_total (nothing hidden)
  frames               >= --min-frames (the window is really presenting)
  fresh                the dump was written in the last 5 s
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
    a = ap.parse_args()
    stats_path = a.stats or os.path.join(a.prefix, "run", "live-stats.json")

    def evaluate(d):
        meta = d.get("meta") or {}
        res = d.get("residency") or {}
        status = d.get("status") or {}
        our = status.get("our_sub") or {}
        return {
            "connected": d.get("connected") is True,
            "meta_over_socket": bool(meta) and meta.get("meta_source") == "socket",
            "residency_painted": (res.get("status") == "estimate"
                                  and res.get("painted") == res.get("slots")
                                  and (res.get("painted") or 0) > 0),
            "no_forced_drops": our.get("forced_drops") == 0,
            "no_uncounted_drops": (our.get("dropped_droppable") or 0) <= (status.get("drops_total") or 0),
            "frames": (d.get("frames") or 0) >= a.min_frames,
        }

    deadline = time.time() + a.wait
    d, fresh, checks = None, False, {}
    while True:
        d = load(stats_path)
        if d:
            fresh = (time.time() - os.path.getmtime(stats_path)) <= 5.0
            if fresh:
                checks = evaluate(d)
                if all(checks.values()):
                    break
        if time.time() >= deadline:
            break
        time.sleep(0.25)

    if not d:
        print(json.dumps({"stats_path": stats_path, "stats_exists": False, "stats_fresh": False,
                          "ok": False, "why": "no stats dump (app not running?)"}))
        return 1

    meta = d.get("meta") or {}
    res = d.get("residency") or {}
    status = d.get("status") or {}
    our = status.get("our_sub") or {}
    r = {
        "stats_path": stats_path, "stats_exists": True, "stats_fresh": fresh,
        "pid": d.get("pid"), "uptime_s": d.get("uptime_s"), "frames": d.get("frames"),
        "connected": d.get("connected"), "connects": d.get("connects"),
        "reconnects_with_replay": d.get("reconnects_with_replay"),
        "meta_source": meta.get("meta_source"), "meta_model": meta.get("model"),
        "meta_run_id": meta.get("run_id"),
        "residency_status": res.get("status"), "residency_slots": res.get("slots"),
        "residency_painted": res.get("painted"),
        "drops_total": status.get("drops_total"),
        "dropped_ours": our.get("dropped_droppable"), "forced_ours": our.get("forced_drops"),
        "replay_seen": d.get("replay_seen"),
        "placement_verified": (d.get("placement") or {}).get("verified"),
        "events_total": d.get("events_total"), "last_seq": d.get("last_seq"),
    }
    if not checks:
        checks = evaluate(d)
    checks["fresh"] = fresh
    r["checks"] = checks
    r["ok"] = all(checks.values())
    r["why"] = ",".join(k for k, v in checks.items() if not v)
    print(json.dumps(r))
    return 0 if r["ok"] else 1


if __name__ == "__main__":
    sys.exit(main())
