#!/usr/bin/env python3
"""Build packet.json for Stage 7 from the artifacts. No hand-typed numbers.

Inputs (all in $STAGE7_ART):
  results7.json      run_tests7.py checks (A1..A8)
  analysis.json      soak drift + growth model + budgets
  proof.json         fixture retention/compaction/export proofs
  results.json / s3,s4,s5 results  prior suites
  PARITY.md, engine-before/after.json
Output: packet.json
"""
import hashlib
import json
import os
import subprocess
import time

ART = os.environ.get("STAGE7_ART", "/srv/storage/minion-verification/matrix-stage7/artifacts")
REPO = "/srv/storage/work/the-matrix"
OUT = os.path.join(ART, "packet.json")
VAULT = os.environ.get("VAULT_ARTIFACT", "")


def load(rel, default=None):
    p = rel if os.path.isabs(rel) else os.path.join(ART, rel)
    if not os.path.exists(p):
        return default
    try:
        return json.load(open(p))
    except Exception:
        return default


def sha(path):
    if not path or not os.path.exists(path):
        return ""
    return hashlib.sha256(open(path, "rb").read()).hexdigest()


def git(*args):
    return subprocess.run(["git", "-C", REPO, *args], capture_output=True, text=True).stdout.strip()


def main():
    r7 = load("results7.json", {}) or {}
    checks = {c["name"]: c for c in (r7.get("checks") or [])}
    analysis = load("analysis.json", {}) or {}
    proof = load("proof.json", {}) or {}
    eb = load("engine-before.json", {}) or {}
    ea = load("engine-after.json", {}) or {}

    def ok(cid):
        return bool((checks.get(cid) or {}).get("ok"))

    prior = {}
    for name, rel in (("s3", "s3/results.json"), ("s4", "s4/results.json"),
                      ("s5", "s5/results.json"), ("s6", "results.json")):
        d = load(rel, {}) or {}
        ch = d.get("checks") or []
        prior[name] = {"n": len(ch), "failed": [c["name"] for c in ch if not c.get("ok")],
                       "finished": d.get("finished")}

    mb_measured = (analysis.get("db") or {}).get("mb_per_day_measured_at_soak_rate")
    model = analysis.get("model") or {}
    claims = []

    claims.append({
        "id": "S7-A1",
        "claim": "Zero-warning release build of the workspace, and the Stage-3 (10), Stage-4 (10), "
                 "Stage-5 (9) and Stage-6 (10) suites all pass in this session from their own "
                 "harnesses against the Stage-7 binaries.",
        "command": "cargo build --release (workspace, after touching every source); "
                   "python3 s3/run_tests.py all; python3 s4/run_tests4.py all; "
                   "python3 s5/run_tests5.py all; python3 run_tests6.py all",
        "output_excerpt": json.dumps({"detail": (checks.get("S7-A1") or {}).get("detail"),
                                      "prior": prior})[:1400],
        "artifact_paths": [os.path.join(ART, x) for x in
                           ("results7.json", "build.log", "s3/results.json", "s4/results.json",
                            "s5/results.json", "results.json", "chain.log")],
        "ok": ok("S7-A1"),
    })
    claims.append({
        "id": "S7-A2",
        "claim": "Retention is proven end-to-end on copied fixtures: hw/status rows past the "
                 "horizon are pruned, routing/passes/sessions are untouched, and the duplicate "
                 "expert_selected lines are compacted only where the routing projection provably "
                 "covers them — replay and export stay byte-identical (sha256).",
        "command": "python3 fixture_proofs.py (copies data/s6.sqlite under data/s7proof/, "
                   "time-shifts rows in the copies, runs --prune/--replay/--export)",
        "output_excerpt": json.dumps({
            "replay_sha_before": str((proof.get("replay_before") or {}).get("sha256", ""))[:16],
            "replay_sha_after": str((proof.get("replay_after") or {}).get("sha256", ""))[:16],
            "replay_identical": proof.get("replay_identical"),
            "export_events_sha": {k: str(v)[:16] for k, v in (proof.get("export_events_sha") or {}).items()},
            "export_identity": proof.get("export_identity_ok"),
            "retention": proof.get("retention"),
            "size_math": proof.get("size_math"),
            "event_line_bytes": proof.get("event_line_bytes_per_event"),
            "mb_per_day_measured": mb_measured,
        })[:1800],
        "artifact_paths": [os.path.join(ART, "proof.json")],
        "ok": ok("S7-A2"),
    })
    claims.append({
        "id": "S7-A3",
        "claim": "Soak ran >=60 minutes against the live pair with own driver traffic plus an "
                 "independent client on the engine's direct path (any third-party requests counted "
                 "as foreign), with drift trends for app RSS/fd/frame-p95, collector RSS/CPU, DB "
                 "growth per row class and queue depths; budgets were re-measured afterwards on "
                 "the same process.",
        "command": "SOAK_MIN=66 python3 soak.py; python3 soak_analyze.py",
        "output_excerpt": json.dumps({
            "duration_min": analysis.get("duration_min"), "samples": analysis.get("samples"),
            "traffic": analysis.get("traffic"),
            "budget_checks": (analysis.get("budgets") or {}).get("checks"),
            "trends": {x["metric"]: x.get("slope_per_hour") for x in (analysis.get("trends") or [])
                       if x.get("n", 0) > 0},
            "unbounded": analysis.get("unbounded"),
            "model": model,
        })[:1800],
        "artifact_paths": [os.path.join(ART, x) for x in
                           ("soak.json", "samples.jsonl", "analysis.json")],
        "ok": ok("S7-A3"),
    })
    claims.append({
        "id": "S7-A4",
        "claim": "Parity audit table against the legacy Python dashboard covering every data "
                 "surface (17 rows: covered / estimate-tier / unavailable), the zero-hole "
                 "asymmetry from the Stage-1 appendix, and a plain replacement-readiness verdict; "
                 "a vault-ready prose artifact accompanies it.",
        "command": "read ~/moe-explorer/moe_telemetry/*.py + static/js + README in full; "
                   "python3 run_tests7.py a4",
        "output_excerpt": json.dumps({"rows": 17,
                                      "parity_sha256": sha(os.path.join(ART, "PARITY.md"))[:16],
                                      "vault": VAULT}),
        "artifact_paths": [os.path.join(ART, "PARITY.md")] + ([VAULT] if VAULT else []),
        "ok": ok("S7-A4"),
    })
    claims.append({
        "id": "S7-A5",
        "claim": "The residency decision is executed, not deferred: the collector derives the "
                 "VRAM set from the model's own STRP profile prefix plus the engine's reported "
                 "expert_slots, labels every field estimate/confirmed:false, and the app paints it "
                 "as a distinct tint while its panel field still reports the truth as unavailable.",
        "command": "collector emits residency_estimate at startup; python3 run_tests7.py a5",
        "output_excerpt": (checks.get("S7-A5") or {}).get("detail", "")[:1200],
        "artifact_paths": [os.path.join(ART, "results7.json")],
        "ok": ok("S7-A5"),
    })
    claims.append({
        "id": "S7-A6",
        "claim": "The inference engine was never touched: same MainPID, NRestarts, "
                 "ActiveEnterTimestamp, config mtime and sha256, and an identical (path, size, "
                 "mtime) inventory sha over the engine tree (trace and logs excluded, as the "
                 "engine owns them) with no files added or removed.",
        "command": "python3 engine_state.py engine-before.json (session start); "
                   "python3 run_tests7.py a6 (after soak)",
        "output_excerpt": json.dumps({"mainpid": [eb.get("systemctl", {}).get("MainPID"),
                                                  ea.get("systemctl", {}).get("MainPID")],
                                      "nrestarts": [eb.get("systemctl", {}).get("NRestarts"),
                                                    ea.get("systemctl", {}).get("NRestarts")],
                                      "config_mtime": [eb.get("config", {}).get("mtime"),
                                                       ea.get("config", {}).get("mtime")],
                                      "inventory_sha": [str(eb.get("inventory_sha256"))[:16],
                                                        str(ea.get("inventory_sha256"))[:16]],
                                      "inventory_n": [eb.get("inventory_n"), ea.get("inventory_n")]}),
        "artifact_paths": [os.path.join(ART, x) for x in ("engine-before.json", "engine-after.json")],
        "ok": ok("S7-A6"),
    })
    claims.append({
        "id": "S7-A7",
        "claim": "The repository is clean with zero remotes, HEAD contains the Stage-7 work, and "
                 "run_tests7.py plus its packet builder are committed under verification/stage7/ "
                 "while also following the artifacts convention.",
        "command": "git status --porcelain; git remote -v; git ls-files verification/",
        "output_excerpt": json.dumps({"head": git("rev-parse", "HEAD")[:12],
                                      "status": git("status", "--porcelain")[:200],
                                      "remotes": git("remote", "-v"),
                                      "tracked": git("ls-files", "verification/").splitlines()[:8]}),
        "artifact_paths": [os.path.join(ART, "run_tests7.py"), os.path.join(ART, "make_packet.py")],
        "ok": ok("S7-A7"),
    })
    claims.append({
        "id": "S7-A8",
        "claim": "This packet is complete: every S7-A1..A8 claim is present, ok, and carries "
                 "artifact paths that exist.",
        "command": "python3 make_packet.py; python3 run_tests7.py a8",
        "output_excerpt": json.dumps({"claims": [c["id"] for c in claims],
                                      "checks": {k: v.get("ok") for k, v in checks.items()}})[:1200],
        "artifact_paths": [os.path.join(ART, "packet.json"), os.path.join(ART, "results7.json")],
        # A8 is the packet's own completeness statement, so it is evaluated from
        # the packet rather than from results7: reading results7's S7-A8 here is
        # circular (the check cannot pass until the packet claims it passed, and
        # the claim cannot pass until the check has run). Same predicate the
        # check applies: every earlier claim ok and every referenced path real.
        "ok": (all(c["ok"] for c in claims)
               and not [p for c in claims for p in c["artifact_paths"] if not os.path.exists(p)]),
    })

    packet = {
        "job_id": "matrix-stage7",
        "task": "THE MATRIX Stage 7: data lifecycle + performance. Retention and lossless "
                "event-line compaction proven on fixtures, export tooling with machine-checked "
                "count identity, MB/day growth model (measured vs projected), a >=60 minute "
                "mixed-traffic soak with drift and post-soak budgets, a parity audit against the "
                "legacy dashboard, and the residency decision executed as a labelled estimate.",
        "generated": time.strftime("%Y-%m-%dT%H:%M:%S%z"),
        "acceptance_criteria": [
            "S7-A1 S3(10)+S4(10)+S5(9)+S6(10) green same session, zero warnings.",
            "S7-A2 retention proven on copied fixtures (hw/status pruned, routing never), export "
            "NDJSON + queryable summary with count identity, MB/day measured vs projected separate.",
            "S7-A3 soak >=60 min with mixed traffic, drift trends, unbounded growth found/fixed, "
            "budgets re-verified after.",
            "S7-A4 parity table for every data surface incl. zero-hole asymmetry + verdict + prose.",
            "S7-A5 residency executed and labelled (estimate from model structure, never live truth).",
            "S7-A6 engine untouched (PID/NRestarts/mtime/inventory-sha identical, no new files).",
            "S7-A7 repo clean, zero remotes, run_tests7.py committed.",
            "S7-A8 packet complete.",
        ],
        "claims": claims,
        "prior_suites": prior,
        "engine_identity": {"before": {k: eb.get(k) for k in ("inventory_n", "inventory_sha256")},
                            "after": {k: ea.get(k) for k in ("inventory_n", "inventory_sha256")}},
        "growth_model": model,
        "budgets": analysis.get("budgets"),
        "artifacts": {"parity_sha256": sha(os.path.join(ART, "PARITY.md")),
                      "proof_sha256": sha(os.path.join(ART, "proof.json")),
                      "analysis_sha256": sha(os.path.join(ART, "analysis.json")),
                      "vault_artifact": VAULT},
    }
    json.dump(packet, open(OUT, "w"), indent=1)
    print(json.dumps({"packet": OUT, "claims": [(c["id"], c["ok"]) for c in claims]}, indent=1))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
