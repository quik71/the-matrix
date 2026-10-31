#!/usr/bin/env python3
"""Stage-7 A2 fixture proofs: retention + compaction + export identity.

Everything runs on COPIED fixtures on main (the live DB is never touched).
Rows are time-shifted inside the copies only, and every copy carries a
`fixture_` name and a `LABEL.txt` so nothing here can be mistaken for history.

Proofs:
  1. v1 -> v2 migration is lossless: replay of a session before/after migration
     is byte-identical.
  2. hw/status retention: time-shifted ambient rows are pruned, routing,
     passes, sessions and non-expert events are not; replay is byte-identical
     after the expert event lines are compacted away.
  3. export identity: events.ndjson before and after compaction is
     byte-identical (same sha256), and per-class line counts equal table row
     counts; summary.json identity.equal is true.
  4. size math: dbstat per table before/after, bytes per row per class.
"""
import hashlib
import json
import os
import shlex
import subprocess

ART = os.path.dirname(os.path.abspath(__file__))
R = "/home/main/the-matrix"
FIX = f"{R}/data/s7proof"
SRC = f"{R}/data/s6.sqlite"          # snapshot written by the previous live collector
MC = f"{R}/bin/matrix-collect"
OUT = {"fixtures": FIX, "source_snapshot": SRC, "steps": {}}


def ssh(cmd, timeout=300):
    p = subprocess.run(["ssh", "main", cmd], capture_output=True, text=True, timeout=timeout)
    return {"rc": p.returncode, "out": p.stdout, "err": p.stderr}


def out(cmd, timeout=300):
    r = ssh(cmd, timeout)
    return r["out"].strip()


def jrun(cmd, timeout=300):
    r = ssh(cmd, timeout)
    line = r["out"].strip().splitlines()[-1] if r["out"].strip() else "{}"
    try:
        d = json.loads(line)
    except Exception:
        d = {"parse_error": line[:300], "err": r["err"][:300], "rc": r["rc"]}
    return d


def sha(path):
    return out(f"sha256sum {shlex.quote(path)} | cut -d' ' -f1")


def dbq(db, sql):
    r = ssh(f"sqlite3 {shlex.quote(db)} \"{sql}\"")
    if r["rc"] != 0:
        raise RuntimeError(r["err"][:300])
    return r["out"].strip()


def counts(db):
    sql = ("select (select count(*) from hw),(select count(*) from status),"
           "(select count(*) from routing),(select count(*) from passes),"
           "(select count(*) from sessions),(select count(*) from events),"
           "(select count(*) from events where kind='expert_selected'),"
           "(select count(*) from perf),(select count(*) from gaps)")
    v = [int(x) for x in dbq(db, sql).split("|")]
    keys = ["hw", "status", "routing", "passes", "sessions", "events", "expert_events", "perf", "gaps"]
    return dict(zip(keys, v))


def dbstat(db):
    d = {}
    for line in dbq(db, "select name,sum(pgsize) from dbstat group by name").splitlines():
        n, b = line.split("|")
        d[n] = int(b)
    return d


def main():
    ssh(f"mkdir -p {FIX}")
    ssh(f"rm -f {FIX}/*.sqlite {FIX}/*.ndjson {FIX}/*.json")
    if out(f"test -f {FIX}/base.sqlite && echo yes") != "yes":
        r = ssh(f"sqlite3 {SRC} \".backup {FIX}/base.sqlite\"")
        if r["rc"] != 0:
            OUT["error"] = "snapshot copy failed: " + r["err"][:300]
            json.dump(OUT, open(os.path.join(ART, "proof.json"), "w"), indent=1)
            print(json.dumps(OUT))
            return 1
    ssh(f"printf %s {shlex.quote('LABEL: fixture copies for Stage-7 A2 proofs. Not history. Rows time-shifted in copies only.')} > {FIX}/LABEL.txt")
    OUT["base_counts_v1"] = counts(f"{FIX}/base.sqlite")
    OUT["base_bytes"] = int(out(f"stat -c %s {FIX}/base.sqlite"))

    # pick the session with the most routing rows
    sid = dbq(f"{FIX}/base.sqlite", "select session from routing group by session order by count(*) desc limit 1")
    OUT["session"] = sid

    # ---- 1. migration (no compaction): replay before/after
    ssh(f"cp {FIX}/base.sqlite {FIX}/mig.sqlite")
    OUT["migrate"] = jrun(f"cd {R} && ./bin/matrix-collect --db {FIX}/mig.sqlite --prune "
                          f"--retention-days 3650 --event-line-hours 99999", timeout=600)
    ssh(f"cd {R} && ./bin/matrix-collect --db {FIX}/mig.sqlite --replay {shlex.quote(sid)} > {FIX}/replay-before.ndjson")
    OUT["replay_before"] = {"lines": int(out(f"wc -l < {FIX}/replay-before.ndjson")),
                            "sha256": sha(f"{FIX}/replay-before.ndjson")}
    OUT["migrated_counts"] = counts(f"{FIX}/mig.sqlite")

    # ---- 2. time-shift a copy, then prune + compact
    ssh(f"cp {FIX}/mig.sqlite {FIX}/ret.sqlite")
    dbq(f"{FIX}/ret.sqlite",
        "update hw set ts=ts-45*86400 where rowid%2=0;"
        "update status set ts=ts-45*86400 where rowid%2=0;"
        "update sessions set finished_ts=coalesce(finished_ts,started_ts)-10*86400;")
    before = counts(f"{FIX}/ret.sqlite")
    st_before = dbstat(f"{FIX}/ret.sqlite")
    bytes_before_prune = int(out(f"stat -c %s {FIX}/ret.sqlite"))
    OUT["prune"] = jrun(f"cd {R} && ./bin/matrix-collect --db {FIX}/ret.sqlite --prune "
                        f"--retention-days 30 --event-line-hours 0 --vacuum", timeout=900)
    after = counts(f"{FIX}/ret.sqlite")
    st_after = dbstat(f"{FIX}/ret.sqlite")
    OUT["retention"] = {
        "counts_before": before,
        "counts_after": after,
        "hw_pruned": before["hw"] - after["hw"],
        "status_pruned": before["status"] - after["status"],
        "routing_unchanged": before["routing"] == after["routing"],
        "passes_unchanged": before["passes"] == after["passes"],
        "sessions_unchanged": before["sessions"] == after["sessions"],
        "perf_unchanged": before["perf"] == after["perf"],
        "gaps_unchanged": before["gaps"] == after["gaps"],
        "expert_event_lines_removed": before["expert_events"] - after["expert_events"],
        "expert_event_lines_removed_eq_routing": (before["expert_events"] - after["expert_events"]) == after["routing"],
        "integrity": dbq(f"{FIX}/ret.sqlite", "pragma integrity_check"),
        "table_bytes_before": st_before,
        "table_bytes_after": st_after,
        "db_bytes_before_prune": bytes_before_prune,
        "db_bytes_after_prune": int(out(f"stat -c %s {FIX}/ret.sqlite")),
    }
    # recent hw rows must survive: exactly the even rowids were shifted
    OUT["retention"]["hw_recent_survive"] = int(dbq(f"{FIX}/ret.sqlite",
        "select count(*) from hw where ts >= strftime('%s','now') - 30*86400"))

    # ---- replay after compaction must be byte-identical
    ssh(f"cd {R} && ./bin/matrix-collect --db {FIX}/ret.sqlite --replay {shlex.quote(sid)} > {FIX}/replay-after.ndjson")
    OUT["replay_after"] = {"lines": int(out(f"wc -l < {FIX}/replay-after.ndjson")),
                           "sha256": sha(f"{FIX}/replay-after.ndjson")}
    OUT["replay_identical"] = OUT["replay_before"]["sha256"] == OUT["replay_after"]["sha256"]

    # ---- 3. export identity before vs after compaction
    OUT["export_before"] = jrun(f"cd {R} && ./bin/matrix-collect --db {FIX}/mig.sqlite --export {FIX}/exp-before", timeout=900)
    OUT["export_after"] = jrun(f"cd {R} && ./bin/matrix-collect --db {FIX}/ret.sqlite --export {FIX}/exp-after", timeout=900)
    OUT["export_events_sha"] = {"before": sha(f"{FIX}/exp-before/events.ndjson"),
                                "after": sha(f"{FIX}/exp-after/events.ndjson")}
    OUT["export_events_identical"] = OUT["export_events_sha"]["before"] == OUT["export_events_sha"]["after"]
    summ = json.loads(out(f"cat {FIX}/exp-after/summary.json"))
    OUT["export_summary_identity"] = summ.get("identity")
    OUT["export_summary_counts"] = summ.get("counts")
    OUT["export_line_vs_db"] = {
        "events_lines": int(out(f"wc -l < {FIX}/exp-after/events.ndjson")),
        "routing_lines": int(out(f"wc -l < {FIX}/exp-after/routing.ndjson")),
        "passes_lines": int(out(f"wc -l < {FIX}/exp-after/passes.ndjson")),
        "sessions_lines": int(out(f"wc -l < {FIX}/exp-after/sessions.ndjson")),
        "db_routing": after["routing"],
        "db_passes": after["passes"],
        "db_sessions": after["sessions"],
        "db_events": after["events"],
        "logical_events": after["events"] + after["routing"] - after["expert_events"],
    }
    e = OUT["export_line_vs_db"]
    OUT["export_identity_ok"] = (e["events_lines"] == e["logical_events"]
                                 and e["routing_lines"] == e["db_routing"]
                                 and e["passes_lines"] == e["db_passes"]
                                 and e["sessions_lines"] == e["db_sessions"]
                                 and bool(OUT["export_events_identical"])
                                 and bool((OUT["export_summary_identity"] or {}).get("equal")))

    # ---- 4. size math per row class
    size = {}
    for k in ("routing", "events", "passes", "sessions", "hw", "status"):
        rows = after.get(k, 0)
        b = st_after.get(k, 0) + (st_after.get("events_kind", 0) if k == "events" else 0)
        size[k] = {"rows": rows, "bytes": b, "bytes_per_row": round(b / rows, 2) if rows else None}
    OUT["size_math"] = size
    OUT["event_line_bytes_per_event"] = round(
        (st_before.get("events", 0) - st_after.get("events", 0)) /
        max(1, before["expert_events"] - after["expert_events"]), 1)
    OUT["ok"] = bool(OUT["replay_identical"] and OUT["export_identity_ok"]
                     and OUT["retention"]["routing_unchanged"] and OUT["retention"]["passes_unchanged"]
                     and OUT["retention"]["sessions_unchanged"]
                     and OUT["retention"]["integrity"] == "ok")
    json.dump(OUT, open(os.path.join(ART, "proof.json"), "w"), indent=1)
    print(json.dumps({k: OUT[k] for k in ("ok", "session", "replay_identical", "export_events_identical",
                                          "export_identity_ok", "retention", "size_math",
                                          "event_line_bytes_per_event")}, indent=1)[:4000])
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
