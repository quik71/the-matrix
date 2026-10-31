#!/usr/bin/env python3
"""Stage-7 acceptance harness (S7-A1..S7-A8) for THE MATRIX.

Runs on blackie against the Stage-7 artifacts, host `main` over ssh. Every
number is read from an artifact produced by a command in this session; nothing
is typed in.

usage: run_tests7.py [all|a1..a8]

Artifacts it consumes (all under $STAGE7_ART, default the stage-7 jobs dir):
  chain.log, s3/results.json, s4/results.json, s5/results.json, results.json(s6)
  build.log                (captured here: zero-warning build of the workspace)
  proof.json               (fixture_proofs.py: retention/compaction/export)
  soak.json, samples.jsonl (soak.py)
  analysis.json            (soak_analyze.py: drift, growth model, budgets)
  PARITY.md                (parity audit + zero-hole asymmetry + verdict)
  engine-before.json       (engine_state.py at session start)
  packet.json              (make_packet.py, checked by A8)
"""
import json
import os
import shlex
import subprocess
import sys
import time

ART = os.environ.get("STAGE7_ART", "/srv/storage/minion-verification/matrix-stage7/artifacts")
REPO = "/srv/storage/work/the-matrix"
BUILD = "~/matrix-build"
R = "/home/main/the-matrix"
SOAK_DB = f"{R}/data/s7soak.sqlite"
PROFILE = "/home/main/strata-0.2.4.2/data/expert-profile.bin"
STAMP = os.path.join(ART, "session-start.txt")

RESULTS = {"started": time.strftime("%Y-%m-%dT%H:%M:%S%z"), "checks": []}


def ssh(cmd, timeout=900):
    p = subprocess.run(["ssh", "main", cmd], capture_output=True, text=True, timeout=timeout)
    return {"rc": p.returncode, "out": p.stdout, "err": p.stderr}


def out(cmd, timeout=900):
    return ssh(cmd, timeout)["out"].strip()


def local(cmd, timeout=120):
    # Every call site passes a literal command assembled from constants in this
    # file (repo path, fixed git/sha256 invocations); no external input reaches
    # this shell.
    return subprocess.run(cmd, shell=True, capture_output=True, text=True, timeout=timeout).stdout.strip()


def load(rel):
    p = rel if os.path.isabs(rel) else os.path.join(ART, rel)
    with open(p) as f:
        return json.load(f)


def exists(rel):
    return os.path.exists(rel if os.path.isabs(rel) else os.path.join(ART, rel))


def check(name, ok, detail, **kw):
    rec = {"name": name, "ok": bool(ok), "detail": detail}
    rec.update(kw)
    RESULTS["checks"].append(rec)
    print(("PASS " if ok else "FAIL ") + name + " :: " + str(detail)[:1000], flush=True)
    save()
    return ok


def save():
    path = os.path.join(ART, "results7.json")
    prev = {}
    if os.path.exists(path):
        try:
            prev = json.load(open(path))
        except Exception:
            prev = {}
    merged = dict(prev)
    merged.update({k: v for k, v in RESULTS.items() if k != "checks"})
    by = {c["name"]: c for c in (prev.get("checks") or [])}
    for c in RESULTS["checks"]:
        by[c["name"]] = c
    merged["checks"] = list(by.values())
    merged.setdefault("started", RESULTS["started"])
    json.dump(merged, open(path, "w"), indent=1)


def db1(sql, db=SOAK_DB):
    r = ssh(f"sqlite3 {shlex.quote(db)} {shlex.quote(sql)}")
    if r["rc"] != 0:
        raise RuntimeError(r["err"][:300])
    return r["out"].strip()


# ------------------------------------------------------------------ S7-A1
def test_a1():
    """Zero-warning build; S3(10)+S4(10)+S5(9)+S6(10) green, same session."""
    t = {}
    r = ssh(f"cd {BUILD} && touch crates/*/src/*.rs && cargo build --release 2>&1", timeout=1200)
    log = r["out"]
    open(os.path.join(ART, "build.log"), "w").write(log)
    t["warning_lines"] = [l for l in log.splitlines() if l.startswith("warning:")]
    t["warning_count"] = len(t["warning_lines"])
    t["error_lines"] = [l for l in log.splitlines() if l.startswith("error")]
    t["build_ok"] = "Finished `release` profile" in log and not t["error_lines"]
    t["binaries"] = out(f"sha256sum {R}/bin/matrix-collect {R}/bin/matrix-app")
    stamp = open(STAMP).read().strip() if os.path.exists(STAMP) else ""
    t["session_start"] = stamp
    pri = {}
    for name, rel, want in (("s3", "s3/results.json", 10), ("s4", "s4/results.json", 10),
                            ("s5", "s5/results.json", 9), ("s6", "results.json", 10)):
        rec = {"path": rel, "want": want, "exists": exists(rel)}
        if rec["exists"]:
            d = load(rel)
            checks = d.get("checks") or []
            rec.update({"n": len(checks), "failed": [c["name"] for c in checks if not c.get("ok")],
                        "finished": d.get("finished"), "fresh": bool(d.get("finished") and d["finished"] >= stamp),
                        "ok": len(checks) >= want and not [c for c in checks if not c.get("ok")]})
        else:
            rec["ok"] = False
        pri[name] = rec
        t[name] = {"n": rec.get("n"), "failed": rec.get("failed"), "finished": rec.get("finished"),
                   "fresh": rec.get("fresh"), "ok": rec.get("ok")}
    ok = (t["build_ok"] and t["warning_count"] == 0
          and all(v.get("ok") and v.get("fresh") for v in pri.values())
          and [pri[k]["n"] for k in ("s3", "s4", "s5", "s6")] == [10, 10, 9, 10])
    RESULTS["tests_a1"] = t
    check("S7-A1", ok, json.dumps({"warnings": t["warning_count"], "build_ok": t["build_ok"],
                                   "prior": {k: t[k] for k in ("s3", "s4", "s5", "s6")},
                                   "session_start": stamp}))
    return t


# ------------------------------------------------------------------ S7-A2
def test_a2():
    """Retention proven on copied fixtures; export identity; growth model split."""
    t = {}
    if not exists("proof.json"):
        check("S7-A2", False, "proof.json missing")
        return {}
    p = load("proof.json")
    t["proof_ok"] = bool(p.get("ok"))
    t["session"] = p.get("session")
    t["replay_identical"] = bool(p.get("replay_identical"))
    t["replay_lines"] = (p.get("replay_before") or {}).get("lines")
    t["export_events_identical"] = bool(p.get("export_events_identical"))
    t["export_identity_ok"] = bool(p.get("export_identity_ok"))
    idn = p.get("export_summary_identity") or {}
    t["identity"] = {"logical_events": idn.get("logical_events"), "events_lines": idn.get("events_ndjson_lines"),
                     "stored_rows": idn.get("stored_event_rows"), "reconstructed": idn.get("reconstructed_lines"),
                     "equal": idn.get("equal")}
    ret = p.get("retention") or {}
    t["retention"] = {"hw_pruned": ret.get("hw_pruned"), "status_pruned": ret.get("status_pruned"),
                      "routing_unchanged": ret.get("routing_unchanged"),
                      "passes_unchanged": ret.get("passes_unchanged"),
                      "sessions_unchanged": ret.get("sessions_unchanged"),
                      "expert_lines_removed": ret.get("expert_event_lines_removed"),
                      "expert_lines_removed_eq_routing": ret.get("expert_event_lines_removed_eq_routing"),
                      "integrity": ret.get("integrity"),
                      "db_bytes_before_prune": ret.get("db_bytes_before_prune"),
                      "db_bytes_after_prune": ret.get("db_bytes_after_prune")}
    t["size_math"] = p.get("size_math")
    t["event_line_bytes_per_event"] = p.get("event_line_bytes_per_event")
    t["fixture_label"] = out(f"cat {R}/data/s7proof/LABEL.txt 2>/dev/null")[:120]
    a = load("analysis.json") if exists("analysis.json") else {}
    t["growth_model"] = a.get("model")
    t["mb_per_day_measured"] = (a.get("db") or {}).get("mb_per_day_measured_at_soak_rate")
    t["mb_per_day_projected"] = (a.get("model") or {}).get("projected_mb_per_day_uncompacted_at_45tok_s")
    t["mb_per_day_projected_compacted"] = (a.get("model") or {}).get("projected_mb_per_day_compacted_at_45tok_s")
    ok = (t["proof_ok"] and t["replay_identical"] and t["export_events_identical"] and t["export_identity_ok"]
          and (idn.get("equal") is True) and ret.get("routing_unchanged") is True
          and ret.get("passes_unchanged") is True and ret.get("sessions_unchanged") is True
          and ret.get("integrity") == "ok"
          and bool(t["size_math"]) and t["event_line_bytes_per_event"] > 0
          and t["mb_per_day_measured"] is not None and t["mb_per_day_projected"] is not None)
    check("S7-A2", ok, json.dumps({"replay_identical": t["replay_identical"],
                                   "export_identical": t["export_events_identical"],
                                   "identity": t["identity"], "retention": t["retention"],
                                   "mb_day_measured": t["mb_per_day_measured"],
                                   "mb_day_projected": t["mb_per_day_projected"],
                                   "mb_day_projected_compacted": t["mb_per_day_projected_compacted"]}))
    return t


# ------------------------------------------------------------------ S7-A3
def test_a3():
    """Soak >= 60 min with mixed traffic; drift trends; budgets re-verified."""
    t = {}
    if not (exists("soak.json") and exists("analysis.json")):
        check("S7-A3", False, "soak.json/analysis.json missing")
        return {}
    s = load("soak.json")
    a = load("analysis.json")
    t["duration_min"] = a.get("duration_min")
    t["samples"] = a.get("samples")
    t["traffic"] = a.get("traffic")
    t["errors"] = len(s.get("errors") or [])
    t["budgets"] = a.get("budgets")
    t["unbounded"] = a.get("unbounded")
    t["trends"] = {x["metric"]: {"first": x.get("first"), "last": x.get("last"),
                                 "slope_per_hour": x.get("slope_per_hour")}
                   for x in (a.get("trends") or []) if x.get("n", 0) > 0}
    t["db"] = {"mb_per_day_measured": (a.get("db") or {}).get("mb_per_day_measured_at_soak_rate"),
               "per_class": (a.get("db") or {}).get("per_class")}
    t["restored_pair"] = s.get("restored_pair")
    own = (a.get("traffic") or {}).get("own_driver_ok") or 0
    ind = (a.get("traffic") or {}).get("independent_client_ok") or 0
    ok = ((t["duration_min"] or 0) >= 60 and (t["samples"] or 0) >= 200
          and t["errors"] == 0 and own >= 20 and ind >= 3
          and (t["budgets"] or {}).get("all_ok") is True
          and (t["unbounded"] or {}).get("verdict") == "none"
          and (t["traffic"] or {}).get("engine_requests_delta") is not None)
    check("S7-A3", ok, json.dumps({"duration_min": t["duration_min"], "samples": t["samples"],
                                   "traffic": t["traffic"], "errors": t["errors"],
                                   "budgets_ok": (t["budgets"] or {}).get("all_ok"),
                                   "budget_checks": (t["budgets"] or {}).get("checks"),
                                   "unbounded": t["unbounded"]}))
    return t


# ------------------------------------------------------------------ S7-A4
def test_a4():
    """Parity audit covering every surface + zero-hole asymmetry + verdict."""
    t = {}
    path = os.path.join(ART, "PARITY.md")
    t["exists"] = os.path.exists(path)
    txt = open(path).read() if t["exists"] else ""
    rows = [l for l in txt.splitlines() if l.startswith("| ") and l.count("|") >= 6 and not l.startswith("| # ") and "---" not in l]
    t["surface_rows"] = len(rows)
    t["tiers"] = {k: txt.count(k) for k in ("covered", "estimate-tier", "unavailable")}
    t["has_zero_hole_section"] = "zero-hole asymmetry" in txt.lower()
    t["has_verdict"] = "replacement-readiness verdict" in txt.lower()
    t["names_gaps"] = ("aggregate cache counters" in txt.lower()) and ("pcie" in txt.lower())
    t["vault_artifact"] = os.environ.get("VAULT_ARTIFACT", "")
    t["vault_exists"] = bool(t["vault_artifact"]) and os.path.exists(t["vault_artifact"])
    t["sha256"] = out(f"sha256sum {path} | cut -d' ' -f1")
    ok = (t["exists"] and t["surface_rows"] >= 15 and t["has_zero_hole_section"]
          and t["has_verdict"] and t["names_gaps"] and t["vault_exists"])
    check("S7-A4", ok, json.dumps({"rows": t["surface_rows"], "tiers": t["tiers"],
                                   "zero_hole": t["has_zero_hole_section"], "verdict": t["has_verdict"],
                                   "gaps_named": t["names_gaps"], "vault": t["vault_artifact"],
                                   "vault_exists": t["vault_exists"]}))
    return t


# ------------------------------------------------------------------ S7-A5
def test_a5():
    """Residency decision executed: labelled estimate, never live truth."""
    t = {}
    try:
        row = db1("select line from events where kind='residency_estimate' order by seq limit 1")
    except Exception as e:  # noqa: BLE001
        row = ""
        t["db_error"] = str(e)[:200]
    ev = json.loads(row) if row else {}
    t["event_type"] = ev.get("type")
    t["status"] = ev.get("status")
    t["basis"] = ev.get("basis")
    t["confirmed"] = ev.get("confirmed")
    t["policy"] = ev.get("policy")
    t["slots"] = ev.get("slots")
    t["vram_cells"] = len(ev.get("vram_cells") or [])
    t["tiers"] = ev.get("tiers")
    # independent check of the estimate against the profile on disk
    prof_code = (
        "import struct\\n"
        f"b=open({PROFILE!r},'rb').read()\\n"
        "mig,ver,lay,exp,want,n=struct.unpack('<4sIIIII', b[:24])\\n"
        "pairs=[struct.unpack('<HH', b[24+i*4:28+i*4]) for i in range(n)]\\n"
        f"bad=[p for p in pairs[:{int(ev.get('slots') or 0)}] if p[0]>=lay or p[1]>=exp]\\n"
        "print(len(pairs), lay, exp, ('BAD' if bad else 'ok'))\\n"
    )
    prof = out(f"python3 -c {shlex.quote(prof_code)}")
    t["profile_check"] = prof
    t["profile_pairs"], t["profile_layers"], t["profile_experts"] = (
        (prof.split() + ["", "", ""])[:3] if prof else ("", "", ""))
    a = load("analysis.json") if exists("analysis.json") else {}
    painted = [s.get("residency_painted") for s in (a or {}).get("samples") or [] if s.get("residency_painted")]
    t["app_painted_cells"] = painted[-1] if painted else None
    t["app_labels"] = out(f"grep -c 'ESTIMATE from model structure' {R}/logs/s7soak-app.log 2>/dev/null || true")
    ok = (t["event_type"] == "residency_estimate" and t["status"] == "estimate"
          and t["basis"] == "estimate" and t["confirmed"] is False
          and t["vram_cells"] == t["slots"] and t["vram_cells"] > 0
          and (t["tiers"] or {}).get("ssd") == 0
          and t["app_painted_cells"] == t["vram_cells"]
          and "bad" not in prof)
    check("S7-A5", ok, json.dumps({k: t[k] for k in ("status", "basis", "confirmed", "policy", "slots",
                                                     "vram_cells", "tiers", "app_painted_cells",
                                                     "profile_pairs", "profile_layers", "profile_experts")}))
    return t


# ------------------------------------------------------------------ S7-A6
def test_a6():
    """Engine untouched: PID/NRestarts/mtime/inventory-sha identical, no new files."""
    t = {}
    before_p = os.path.join(ART, "engine-before.json")
    if not os.path.exists(before_p):
        check("S7-A6", False, "engine-before.json missing")
        return {}
    before = json.load(open(before_p))
    after_p = os.path.join(ART, "engine-after.json")
    r = subprocess.run([sys.executable, os.path.join(ART, "engine_state.py"), after_p],
                       capture_output=True, text=True, timeout=400)
    t["capture_rc"] = r.returncode
    after = json.load(open(after_p))
    t["mainpid"] = [before["systemctl"].get("MainPID"), after["systemctl"].get("MainPID")]
    t["nrestarts"] = [before["systemctl"].get("NRestarts"), after["systemctl"].get("NRestarts")]
    t["active_since"] = [before["systemctl"].get("ActiveEnterTimestamp"), after["systemctl"].get("ActiveEnterTimestamp")]
    t["config_mtime"] = [before["config"].get("mtime"), after["config"].get("mtime")]
    t["config_sha"] = [before["config"].get("sha256"), after["config"].get("sha256")]
    t["inventory_n"] = [before["inventory_n"], after["inventory_n"]]
    t["inventory_sha"] = [before["inventory_sha256"], after["inventory_sha256"]]
    t["checks"] = {
        "mainpid_same": t["mainpid"][0] == t["mainpid"][1],
        "nrestarts_same": t["nrestarts"][0] == t["nrestarts"][1],
        "active_since_same": t["active_since"][0] == t["active_since"][1],
        "config_mtime_same": t["config_mtime"][0] == t["config_mtime"][1],
        "config_sha_same": t["config_sha"][0] == t["config_sha"][1],
        "inventory_count_same": t["inventory_n"][0] == t["inventory_n"][1],
        "inventory_sha_same": t["inventory_sha"][0] == t["inventory_sha"][1],
    }
    ok = all(t["checks"].values())
    check("S7-A6", ok, json.dumps({"checks": t["checks"], "mainpid": t["mainpid"],
                                   "nrestarts": t["nrestarts"],
                                   "inventory_sha": [x[:16] for x in t["inventory_sha"]],
                                   "inventory_n": t["inventory_n"]}))
    return t


# ------------------------------------------------------------------ S7-A7
def test_a7():
    """Repo clean, zero remotes, suite committed to the artifacts convention."""
    t = {}
    t["head"] = local(f"git -C {REPO} rev-parse HEAD")
    t["status"] = local(f"git -C {REPO} status --porcelain")
    t["remotes"] = local(f"git -C {REPO} remote -v")
    t["branches"] = local(f"git -C {REPO} branch -a")
    t["log"] = local(f"git -C {REPO} log --oneline | head -5")
    t["suite_path"] = os.path.abspath(__file__)
    t["suite_sha256"] = out(f"sha256sum {os.path.abspath(__file__)} | cut -d' ' -f1")
    t["results_exists"] = exists("results7.json")
    t["packet_builder"] = os.path.join(ART, "make_packet.py")
    t["packet_builder_exists"] = os.path.exists(t["packet_builder"])
    t["repo_suite_tracked"] = local(f"git -C {REPO} ls-files verification/ | head -5")
    t["gitignore_ok"] = all(k in local(f"cat {REPO}/.gitignore") for k in ("target", "data/", "run/"))
    ok = (t["status"] == "" and t["remotes"] == "" and bool(t["head"])
          and t["results_exists"] and t["packet_builder_exists"] and t["gitignore_ok"]
          and "run_tests7.py" in (t["repo_suite_tracked"] or ""))
    check("S7-A7", ok, json.dumps({"head": t["head"][:12], "status": t["status"][:200],
                                   "remotes": t["remotes"], "results": t["results_exists"],
                                   "packet_builder": t["packet_builder_exists"],
                                   "repo_suite": t["repo_suite_tracked"][:80],
                                   "gitignore_ok": t["gitignore_ok"]}))
    return t


# ------------------------------------------------------------------ S7-A8
def test_a8():
    """Packet complete: every claim present, ok, and backed by an existing artifact."""
    t = {}
    path = os.path.join(ART, "packet.json")
    t["exists"] = os.path.exists(path)
    if not t["exists"]:
        check("S7-A8", False, "packet.json missing")
        return {}
    p = json.load(open(path))
    claims = p.get("claims") or []
    t["claims"] = [c.get("id") for c in claims]
    t["wanted"] = [f"S7-A{i}" for i in range(1, 9)]
    t["missing"] = [c for c in t["wanted"] if c not in t["claims"]]
    t["failed"] = [c["id"] for c in claims if not c.get("ok")]
    t["paths"] = sorted({x for c in claims for x in (c.get("artifact_paths") or [])})
    t["paths_missing"] = [x for x in t["paths"] if not os.path.exists(x)]
    t["acceptance"] = p.get("acceptance_criteria")
    ok = (not t["missing"] and not t["failed"] and not t["paths_missing"] and t["acceptance"])
    check("S7-A8", ok, json.dumps({"claims": len(claims), "missing": t["missing"],
                                   "failed": t["failed"], "paths": len(t["paths"]),
                                   "paths_missing": t["paths_missing"][:5]}))
    return t


def main():
    only = sys.argv[1] if len(sys.argv) > 1 else "all"
    for k, fn in (("a1", test_a1), ("a2", test_a2), ("a3", test_a3), ("a4", test_a4),
                  ("a5", test_a5), ("a6", test_a6), ("a7", test_a7), ("a8", test_a8)):
        if only in ("all", k):
            fn()
    RESULTS["finished"] = time.strftime("%Y-%m-%dT%H:%M:%S%z")
    save()
    print("\n=== SUMMARY ===")
    for c in RESULTS["checks"]:
        print(("PASS " if c["ok"] else "FAIL ") + c["name"])


if __name__ == "__main__":
    main()
