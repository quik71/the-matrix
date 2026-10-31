#!/usr/bin/env python3
"""Turn soak samples into drift trends, a DB growth model and a budget verdict.

Input : soak.json + samples.jsonl (written by soak.py)
Output: analysis.json  (all numbers computed here, none typed by hand)
"""
import json
import os
import statistics

ART = os.path.dirname(os.path.abspath(__file__))
S = json.load(open(os.path.join(ART, "soak.json")))
samples = S.get("samples") or []
if not samples and os.path.exists(os.path.join(ART, "samples.jsonl")):
    samples = [json.loads(l) for l in open(os.path.join(ART, "samples.jsonl")) if l.strip()]

OUT = {"source": "analysis of soak.json/samples.jsonl", "samples": len(samples)}


def series(fn):
    xs, ys = [], []
    for s in samples:
        v = fn(s)
        if v is None:
            continue
        xs.append(s["ts"])
        ys.append(float(v))
    return xs, ys


def slope_per_hour(xs, ys):
    if len(xs) < 3:
        return None
    t0 = xs[0]
    X = [(x - t0) / 3600.0 for x in xs]
    n = len(X)
    mx = sum(X) / n
    my = sum(ys) / n
    den = sum((x - mx) ** 2 for x in X)
    if den == 0:
        return 0.0
    return sum((x - mx) * (y - my) for x, y in zip(X, ys)) / den


def clean(v):
    if v is None:
        return None
    return round(v, 3)


def trend(name, fn, unit):
    xs, ys = series(fn)
    if not xs:
        return {"metric": name, "n": 0}
    return {
        "metric": name, "unit": unit, "n": len(ys),
        "first": clean(ys[0]), "last": clean(ys[-1]),
        "min": clean(min(ys)), "max": clean(max(ys)),
        "median": clean(statistics.median(ys)),
        "slope_per_hour": clean(slope_per_hour(xs, ys)),
        "range": clean(max(ys) - min(ys)),
    }


OUT["duration_min"] = round((samples[-1]["ts"] - samples[0]["ts"]) / 60.0, 2) if len(samples) > 1 else None
OUT["trends"] = [
    trend("app_rss_mib", lambda s: (s.get("app") or {}).get("rss_kib", 0) / 1024.0 or None, "MiB"),
    trend("app_hwm_mib", lambda s: (s.get("app") or {}).get("hwm_kib", 0) / 1024.0 or None, "MiB"),
    trend("app_fds", lambda s: (s.get("app") or {}).get("fds") or None, "fds"),
    trend("app_frame_p95_ms_interval", lambda s: s.get("p95_interval_ms"), "ms"),
    trend("app_frame_p95_cumulative_ms", lambda s: (s.get("frame_ms") or {}).get("p95"), "ms"),
    trend("coll_rss_mib", lambda s: (s.get("coll") or {}).get("rss_kib", 0) / 1024.0 or None, "MiB"),
    trend("coll_cpu_pct", lambda s: None, "%"),
    trend("coll_fds", lambda s: (s.get("coll") or {}).get("fds") or None, "fds"),
    trend("queue_depth", lambda s: (s.get("queue") or {}).get("depth"), "events"),
    trend("queue_max_depth", lambda s: (s.get("queue") or {}).get("max_depth"), "events"),
    trend("trace_offset_mib", lambda s: ((s.get("queue") or {}).get("trace_offset") or 0) / 1048576.0 or None, "MiB"),
    trend("db_bytes_mib", lambda s: ((s.get("db") or {}).get("db_bytes") or 0) / 1048576.0 or None, "MiB"),
    trend("routing_rows", lambda s: (s.get("db") or {}).get("routing"), "rows"),
    trend("records", lambda s: s.get("records"), "records"),
]

# collector CPU from tick deltas between consecutive samples
cc = []
for a, b in zip(samples, samples[1:]):
    ta = (a.get("coll") or {}).get("ticks")
    tb = (b.get("coll") or {}).get("ticks")
    dt = b["ts"] - a["ts"]
    if ta is None or tb is None or dt <= 0:
        continue
    cc.append(((tb - ta) / 100.0) / dt * 100.0)
if cc:
    OUT["collector_cpu"] = {"samples": len(cc), "mean_pct": round(statistics.mean(cc), 3),
                            "p95_pct": round(sorted(cc)[int(0.95 * (len(cc) - 1))], 3),
                            "max_pct": round(max(cc), 3)}

# ---------------------------------------------------------------- DB growth
c0, c1 = S.get("db_counts_start") or {}, S.get("db_counts_end") or {}
b0, b1 = S.get("dbstat_start") or {}, S.get("dbstat_end") or {}
dur_min = OUT["duration_min"] or 0.0
db0, db1 = c0.get("db_bytes"), c1.get("db_bytes")
rows = {}
for k in ("routing", "expert_events", "passes", "sessions", "perf", "hw", "status", "gaps", "events"):
    if k in c0 and k in c1:
        rows[k] = {"start": c0[k], "end": c1[k], "delta": c1[k] - c0[k],
                   "per_min": round((c1[k] - c0[k]) / dur_min, 1) if dur_min else None}

TABLE_KEYS = {
    "routing": ["routing", "routing_session"],
    "events": ["events", "events_kind", "events_session"],
    "passes": ["passes", "sqlite_autoindex_passes_1"],
    "sessions": ["sessions"],
    "perf": ["perf"],
    "hw": ["hw"],
    "status": ["status"],
    "gaps": ["gaps"],
}
starts = {}
for k, keys in TABLE_KEYS.items():
    starts[k] = sum(b0.get(x, 0) for x in keys)
ends = {}
for k, keys in TABLE_KEYS.items():
    ends[k] = sum(b1.get(x, 0) for x in keys)

per_class = {}
for k in TABLE_KEYS:
    n1 = c1.get(k if k != "events" else "events", 0)
    n0 = c0.get(k if k != "events" else "events", 0)
    d_bytes = ends[k] - starts[k]
    per_class[k] = {
        "rows_start": n0, "rows_end": n1, "rows_delta": n1 - n0,
        "bytes_start": starts[k], "bytes_end": ends[k], "bytes_delta": d_bytes,
        "bytes_per_row_end": round(ends[k] / n1, 1) if n1 else None,
        "mb_per_day_at_soak_rate": round(d_bytes / dur_min * 1440 / 1e6, 2) if dur_min else None,
    }
OUT["db"] = {
    "duration_min": dur_min,
    "bytes_start": db0, "bytes_end": db1, "bytes_delta": (db1 - db0) if None not in (db0, db1) else None,
    "rows": rows,
    "per_class": per_class,
    "dbstat_available": bool(b1),
    "mb_per_day_measured_at_soak_rate": round((db1 - db0) / dur_min * 1440 / 1e6, 2) if dur_min and None not in (db0, db1) else None,
}

# ------------------------------------------------- growth model: measured vs projected
rec_delta = rows.get("routing", {}).get("delta", 0)
rec_per_s = rec_delta / (dur_min * 60.0) if dur_min else 0
rec_bytes = per_class.get("routing", {}).get("bytes_per_row_end") or 0
ev_bytes = per_class.get("events", {}).get("bytes_delta", 0)
ev_per_row = (ev_bytes / rec_delta) if rec_delta else 0
pass_per_row = (per_class.get("passes", {}).get("bytes_delta", 0) / rec_delta) if rec_delta else 0
sess_per_row = (per_class.get("sessions", {}).get("bytes_delta", 0) / rec_delta) if rec_delta else 0
SUSTAINED_TOK_S = 45.0
REC_PER_TOKEN = 48
sustained_rec_s = SUSTAINED_TOK_S * REC_PER_TOKEN
compaction_horizon_h = 1.0
OUT["model"] = {
    "records_per_s_measured": round(rec_per_s, 1),
    "bytes_per_routing_row_measured": round(rec_bytes, 1),
    "event_line_bytes_per_record_measured": round(ev_per_row, 1),
    "pass_bytes_per_record_measured": round(pass_per_row, 2),
    "session_bytes_per_record_measured": round(sess_per_row, 2),
    "measured_mb_per_day_at_soak_duty": round((rec_per_s * 86400 * (rec_bytes + ev_per_row + pass_per_row + sess_per_row)) / 1e6, 2) if rec_per_s else None,
    "projected_mb_per_day_uncompacted_at_45tok_s": round((sustained_rec_s * 86400 * (rec_bytes + ev_per_row + pass_per_row + sess_per_row)) / 1e6, 2),
    "projected_mb_per_day_compacted_at_45tok_s": round((sustained_rec_s * 86400 * (rec_bytes + pass_per_row + sess_per_row)) / 1e6, 2),
    "projected_event_buffer_mb_at_horizon": round((sustained_rec_s * 3600 * compaction_horizon_h * ev_per_row) / 1e6, 1),
    "assumption": "projected = 45 tok/s x 48 layers/token sustained (100% duty) x measured bytes/row; measured = the soak's own duty cycle. The two are reported separately and never mixed.",
}

# ---------------------------------------------------------------- traffic mix
e0 = (S.get("engine_at_start") or {}).get("requests")
e1 = (S.get("engine_at_end") or {}).get("requests")
own = S.get("own_ok", 0)
ind = S.get("independent_ok", 0)
eng = (e1 - e0) if None not in (e0, e1) else None
OUT["traffic"] = {"engine_requests_delta": eng, "own_driver_ok": own, "independent_client_ok": ind,
                  "foreign_unaccounted": (eng - own - ind) if eng is not None else None,
                  "own_gens": len(S.get("own_gens") or []),
                  "paths": sorted({g.get("path") for g in (S.get("own_gens") or []) if g.get("ok")})}
sess = (S.get("db_counts_end") or {}).get("sessions")
OUT["traffic"]["collector_sessions"] = sess
OUT["traffic"]["session_request_delta_sum"] = None

# ---------------------------------------------------------------- budgets
bp = S.get("budget_post_soak") or {}
p95 = (bp.get("frame_ms_after") or {}).get("p95")
checks = {
    "rain_on_cpu_le_5pct": (bp.get("cpu_pct_60s_rain_on") is not None and bp["cpu_pct_60s_rain_on"] <= 5.0),
    "app_vram_le_512mib": (bp.get("app_vram_peak_mib") is not None and bp["app_vram_peak_mib"] <= 512),
    "rain_off_spurious_zero": ((bp.get("rain_off") or {}).get("frames_spurious") == 0),
    "rain_off_frames_le_1": ((bp.get("rain_off") or {}).get("frames") is not None and (bp["rain_off"]["frames"] <= 1)),
    "rain_off_cpu_le_0_5pct": (bp.get("cpu_pct_60s_rain_off") is not None and bp["cpu_pct_60s_rain_off"] <= 0.5),
    "frame_p95_le_33ms": (p95 is not None and p95 <= 33.3),
}
OUT["budgets"] = {"measured": {k: bp.get(k) for k in ("cpu_pct_60s_rain_on", "app_vram_peak_mib",
                                                      "cpu_pct_60s_rain_off", "rain_off",
                                                      "frames_during_cpu_window", "uptime_s", "rss_kib", "fds")},
                  "frame_ms_after": bp.get("frame_ms_after"),
                  "checks": checks, "all_ok": all(checks.values())}

# ------------------------------------------------- leakage / unbounded growth
leaks = []
for t in OUT["trends"]:
    if t.get("n", 0) < 5 or t.get("slope_per_hour") is None:
        continue
    name, sl, first, last = t["metric"], t["slope_per_hour"], t.get("first"), t.get("last")
    if name in ("app_rss_mib", "app_hwm_mib", "coll_rss_mib", "app_fds", "coll_fds", "queue_depth", "queue_max_depth"):
        if first and sl is not None and sl > max(2.0, 0.02 * abs(first)):
            leaks.append({"metric": name, "slope_per_hour": sl, "first": first, "last": last,
                          "verdict": "grows faster than 2%/h or 2 units/h"})
OUT["unbounded"] = {"suspects": leaks, "verdict": "none" if not leaks else "review"}

OUT["errors"] = S.get("errors", [])[:20]
OUT["restored_pair"] = S.get("restored_pair")
json.dump(OUT, open(os.path.join(ART, "analysis.json"), "w"), indent=1)

print(json.dumps({k: OUT[k] for k in ("duration_min", "traffic", "budgets", "unbounded",
                                      "mb_per_day" if "mb_per_day" in OUT else "db")}, indent=1)[:4000])
print("MODEL", json.dumps(OUT["model"], indent=1))
print("TRENDS")
for t in OUT["trends"]:
    print(" ", t["metric"], t.get("first"), "->", t.get("last"), "slope/h", t.get("slope_per_hour"), t["unit"])
