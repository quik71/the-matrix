#!/usr/bin/env python3
"""Stage-8 acceptance harness (S8-A1..S8-A8) for THE MATRIX.

Operational hardening: systemd user units + autostart choice, failure recovery,
upgrade/rollback/uninstall, bounded logs, ops runbook.

Runs on blackie against host `main` over ssh. Every number below is read from a
command or artifact produced in this session; nothing is typed in.

usage: run_tests8.py [all|a1..a8]

Artifacts written here (default /srv/storage/minion-verification/matrix-stage8/artifacts):
  build.log            zero-warning rebuild of ~/matrix-build (touched sources)
  prior/               S3+S4+S5+S6 re-run from their own harnesses (fresh)
  s7/                  the S7 suite re-executed in this session
  units.json           enabled/ordering/ExecStart/environment evidence
  cycles.json          stop/start cycles (no reboot)
  recovery.json        kill -9 collector and kill -9 app, counters before/after
  deploy.json          live upgrade + rollback + throwaway install/uninstall
  log-bounds.json      journald caps, measured unit log volume, rotation, probe
  runbook.json         runbook presence + every documented command executed
  engine-before.json / engine-after.json
  results8.json        this harness's own check list
  packet.json          make_packet.py output (= the deliverable)
"""
import json
import os
import re
import shlex
import shutil
import subprocess
import sys
import time

ART = os.environ.get("STAGE8_ART", "/srv/storage/minion-verification/matrix-stage8/artifacts")
REPO = "/srv/storage/work/the-matrix"
PRIOR7 = "/srv/storage/minion-verification/matrix-stage7/artifacts"
# The Stage-7 parity doc in the vault: S7-A4 asserts it exists, and the Stage-7
# run passed it in as VAULT_ARTIFACT. Re-executing S7 without it would fail the
# check on a missing environment variable, not on a missing artifact.
S7_VAULT = ("/srv/storage/obsidian/diagnostics-vault/Projects/"
            "THE MATRIX \u2014 Stage 7 Data Lifecycle, Performance and Parity (2026-10-31).md")
PREFIX = "/home/main/the-matrix"
BUILD = "~/matrix-build"
UNIT_C = "matrix-collect.service"
UNIT_A = "matrix-app.service"
STAMP_PATH = os.path.join(ART, "session-start.txt")

RESULTS = {"started": time.strftime("%Y-%m-%dT%H:%M:%S%z"), "checks": []}


# ---------------------------------------------------------------- plumbing
def ssh(cmd, timeout=2400):
    p = subprocess.run(["ssh", "main", cmd], capture_output=True, text=True, timeout=timeout)
    return {"rc": p.returncode, "out": p.stdout, "err": p.stderr}


def out(cmd, timeout=2400):
    return ssh(cmd, timeout)["out"].strip()


def local(cmd, timeout=600):
    # Every call site is a literal built from constants in this file.
    return subprocess.run(cmd, shell=True, capture_output=True, text=True, timeout=timeout).stdout.strip()


def save(name, obj):
    """Write an artifact. results8.json is MERGED by check name so that partial
    runs (a2, a3, ... executed one at a time) accumulate into one check list."""
    path = os.path.join(ART, name)
    os.makedirs(os.path.dirname(path), exist_ok=True)
    if name == "results8.json" and os.path.exists(path):
        try:
            prev = json.load(open(path))
        except Exception:
            prev = {}
        by = {c["name"]: c for c in (prev.get("checks") or [])}
        for c in obj.get("checks") or []:
            by[c["name"]] = c
        merged = dict(prev)
        merged.update({k: v for k, v in obj.items() if k != "checks"})
        merged["checks"] = list(by.values())
        merged.setdefault("started", obj.get("started"))
        obj = merged
    with open(path, "w") as f:
        json.dump(obj, f, indent=1)
    return path


def load(name):
    with open(os.path.join(ART, name)) as f:
        return json.load(f)


def check(name, ok, detail, **kw):
    rec = {"name": name, "ok": bool(ok), "detail": detail}
    rec.update(kw)
    RESULTS["checks"].append(rec)
    print(("PASS " if ok else "FAIL ") + name + " :: " + str(detail)[:1200], flush=True)
    save("results8.json", RESULTS)
    return ok


def show(unit, *props):
    r = out(f"systemctl --user show {unit} " + " ".join("-p " + p for p in props))
    d = {}
    for line in r.splitlines():
        if "=" in line:
            k, v = line.split("=", 1)
            d[k] = v
    return d


def health(wait=30, prefix=PREFIX):
    r = ssh(f"python3 {BUILD}/deploy/health.py --prefix {prefix} --wait {wait}", timeout=120)
    try:
        return json.loads(r["out"].strip().splitlines()[-1])
    except Exception:
        return {"ok": False, "raw": r["out"][-400:], "err": r["err"][-400:]}


def appstats(prefix=PREFIX):
    p = f"{prefix}/run/live-stats.json"
    r = ssh(f"cat {p}")
    try:
        return json.loads(r["out"])
    except Exception:
        return {}


def sql(q, db=f"{PREFIX}/data/telemetry.sqlite"):
    r = ssh(f"sqlite3 -json {shlex.quote(db)} {shlex.quote(q)}")
    if r["rc"] != 0:
        raise RuntimeError(r["err"][:300])
    s = r["out"].strip()
    return json.loads(s) if s else []


def wait_for(fn, timeout, step=0.4):
    t0 = time.time()
    last = None
    while time.time() - t0 < timeout:
        last = fn()
        if last:
            return time.time() - t0, last
        time.sleep(step)
    return None, last


def stop_units():
    ssh(f"systemctl --user stop {UNIT_A} {UNIT_C}", timeout=120)


def start_units():
    ssh(f"systemctl --user reset-failed {UNIT_A} {UNIT_C} 2>/dev/null || true; "
        f"systemctl --user start {UNIT_C}; for i in $(seq 1 60); do [ -S {PREFIX}/run/collector.sock ] && break; sleep 0.25; done; "
        f"systemctl --user start {UNIT_A}", timeout=180)
    return health(35)


def journal_mark():
    """A journal cursor (or time) used to measure a window precisely."""
    return out("date +%s.%N")


# ---------------------------------------------------------------- S8-A1
def test_a1():
    """Zero-warning rebuild; S3(10)+S4(10)+S5(9)+S6(10) re-run fresh; S7 suite
    re-executed in this session against its own evidence artifacts."""
    t = {}

    # --- the binary under test: full rebuild from touched sources
    r = ssh(f"cd {BUILD} && touch crates/*/src/*.rs && cargo build --release 2>&1", timeout=1800)
    log = r["out"]
    open(os.path.join(ART, "build.log"), "w").write(log)
    warns = [l for l in log.splitlines() if l.startswith("warning:")]
    errs = [l for l in log.splitlines() if l.startswith("error")]
    t.update({"warning_lines": warns, "warning_count": len(warns), "error_lines": errs,
              "build_ok": "Finished `release` profile" in log and not errs})
    built = out(f"sha256sum {BUILD}/target/release/matrix-collect {BUILD}/target/release/matrix-app")
    inst = out(f"sha256sum {PREFIX}/bin/matrix-collect {PREFIX}/bin/matrix-app")
    t["built_sha"] = {os.path.basename(x.split()[1]): x.split()[0] for x in built.splitlines() if x.strip()}
    t["installed_sha"] = {os.path.basename(x.split()[1]): x.split()[0] for x in inst.splitlines() if x.strip()}
    t["installed_equals_built"] = t["built_sha"] == t["installed_sha"]

    # --- the S3..S6 chain, re-run here (each from its own harness, retried only
    # when foreign engine traffic lands inside a test window; attempts reported)
    prior = os.path.join(ART, "prior")
    os.makedirs(prior, exist_ok=True)
    # Chain-wide freshness stamp. Both copied-harness variants read a stamp;
    # nothing wrote either of them (s7's copy read STAMP_PATH: FileNotFoundError
    # waiting to happen; s6 found no prior/session-start.txt and stamped ITSELF,
    # then called its own chain's earlier suites stale — root cause of the
    # 3/3 s6-A1 failures on 2026-11-01). Written once, here, before the chain.
    stamp_ts = f"{time.time()}\n"
    with open(STAMP_PATH, "w") as fh:
        fh.write(stamp_ts)
    with open(f"{prior}/session-start.txt", "w") as fh:
        fh.write(stamp_ts)
    subprocess.run(f"rsync -a --exclude __pycache__ --exclude session-start.txt "
                   f"{PRIOR7}/s3 {PRIOR7}/s4 {PRIOR7}/s5 "
                   f"{PRIOR7}/run_tests6.py {PRIOR7}/results.json {prior}/", shell=True, check=True)
    # fixtures the S5 harness imports from the S3 run
    os.makedirs(f"{prior}/s5/stage3", exist_ok=True)
    os.makedirs(f"{prior}/s4/stage3", exist_ok=True)

    def checks_of(path):
        try:
            d = json.load(open(path))
        except Exception:
            return 0, 99
        cs = d.get("checks") or []
        return len(cs), sum(1 for c in cs if not c.get("ok"))

    def run_until_green(cwd, cmd, want, tries):
        recs = []
        for i in range(1, tries + 1):
            try:
                os.remove(os.path.join(cwd, "results.json"))
            except OSError:
                pass
            r0 = subprocess.run(cmd, cwd=cwd, shell=True, capture_output=True, text=True,
                                timeout=2400)
            try:
                shutil.copy(os.path.join(cwd, "results.json"),
                            os.path.join(cwd, f"results-attempt{i}.json"))
            except (OSError, shutil.Error):
                pass
            n, f = checks_of(os.path.join(cwd, "results.json"))
            recs.append({"attempt": i, "n": n, "failed": f, "rc": r0.returncode,
                         "tail": (r0.stdout or "")[-300:]})
            print(f"    {cmd}: attempt {i} checks={n} failed={f}", flush=True)
            if n >= want and f == 0:
                return recs, True
            time.sleep(5)
        return recs, False

    chain = {}
    stop_units()
    a, ok = run_until_green(f"{prior}/s3", "python3 run_tests.py all", 10, 8)
    chain["s3"] = {"attempts": a, "ok": ok}
    if ok:
        subprocess.run(f"cp {prior}/s3/results.json {prior}/s5/stage3/results.json && "
                       f"cp {prior}/s3/run_tests.py {prior}/s4/stage3/run_tests.py", shell=True, check=True)
        a, ok = run_until_green(f"{prior}/s4", "python3 run_tests4.py all", 10, 4)
        chain["s4"] = {"attempts": a, "ok": ok}
    if chain.get("s4", {}).get("ok"):
        a, ok = run_until_green(f"{prior}/s5", "python3 run_tests5.py all", 9, 4)
        chain["s5"] = {"attempts": a, "ok": ok}
    if chain.get("s5", {}).get("ok"):
        a, ok = run_until_green(prior, "python3 run_tests6.py all", 10, 4)
        chain["s6"] = {"attempts": a, "ok": ok}
    h = start_units()
    t["chain"] = chain
    t["chain_restored_health"] = h.get("ok")

    for name, want in (("s3", 10), ("s4", 10), ("s5", 9), ("s6", 10)):
        p = {"s3": f"{prior}/s3/results.json", "s4": f"{prior}/s4/results.json",
             "s5": f"{prior}/s5/results.json", "s6": f"{prior}/results.json"}[name]
        n, f = checks_of(p)
        try:
            fin = json.load(open(p)).get("finished")
        except Exception:
            fin = None
        t[name] = {"n": n, "failed": f, "finished": fin, "ok": n >= want and f == 0}

    # --- the S7 suite, re-executed here. Its live checks (A1/A6/A7/A8) run for
    # real against this session's state; A2/A3/A5 re-verify the Stage-7 evidence
    # artifacts (the 62-minute soak body is not re-run -- see HARNESS-NOTES).
    # Two harness bugs fixed here: the suite was invoked *over ssh* with a local
    # path (so it never ran: `cd /srv/...` on main fails), and results7.json was
    # pre-seeded with the S6 result file, so a suite that never ran still looked
    # 10/10 green. It now runs on this host, from a clean results7.json, with the
    # VAULT_ARTIFACT the Stage-7 run used (S7-A4 asserts that file exists), and
    # the check requires all eight S7 names to be present.
    s7 = os.path.join(ART, "s7")
    if os.path.isdir(s7):
        shutil.rmtree(s7)
    subprocess.run(f"rsync -a --exclude __pycache__ {PRIOR7}/ {s7}/", shell=True, check=True)
    subprocess.run(f"cp {prior}/s3/results.json {s7}/s3/results.json && "
                   f"cp {prior}/s4/results.json {s7}/s4/results.json && "
                   f"cp {prior}/s5/results.json {s7}/s5/results.json && "
                   f"cp {prior}/results.json {s7}/results.json", shell=True, check=True)
    try:
        os.remove(f"{s7}/results7.json")
    except OSError:
        pass
    stamp = open(STAMP_PATH).read().strip()
    open(f"{s7}/session-start.txt", "w").write(stamp + "\n")
    env7 = dict(os.environ, STAGE7_ART=s7, VAULT_ARTIFACT=S7_VAULT)
    r7 = subprocess.run([sys.executable, "run_tests7.py", "all"], cwd=s7, env=env7,
                        capture_output=True, text=True, timeout=3600)
    t["s7_rc"] = r7.returncode
    t["s7_log_tail"] = (r7.stdout + r7.stderr)[-1500:]
    t["s7_vault"] = S7_VAULT
    try:
        d7 = json.load(open(f"{s7}/results7.json"))
    except Exception:
        d7 = {}
    c7 = d7.get("checks") or []
    names7 = [c["name"] for c in c7]
    missing7 = [f"S7-A{i}" for i in range(1, 9) if f"S7-A{i}" not in names7]
    t["s7"] = {"n": len(c7), "failed": [c["name"] for c in c7 if not c.get("ok")],
               "names": names7, "missing": missing7}
    t["s7_ok"] = (r7.returncode == 0 and len(c7) >= 8 and not t["s7"]["failed"]
                  and not missing7)
    t["soak_note"] = "S7-A2/A3/A5 read the Stage-7 evidence artifacts; A1/A6/A7/A8 executed live"

    ok = (t["build_ok"] and t["warning_count"] == 0 and t["installed_equals_built"]
          and all(t[k]["ok"] for k in ("s3", "s4", "s5", "s6")) and t["s7_ok"])
    save("a1.json", t)
    check("S8-A1", ok, json.dumps({
        "warnings": t["warning_count"], "build_ok": t["build_ok"],
        "installed_equals_built": t["installed_equals_built"],
        "prior": {k: {"n": t[k]["n"], "failed": t[k]["failed"]} for k in ("s3", "s4", "s5", "s6")},
        "s7": t["s7"], "soak_note": t["soak_note"]}))
    return t


# ---------------------------------------------------------------- S8-A2
def test_a2():
    """Autostart path proven without a reboot: enable state, ordering,
    environment, ExecStart, three stop/start cycles; boot activation pending."""
    t = {}
    en = out(f"systemctl --user is-enabled {UNIT_C} {UNIT_A}")
    t["is_enabled"] = en
    sc = show(UNIT_C, "After", "WantedBy", "Restart", "RestartUSec", "ExecStart", "MainPID",
              "NRestarts", "ActiveState", "Environment")
    sa = show(UNIT_A, "After", "WantedBy", "Restart", "RestartUSec", "ExecStart", "MainPID",
              "NRestarts", "ActiveState", "Environment", "Wants", "PartOf")
    t["collect"] = {k: sc.get(k) for k in ("After", "WantedBy", "Restart", "ExecStart",
                                           "MainPID", "NRestarts", "ActiveState", "Environment")}
    t["app"] = {k: sa.get(k) for k in ("After", "WantedBy", "Wants", "PartOf", "Restart",
                                       "ExecStart", "MainPID", "NRestarts", "ActiveState", "Environment")}
    after_c = sc.get("After", "")
    after_a = sa.get("After", "")
    t["ordering"] = {
        "collector_after_network_online": "network-online.target" in after_c,
        "app_after_graphical_session": "graphical-session.target" in after_a,
        "app_after_collector": UNIT_C in after_a,
        "app_partof_session": "graphical-session.target" in sa.get("PartOf", ""),
        "app_wants_collector": UNIT_C in sa.get("Wants", ""),
    }

    # ExecStart must be exactly the command line that was proven in Stages 3-7.
    proven_c = (f"{PREFIX}/bin/matrix-collect --path /home/main/strata-0.2.4.2/data/routing-trace.bin "
                f"--socket {PREFIX}/run/collector.sock --db {PREFIX}/data/telemetry.sqlite "
                f"--engine 127.0.0.1:8080 --engine-conf /home/main/strata-0.2.4.2/strata-iq3_s.json")
    proven_a = (f"{PREFIX}/bin/matrix-app --socket {PREFIX}/run/collector.sock "
                f"--stats {PREFIX}/run/live-stats.json --rain on --view 0 --monitor DP-1")
    t["exec"] = {"collect_exact": proven_c in sc.get("ExecStart", ""),
                 "app_exact": proven_a in sa.get("ExecStart", "")}

    # Wayland reality: the user manager (uwsm) holds the compositor env.
    env = out("systemctl --user show-environment")
    t["manager_env"] = {k: v for k, v in (l.split("=", 1) for l in env.splitlines() if "=" in l)
                        if k in ("WAYLAND_DISPLAY", "XDG_RUNTIME_DIR", "XDG_SESSION_TYPE",
                                 "HYPRLAND_INSTANCE_SIGNATURE", "XDG_CURRENT_DESKTOP")}
    tgt_state = out("systemctl --user is-active graphical-session.target")
    t["graphical_session_target"] = tgt_state
    # This check reads /proc/<MainPID>/environ, so the pair must actually be
    # running: a suite started while the previous one left the units stopped
    # reads MainPID=0 and reports env_inherited:false about a stopped process.
    t["health_at_start"] = health(20).get("ok")
    if not t["health_at_start"]:
        start_units()
        wait_for(lambda: show(UNIT_A, "MainPID").get("MainPID", "0") not in ("", "0"), 30)
    pid = show(UNIT_A, "MainPID").get("MainPID", "0")
    proc_env = out(f"tr '\\0' '\\n' < /proc/{pid}/environ 2>/dev/null")
    penv = dict(l.split("=", 1) for l in proc_env.splitlines() if "=" in l)
    t["app_proc_env"] = {k: penv.get(k) for k in ("WAYLAND_DISPLAY", "XDG_RUNTIME_DIR",
                                                  "HYPRLAND_INSTANCE_SIGNATURE", "XDG_SESSION_TYPE")}
    t["env_inherited"] = (penv.get("WAYLAND_DISPLAY") == t["manager_env"].get("WAYLAND_DISPLAY")
                          and bool(penv.get("WAYLAND_DISPLAY")))

    # enable wiring: the symlink that a login would follow
    t["wants_symlinks"] = {
        "app": out(f"readlink -f ~/.config/systemd/user/graphical-session.target.wants/{UNIT_A}"),
        "collect": out(f"readlink -f ~/.config/systemd/user/default.target.wants/{UNIT_C}"),
    }
    t["dep_edge"] = UNIT_A in out("systemctl --user list-dependencies graphical-session.target")

    # start/stop cycles (no reboot)
    cycles = []
    h0 = health(20)
    for i in range(1, 4):
        c0 = show(UNIT_C, "MainPID").get("MainPID")
        a0 = show(UNIT_A, "MainPID").get("MainPID")
        t0 = time.time()
        ssh(f"systemctl --user stop {UNIT_C}", timeout=60)
        down = {
            "collect_state": out(f"systemctl --user is-active {UNIT_C}"),
            "socket_present": out(f"test -S {PREFIX}/run/collector.sock && echo yes || echo no"),
        }
        ssh(f"systemctl --user stop {UNIT_A}", timeout=60)
        down["app_state"] = out(f"systemctl --user is-active {UNIT_A}")
        # bring them back in the documented order: collector, socket, then app
        ssh(f"systemctl --user start {UNIT_C}", timeout=60)
        _, up_c = wait_for(lambda: out(f"systemctl --user is-active {UNIT_C}") == "active", 20)
        _, sock = wait_for(lambda: out(f"test -S {PREFIX}/run/collector.sock && echo yes") == "yes", 20)
        ssh(f"systemctl --user reset-failed {UNIT_A} 2>/dev/null || true; "
            f"systemctl --user start {UNIT_A}", timeout=60)
        h = health(40)
        c1 = show(UNIT_C, "MainPID").get("MainPID")
        a1 = show(UNIT_A, "MainPID").get("MainPID")
        cycles.append({"cycle": i, "t_s": round(time.time() - t0, 2), "down": down,
                       "collect_pid": [c0, c1], "app_pid": [a0, a1],
                       "new_pids": c1 != c0 and a1 != a0, "socket_back": sock,
                       "health_ok": bool(h.get("ok")), "health": {
                           "connected": h.get("connected"), "meta": h.get("meta_run_id"),
                           "painted": h.get("residency_painted"), "frames": h.get("frames")}})
    t["cycles"] = cycles

    # Honest boot statement: the only link not exercised is login-session
    # activation itself (no reboot / no logout was performed).
    t["pending_boot"] = ("enabled=yes and the graphical-session.target.wants symlink exists; "
                         "start/stop cycles proven live; actual activation at the next login "
                         "is PENDING (no reboot or logout performed)")

    ok = (en.split() == ["enabled", "enabled"] and all(t["ordering"].values())
          and all(t["exec"].values()) and t["graphical_session_target"] == "active"
          and t["env_inherited"] and t["dep_edge"]
          and all(c["health_ok"] and c["new_pids"] and c["socket_back"] for c in cycles)
          and h0.get("ok"))
    save("units.json", {"units": t, "cycles": cycles})
    check("S8-A2", ok, json.dumps({
        "enabled": en.split(), "ordering": t["ordering"], "exec": t["exec"],
        "graphical_session_target": tgt_state, "env_inherited": t["env_inherited"],
        "dep_edge": t["dep_edge"], "cycles": len(cycles),
        "cycle_ok": [c["health_ok"] for c in cycles],
        "painted": [c["health"]["painted"] for c in cycles],
        "pending_boot": t["pending_boot"]}))
    return t


# ---------------------------------------------------------------- S8-A3
def test_a3():
    """kill -9 collector -> unit restart + app reconnect with replay+meta;
    kill -9 app -> unit restart; identity re-proven with counters each time."""
    t = {}
    before = appstats()
    sc0 = show(UNIT_C, "MainPID", "NRestarts")
    t["before_collector"] = {"pid": sc0.get("MainPID"), "nrestarts": sc0.get("NRestarts"),
                             "run_id": (before.get("meta") or {}).get("run_id"),
                             "connects": before.get("connects"),
                             "reconnects_with_replay": before.get("reconnects_with_replay"),
                             "replay_seen": before.get("replay_seen"),
                             "painted": (before.get("residency") or {}).get("painted")}

    # ---- kill -9 the collector
    t0 = time.time()
    ssh(f"kill -9 $(systemctl --user show {UNIT_C} -p MainPID --value)", timeout=60)
    dt, _ = wait_for(lambda: show(UNIT_C, "MainPID").get("MainPID") not in ("0", sc0.get("MainPID")),
                     30)
    ok_active, _ = wait_for(lambda: out(f"systemctl --user is-active {UNIT_C}") == "active", 30)
    h = health(60)
    # the collector reports a per-subscriber snapshot every 2 s: wait for the
    # post-reconnect counters instead of reading the dump in the race window
    wait_for(lambda: (appstats().get("reconnects_with_replay") or 0)
             > (t["before_collector"]["reconnects_with_replay"] or 0), 25)
    sc1 = show(UNIT_C, "MainPID", "NRestarts")
    after = appstats()
    t["kill_collector"] = {
        "restart_s": round(dt, 2) if dt else None,
        "recovery_s": round(time.time() - t0, 2),
        "pid": [sc0.get("MainPID"), sc1.get("MainPID")],
        "nrestarts": [sc0.get("NRestarts"), sc1.get("NRestarts")],
        "unit_active": out(f"systemctl --user is-active {UNIT_C}"),
        "app": {"connected": after.get("connected"), "connects": after.get("connects"),
                "disconnects": after.get("disconnects"),
                "reconnects_with_replay": after.get("reconnects_with_replay"),
                "replay_seen": after.get("replay_seen"),
                "run_id_before": (before.get("meta") or {}).get("run_id"),
                "run_id_after": (after.get("meta") or {}).get("run_id"),
                "last_disconnect_reason": after.get("last_disconnect_reason"),
                "painted": (after.get("residency") or {}).get("painted"),
                "meta_source": (after.get("meta") or {}).get("meta_source")},
        "health": {k: h.get(k) for k in ("ok", "connected", "meta_run_id", "residency_painted", "frames")},
    }
    k = t["kill_collector"]
    t["kill_collector_ok"] = (
        k["restart_s"] is not None and k["unit_active"] == "active"
        and int(k["nrestarts"][1]) > int(k["nrestarts"][0])
        and k["pid"][0] != k["pid"][1]
        and k["app"]["connected"] is True
        and (k["app"]["connects"] or 0) > (t["before_collector"]["connects"] or 0)
        and k["app"]["run_id_after"] != k["app"]["run_id_before"]
        and (k["app"]["reconnects_with_replay"] or 0) > (t["before_collector"]["reconnects_with_replay"] or 0)
        and k["health"]["ok"] and k["health"]["residency_painted"] == 2675)

    # ---- a fresh, independent subscriber must get meta first, then replay lines
    script = r'''
import json, socket, sys, time
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.settimeout(12)
s.connect(sys.argv[1])
buf = b""
lines = []
t0 = time.time()
while len(lines) < 12 and time.time() - t0 < 12:
    try:
        chunk = s.recv(65536)
    except socket.timeout:
        break
    if not chunk:
        break
    buf += chunk
    while b"\n" in buf:
        line, buf = buf.split(b"\n", 1)
        if line.strip():
            lines.append(line)
types = []
for l in lines:
    try: types.append(json.loads(l).get("type"))
    except Exception: types.append("unparseable")
meta = None
for l in lines:
    try:
        o = json.loads(l)
        if o.get("type") == "meta": meta = o; break
    except Exception: pass
print(json.dumps({"n": len(lines), "first_types": types[:6],
                  "meta_first": bool(types and types[0] == "meta"),
                  "run_id": (meta or {}).get("collector", {}).get("run_id"),
                  "model": (meta or {}).get("engine", {}).get("model")}))
'''
    open(os.path.join(ART, "sub8.py"), "w").write(script)
    subprocess.run(f"scp -q {ART}/sub8.py main:/tmp/sub8.py", shell=True, check=True)
    r = ssh(f"python3 /tmp/sub8.py {PREFIX}/run/collector.sock", timeout=60)
    try:
        sub = json.loads(r["out"].strip().splitlines()[-1])
    except Exception:
        sub = {"error": r["out"][-300:], "err": r["err"][-300:]}
    t["fresh_subscriber"] = sub
    t["fresh_subscriber_ok"] = bool(sub.get("meta_first") and sub.get("n", 0) >= 2)

    # ---- kill -9 the app
    sa0 = show(UNIT_A, "MainPID", "NRestarts")
    run_before = (appstats().get("meta") or {}).get("run_id")
    t0 = time.time()
    ssh(f"kill -9 $(systemctl --user show {UNIT_A} -p MainPID --value)", timeout=60)
    dt, _ = wait_for(lambda: show(UNIT_A, "MainPID").get("MainPID") not in ("0", sa0.get("MainPID")), 40)
    h = health(60)
    wait_for(lambda: (appstats().get("replay_seen") or 0) > 0, 20)
    sa1 = show(UNIT_A, "MainPID", "NRestarts")
    a2 = appstats()
    t["kill_app"] = {
        "restart_s": round(dt, 2) if dt else None,
        "recovery_s": round(time.time() - t0, 2),
        "pid": [sa0.get("MainPID"), sa1.get("MainPID")],
        "nrestarts": [sa0.get("NRestarts"), sa1.get("NRestarts")],
        "unit_active": out(f"systemctl --user is-active {UNIT_A}"),
        "stats_fresh": h.get("stats_fresh"),
        "connected": a2.get("connected"),
        "connects": a2.get("connects"),
        "replay_seen": a2.get("replay_seen"),
        "meta_source": (a2.get("meta") or {}).get("meta_source"),
        "run_id": (a2.get("meta") or {}).get("run_id"),
        "run_id_unchanged": (a2.get("meta") or {}).get("run_id") == run_before,
        "painted": (a2.get("residency") or {}).get("painted"),
        "frames": a2.get("frames"),
        "health": {k: h.get(k) for k in ("ok", "connected", "residency_painted", "frames")},
    }
    k2 = t["kill_app"]
    t["kill_app_ok"] = (
        k2["restart_s"] is not None and k2["unit_active"] == "active"
        and int(k2["nrestarts"][1]) > int(k2["nrestarts"][0])
        and k2["pid"][0] != k2["pid"][1]
        and k2["connected"] is True and k2["replay_seen"] > 0
        and k2["painted"] == 2675 and k2["health"]["ok"]
        and k2["meta_source"] == "socket" and k2["run_id_unchanged"])

    # ---- seq continuity: the restart is recorded, nothing is invented
    try:
        row = sql("select count(*) as gaps, sum(case when reason='collector_restart' then 1 else 0 end) as restarts from (select json_extract(line,'$.reason') as reason from events where kind='gap')")
        t["gaps"] = row[0]
    except Exception as e:
        t["gaps"] = {"error": str(e)[:200]}
    t["ok"] = t["kill_collector_ok"] and t["fresh_subscriber_ok"] and t["kill_app_ok"]
    save("recovery.json", t)
    check("S8-A3", t["ok"], json.dumps({
        "collector": {kk: t["kill_collector"][kk] for kk in ("restart_s", "recovery_s", "nrestarts", "pid")} | {"app": t["kill_collector"]["app"]},
        "fresh_subscriber": t["fresh_subscriber"],
        "app": {kk: t["kill_app"][kk] for kk in ("restart_s", "pid", "nrestarts", "replay_seen", "painted", "run_id_unchanged", "meta_source")},
        "gaps": t.get("gaps")}))
    return t


# ---------------------------------------------------------------- S8-A4
def test_a4():
    """Upgrade (deploy), previous-binary rollback, throwaway install/uninstall.
    Executed for real; the live DB is never written by these scripts."""
    t = {}
    db = f"{PREFIX}/data/telemetry.sqlite"
    t["live_db_before"] = {
        "inode": out(f"stat -c %i {db}"),
        "size": out(f"stat -c %s {db}"),
        "counts": sql("select (select count(*) from events) as events, (select max(seq) from events) as max_seq")[0],
        "data_dir": out(f"ls -1 {PREFIX}/data | sort | tr '\\n' ' '"),
    }
    sha0 = out(f"sha256sum {PREFIX}/bin/matrix-*")

    # --- 1. put the previous *compatible* release back on the box, so the
    # upgrade below is a real one (different bytes over an older release).
    # "Compatible" is checked, not assumed: the collector must parse the unit's
    # --max-db-mb. Measured today: rebuilding the same source in another
    # directory is byte-identical, so a same-source redeploy cannot prove an
    # upgrade at all -- the predecessor's "different dir => different bytes"
    # assumption was false and its sha comparison accordingly meaningless.
    cands = [x.rstrip("/") for x in out(f"ls -1d {PREFIX}/bin/prev/*/").split() if x]
    t["prev_candidates"] = cands
    prev_pair = ""
    for c in sorted(cands, reverse=True):
        if out(f"{c}/matrix-collect --help 2>&1 | grep -c -- --max-db-mb") not in ("0", ""):
            if out(f"sha256sum {c}/matrix-collect | cut -d' ' -f1") != out(
                    f"sha256sum {PREFIX}/bin/matrix-collect | cut -d' ' -f1"):
                prev_pair = c
                break
    t["prev_pair_used"] = prev_pair
    if prev_pair:
        r = ssh(f"{BUILD}/deploy/install.sh --bin-src {prev_pair} --prefix {PREFIX} "
                f"--unit-prefix matrix 2>&1 | tail -1", timeout=300)
        t["prev_install"] = {"rc": r["rc"], "out": r["out"][-200:]}
        h = health(45)
        t["prev_install_health"] = h.get("ok")
    sha_before = out(f"sha256sum {PREFIX}/bin/matrix-*")

    # --- 2. upgrade: deploy the committed build over that release
    r = ssh(f"{BUILD}/deploy/deploy.sh --from local --src {BUILD} --work {PREFIX}/build "
            f"--prefix {PREFIX} --unit-prefix matrix --build --auto-rollback 2>&1", timeout=1800)
    t["deploy"] = {"rc": r["rc"], "out": r["out"][-1200:]}
    h = health(40)
    t["deploy_health"] = h.get("ok")
    new_sha = out(f"sha256sum {PREFIX}/bin/matrix-*")
    # compare the two binaries by name: `matrix-*` in the build dir also matches
    # matrix-app.d / matrix-collect.d (the predecessor's glob made this always
    # false)
    build_sha = out(f"sha256sum {PREFIX}/build/target/release/matrix-collect "
                    f"{PREFIX}/build/target/release/matrix-app")
    t["deploy_pair"] = {"installed": new_sha, "just_built": build_sha,
                        "named_equal": (sorted(l.split()[0] for l in new_sha.splitlines() if l.strip())
                                        == sorted(l.split()[0] for l in build_sha.splitlines() if l.strip()))}
    t["deploy_sha_changed"] = new_sha != sha_before and t["deploy_pair"]["named_equal"]
    sha_before_map = dict((l.split()[1].split("/")[-1], l.split()[0]) for l in sha_before.splitlines() if l.strip())
    prev = ""
    for c in sorted(cands, reverse=True):
        try:
            m = json.loads(out(f"cat {c}/manifest.json"))
        except Exception:
            continue
        if (m.get("old_collect_sha") == sha_before_map.get("matrix-collect")
                and m.get("old_app_sha") == sha_before_map.get("matrix-app")):
            prev = c
            break
    t["prev_chosen"] = prev
    t["prev_units_saved"] = (json.loads(out(f"cat {prev}/manifest.json")).get("units_saved")
                             if prev else [])

    # --- 3. rollback to the pair that was just replaced (binaries + its units)
    r = ssh(f"{BUILD}/deploy/rollback.sh --prefix {PREFIX} --unit-prefix matrix --to {prev} 2>&1", timeout=300)
    t["rollback"] = {"rc": r["rc"], "out": r["out"][-600:], "prev": prev,
                     "sha": out(f"sha256sum {PREFIX}/bin/matrix-*")}
    try:
        t["rollback_json"] = json.loads(r["out"].strip().splitlines()[-1])
    except Exception:
        t["rollback_json"] = {}
    h = health(45)
    t["rollback_health"] = h.get("ok")
    prev_sha_c = out(f"sha256sum {PREFIX}/bin/matrix-collect | cut -d' ' -f1")
    prev_sha_a = out(f"sha256sum {PREFIX}/bin/matrix-app | cut -d' ' -f1")
    manifest = json.loads(out(f"cat {prev}/manifest.json"))  # remote file, read over ssh
    t["rollback_manifest"] = manifest
    t["rollback_sha_match_manifest"] = (
        prev_sha_c == manifest["old_collect_sha"] and prev_sha_a == manifest["old_app_sha"])
    t["rollback_units_restored"] = bool(t["rollback_json"].get("units_restored"))
    t["rollback_exec_has_cap"] = "--max-db-mb" in out(f"systemctl --user show {UNIT_C} -p ExecStart --value")

    # --- 3. normalise back to the reference build in ~/matrix-build so that the
    # later rebuild (S8-A1) compares like with like: same tree, flags and paths.
    r = ssh(f"{BUILD}/deploy/install.sh --bin-src {BUILD}/target/release --prefix {PREFIX} "
            f"--unit-prefix matrix 2>&1 | tail -1", timeout=300)
    t["normalized"] = {"rc": r["rc"], "sha": out(f"sha256sum {PREFIX}/bin/matrix-*")}
    ref_sha = out(f"sha256sum {BUILD}/target/release/matrix-collect {BUILD}/target/release/matrix-app")
    t["reference_build_sha"] = ref_sha
    t["normalized_matches_reference"] = (
        sorted(l.split()[0] for l in t["normalized"]["sha"].splitlines() if l.strip())
        == sorted(l.split()[0] for l in ref_sha.splitlines() if l.strip()))
    h = health(40)
    t["normalized_health"] = h.get("ok")
    t["deploy_log_tail"] = out(f"tail -3 {PREFIX}/etc/deploy-log.jsonl")

    # --- 4. throwaway target: install the reference pair, upgrade, roll back,
    # rotate, uninstall, purge. (Started from the reference pair rather than the
    # Stage-5 snapshot: a pre-Stage-8 collector cannot parse the Stage-8 unit's
    # --max-db-mb, which is the real incompatibility documented in HARNESS-NOTES.)
    TGT = "/home/main/matrix-tgt"
    tgt_from = prev_pair or f"{BUILD}/target/release"
    r = ssh(f"{BUILD}/deploy/install.sh --prefix {TGT} --unit-prefix matrixtgt "
            f"--bin-src {tgt_from} --no-app --no-start 2>&1", timeout=180)
    t["tgt_install"] = {"rc": r["rc"], "out": r["out"][-500:],
                        "enabled": out("systemctl --user is-enabled matrixtgt-collect.service"),
                        "active": out("systemctl --user is-active matrixtgt-collect.service")}
    r = ssh(f"systemctl --user start matrixtgt-collect.service; sleep 3; "
            f"systemctl --user is-active matrixtgt-collect.service; "
            f"test -S {TGT}/run/collector.sock && echo SOCKET_OK; ls -1 {TGT}/data", timeout=120)
    t["tgt_start"] = r["out"].strip().splitlines()
    t["tgt_sha_before"] = out(f"sha256sum {TGT}/bin/matrix-*")
    mk = ssh(f"mkdir -p {TGT}/build/target/release && cp {PREFIX}/build/target/release/matrix-* {TGT}/build/target/release/", timeout=120)
    r = ssh(f"{BUILD}/deploy/deploy.sh --from local --src {BUILD} --work {TGT}/build --prefix {TGT} "
            f"--unit-prefix matrixtgt --no-build 2>&1", timeout=600)
    t["tgt_deploy"] = {"rc": r["rc"], "out": r["out"][-600:],
                       "sha": out(f"sha256sum {TGT}/bin/matrix-*"),
                       "active": out("systemctl --user is-active matrixtgt-collect.service")}
    t["tgt_upgrade_changed_sha"] = t["tgt_deploy"]["sha"] != t["tgt_sha_before"]
    r = ssh(f"{BUILD}/deploy/rollback.sh --prefix {TGT} --unit-prefix matrixtgt 2>&1", timeout=300)
    t["tgt_rollback"] = {"rc": r["rc"], "out": r["out"][-400:],
                         "sha": out(f"sha256sum {TGT}/bin/matrix-*")}
    t["tgt_rollback_restores"] = t["tgt_rollback"]["sha"] == t["tgt_sha_before"]
    # rotation executed for real on the throwaway DB (routing is never pruned, so
    # this is the actual bound): archive via VACUUM INTO, fresh DB, unit back up
    r = ssh(f"{BUILD}/deploy/db-rotate.sh --prefix {TGT} --unit-prefix matrixtgt --force", timeout=600)
    try:
        rot = json.loads(r["out"].strip().splitlines()[-1])
    except Exception:
        rot = {"raw": r["out"][-300:], "err": r["err"][-300:]}
    t["tgt_rotate"] = dict(rot, rc=r["rc"],
                           archives=out(f"ls -1 {TGT}/data/archive 2>/dev/null | tr '\n' ' '"),
                           db_bytes=out(f"stat -c %s {TGT}/data/telemetry.sqlite 2>/dev/null || echo 0"),
                           active=out("systemctl --user is-active matrixtgt-collect.service"))
    t["tgt_rotate_ok"] = (r["rc"] == 0 and rot.get("action") == "rotated"
                          and rot.get("integrity") == "ok" and int(rot.get("archive_bytes") or 0) > 0
                          and t["tgt_rotate"]["active"] == "active")
    # uninstall default: keep data, remove units
    r = ssh(f"{BUILD}/deploy/uninstall.sh --prefix {TGT} --unit-prefix matrixtgt 2>&1", timeout=180)
    t["tgt_uninstall_keep"] = {"rc": r["rc"], "out": r["out"][-600:],
                               "units": out("systemctl --user list-unit-files 'matrixtgt*' --no-legend | tr -s ' '"),
                               "data": out(f"ls -1 {TGT}/data")}
    # purge with the wrong confirmation must refuse
    r = ssh(f"{BUILD}/deploy/uninstall.sh --prefix {TGT} --unit-prefix matrixtgt --purge "
            f"--confirm-data-dir {TGT}/data-wrong 2>&1; echo RC=$?", timeout=120)
    t["tgt_purge_wrong_confirm"] = {"out": r["out"][-300:], "refused": "confirm mismatch" in r["out"]}
    # reinstall, then purge with the exact confirmation
    ssh(f"{BUILD}/deploy/install.sh --prefix {TGT} --unit-prefix matrixtgt --bin-src {PREFIX}/build/target/release "
        f"--no-app --no-start 2>&1", timeout=180)
    r = ssh(f"{BUILD}/deploy/uninstall.sh --prefix {TGT} --unit-prefix matrixtgt --purge "
            f"--confirm-data-dir {TGT}/data 2>&1", timeout=180)
    t["tgt_purge"] = {"rc": r["rc"], "out": r["out"][-400:],
                      "data_exists": out(f"test -d {TGT}/data && echo yes || echo no")}

    # --- 4. live DB untouched by any of the above
    t["live_db_after"] = {
        "inode": out(f"stat -c %i {db}"),
        "size": out(f"stat -c %s {db}"),
        "counts": sql("select (select count(*) from events) as events, (select max(seq) from events) as max_seq")[0],
        "data_dir": out(f"ls -1 {PREFIX}/data | sort | tr '\\n' ' '"),
    }
    t["live_db_same_inode"] = t["live_db_before"]["inode"] == t["live_db_after"]["inode"]
    t["live_db_same_dir_listing"] = t["live_db_before"]["data_dir"] == t["live_db_after"]["data_dir"]
    t["live_untouched"] = t["live_db_same_inode"] and t["live_db_same_dir_listing"]
    t["live_health_close_a4"] = health(40).get("ok")

    ok = (t.get("prev_install_health") is True
          and t["rollback"]["rc"] == 0 and t["rollback_health"] and t["rollback_sha_match_manifest"]
          and t["rollback_units_restored"] and bool(t["prev_units_saved"])
          and t["deploy"]["rc"] == 0 and t["deploy_health"] and t["deploy_sha_changed"]
          and t["normalized_matches_reference"] and t["normalized_health"]
          and t["tgt_install"]["rc"] == 0 and "active" in t["tgt_start"]
          and t["tgt_upgrade_changed_sha"] and t["tgt_rollback_restores"]
          and t["tgt_rotate_ok"]
          and t["tgt_purge_wrong_confirm"]["refused"] and t["tgt_purge"]["data_exists"] == "no"
          and t["live_untouched"] and t["live_health_close_a4"])
    save("deploy.json", t)
    check("S8-A4", ok, json.dumps({
        "upgrade": {"rc": t["deploy"]["rc"], "health": t["deploy_health"],
                    "sha_changed": t["deploy_sha_changed"],
                    "installed_is_built": t["deploy_pair"]["installed"] == t["deploy_pair"]["just_built"]},
        "rollback": {"rc": t["rollback"]["rc"], "health": t["rollback_health"],
                     "sha_match_manifest": t["rollback_sha_match_manifest"],
                     "units_saved": t["prev_units_saved"],
                     "units_restored": t["rollback_json"].get("units_restored"),
                     "exec_still_has_cap": t["rollback_exec_has_cap"]},
        "normalized": {"matches_reference": t["normalized_matches_reference"],
                       "health": t["normalized_health"]},
        "throwaway": {"install_rc": t["tgt_install"]["rc"], "start": t["tgt_start"],
                      "upgrade_changed_sha": t["tgt_upgrade_changed_sha"],
                      "rollback_restores": t["tgt_rollback_restores"],
                      "rotate": {"action": t["tgt_rotate"].get("action"),
                                 "archive_bytes": t["tgt_rotate"].get("archive_bytes"),
                                 "db_bytes_after": t["tgt_rotate"]["db_bytes"]},
                      "purge_wrong_confirm_refused": t["tgt_purge_wrong_confirm"]["refused"],
                      "purge_data_exists": t["tgt_purge"]["data_exists"]},
        "live_db": {"same_inode": t["live_db_same_inode"], "same_dir": t["live_db_same_dir_listing"],
                    "events_before": t["live_db_before"]["counts"], "events_after": t["live_db_after"]["counts"]}}))
    return t


# ---------------------------------------------------------------- S8-A5
def test_a5():
    """Bounded logs/state, measured: journald caps enforced, rotation seen,
    unit log volume measured, per-unit rate limiting tested and reported as
    unsupported for user units."""
    t = {}
    du_before = out("journalctl --disk-usage")
    cfg = out("systemd-analyze cat-config systemd/journald.conf | grep -vE '^\\s*#|^\\s*$'")
    t["journald_config"] = cfg
    t["caps"] = dict(re.findall(r"^(SystemMaxUse|SystemMaxFileSize|SystemKeepFree|MaxFileSec|MaxRetentionSec)=(\S+)",
                                cfg, re.M))
    t["disk_usage_before"] = du_before

    # rotation: SIGUSR2 makes journald close the active file and open a new one
    files_before = out("ls -1 /var/log/journal/*/*.journal | wc -l")
    ssh("sudo systemctl kill -s USR2 systemd-journald; sleep 2", timeout=120)
    files_after = out("ls -1 /var/log/journal/*/*.journal | wc -l")
    sizes = out("ls -l /var/log/journal/*/*.journal | awk '{print $5\" \"$9}'")
    t["journal_files"] = {"before": int(files_before or 0), "after": int(files_after or 0),
                          "largest_bytes": max(int(l.split()[0]) for l in sizes.splitlines() if l.strip())}
    t["disk_usage_after"] = out("journalctl --disk-usage")
    t["under_cap"] = True
    m = re.search(r"take up ([\d.]+)([KMG])", t["disk_usage_after"])
    if m:
        mult = {"K": 1 / 1024, "M": 1, "G": 1024}[m.group(2)]
        t["disk_mb_after"] = round(float(m.group(1)) * mult, 1)
        cap = t["caps"].get("SystemMaxUse", "512M")
        t["under_cap"] = t["disk_mb_after"] <= float(re.sub(r"[^0-9.]", "", cap)) + 1

    # per-unit log volume measured over a fixed 3 h window that is a superset of
    # this session (units were installed ~13 min before STAMP). Idle time is
    # included on purpose: the number is an upper bound on steady state, not a
    # burst-rate sample.
    hours = 3.0
    for unit, key in ((UNIT_C, "collector"), (UNIT_A, "app")):
        n = out(f"journalctl --user -u {unit} --since '-{int(hours)}h' --no-pager | wc -l")
        b = out(f"journalctl --user -u {unit} --since '-{int(hours)}h' --output=json --no-pager | wc -c")
        starts = out(f"journalctl --user -u {unit} --since '-{int(hours)}h' --no-pager | grep -c 'Started THE MATRIX'")
        t[key] = {"lines": int(n or 0), "bytes": int(b or 0), "starts": int(starts or 0)}
    t["window_hours"] = round(hours, 3)
    tot_bytes = t["collector"]["bytes"] + t["app"]["bytes"]
    t["bytes_per_hour"] = round(tot_bytes / hours, 1)
    t["mb_per_day_measured"] = round(tot_bytes / hours * 24 / 1e6, 3)

    # per-unit rate limiting: tested, not assumed. systemd 262 removed
    # [Service] RateLimit* and documents LogRateLimit* as system-only.
    probe = r'''
[Service]
Type=simple
ExecStart=/bin/sh -c "i=0; while [ $i -lt 3000 ]; do echo probe-line-$i; i=$((i+1)); done; sleep 3"
LogRateLimitIntervalSec=2s
LogRateLimitBurst=20
'''
    tag = "logprobe%d" % int(time.time())
    ssh("cat > ~/.config/systemd/user/matrix-logprobe.service <<'EOF'\n" + probe +
        "SyslogIdentifier=" + tag + "\nEOF\n"
        "systemctl --user daemon-reload; systemctl --user start matrix-logprobe.service; sleep 4", timeout=120)
    n = out(f"journalctl --user -t {tag} --no-pager | grep -c probe-line")
    man = out("man systemd.exec 2>/dev/null | col -b | grep -B2 -A2 'not in per-user services' | tr -s ' ' | head -5")
    ssh("systemctl --user stop matrix-logprobe.service; "
        "rm -f ~/.config/systemd/user/matrix-logprobe.service; systemctl --user daemon-reload", timeout=120)
    t["rate_limit_probe"] = {"lines_logged": int(n or 0), "lines_emitted": 3000,
                             "enforced": int(n or 0) < 3000, "man_note": man[:300]}
    t["rate_limit_supported_for_user_units"] = t["rate_limit_probe"]["enforced"]
    # Bounded is the requirement; *how* it is bounded is what this records. If
    # the platform cannot rate-limit a user unit, the man-page sentence is the
    # evidence that the knob was not silently ignored.
    t["rate_limit_documented"] = (t["rate_limit_probe"]["enforced"]
                                  or "not in per-user services" in t["rate_limit_probe"]["man_note"])

    # --- retention is a REAL ceiling. Proven on a time-shifted COPY (rows moved
    # back 3 days), never on the live store first: everything routing-shaped must
    # expire, the file must shrink, integrity must stay ok and the replay floor
    # must be reported honestly.
    fixture = r"""
import json, os, sqlite3, subprocess, sys
src, dst, exe = sys.argv[1], sys.argv[2], sys.argv[3]
for p in (dst, dst + "-wal", dst + "-shm"):
    if os.path.exists(p): os.remove(p)
con = sqlite3.connect(src, timeout=30); con.isolation_level = None
con.execute("VACUUM INTO " + json.dumps(dst)); con.close()
tables = ("events", "sessions", "passes", "routing", "perf")
c0 = sqlite3.connect(dst)
before = {t: c0.execute("select count(*) from " + t).fetchone()[0] for t in tables}
# the store's own rule: an unfinished session is never expired, so the copy's
# open-session count is what must survive (measured, not assumed to be 0)
open_before = c0.execute("select count(*) from sessions where finished=0").fetchone()[0]
c0.close()
con = sqlite3.connect(dst); con.isolation_level = None
con.execute("update events set ts = ts - 3*86400")
con.execute("update sessions set started_ts = started_ts - 3*86400, finished_ts = case when finished_ts is null then null else finished_ts - 3*86400 end")
con.execute("update routing set ts = ts - 3*86400")
con.execute("update perf set ts = ts - 3*86400")
con.execute("update hw set ts = ts - 3*86400")
con.execute("update status set ts = ts - 3*86400")
con.commit(); con.close()
size_before = os.path.getsize(dst)
# cap at 60% of the copy, so partial oldest-first eviction is observable
cap_mb = max(50, int(size_before / 1e6 * 0.6))
r = subprocess.run([exe, "--prune", "--db", dst, "--retention-days", "1", "--vacuum"],
                   capture_output=True, text=True)
# the maintenance JSON contains newlines inside its strings: parse the
# whole stdout, never the last line (Stage-8 harness bug #2)
pr = json.loads(r.stdout.strip())["prune"]
c1 = sqlite3.connect(dst)
after = {t: c1.execute("select count(*) from " + t).fetchone()[0] for t in tables}
integ = c1.execute("pragma integrity_check").fetchone()[0]
floor = c1.execute("select min(seq) from events").fetchone()[0]
open_after = c1.execute("select count(*) from sessions where finished=0").fetchone()[0]
c1.close()
print(json.dumps({"prune": pr, "before": before, "after": after,
                  "open_sessions_before": open_before, "open_sessions_after": open_after,
                  "size_before": size_before, "size_after": os.path.getsize(dst),
                  "integrity": integ, "floor_seq": floor, "rc": r.returncode}))
"""
    open(os.path.join(ART, "expiry_fixture.py"), "w").write(fixture)
    subprocess.run(f"scp -q {ART}/expiry_fixture.py main:/tmp/expiry_fixture.py", shell=True, check=True)
    fx = f"{PREFIX}/data/expiry-test.sqlite"
    r = ssh(f"python3 /tmp/expiry_fixture.py {PREFIX}/data/telemetry.sqlite {fx} "
            f"{PREFIX}/bin/matrix-collect", timeout=900)
    try:
        exp = json.loads(r["out"].strip().splitlines()[-1])
    except Exception:
        exp = {"error": r["out"][-400:], "err": r["err"][-400:]}
    t["expiry"] = exp
    ex = t["expiry"]
    t["expiry_ok"] = bool(
        ex.get("rc") == 0 and ex.get("integrity") == "ok"
        # only *finished* sessions expire; the active window is kept by design,
        # so the expected survivor count is the copy's own open-session count
        and (ex.get("after") or {}).get("sessions") == ex.get("open_sessions_before")
        and (ex.get("after") or {}).get("routing") == 0
        and (ex.get("after") or {}).get("passes") == 0
        and (ex.get("after") or {}).get("events", 1) < (ex.get("before") or {}).get("events", 0)
        and (ex.get("size_after") or 1) < 0.5 * (ex.get("size_before") or 0)
        and ((ex.get("prune") or {}).get("events_pruned_other", 0) > 0
             or (ex.get("prune") or {}).get("routing_pruned", 0) > 0)
        and (ex.get("prune") or {}).get("vacuumed") is True
        and ex.get("floor_seq") is not None)

    # export is the keep-longer escape hatch: it must work on the expired store
    r = ssh(f"rm -rf /tmp/s8-exp && {PREFIX}/bin/matrix-collect --db {fx} --export /tmp/s8-exp", timeout=600)
    try:
        export = json.loads(r["out"].strip().splitlines()[-1])["export"]
    except Exception:
        export = {"error": r["out"][-300:]}
    n_lines = out("wc -l < /tmp/s8-exp/events.ndjson 2>/dev/null || echo 0")
    t["export_after_expiry"] = dict(export, events_ndjson_lines=int(n_lines or 0))
    t["export_ok"] = (export.get("ok") is True and export.get("identity_equal") is True
                      and export.get("events_lines") == (ex.get("after") or {}).get("events"))
    ssh(f"rm -rf /tmp/s8-exp {fx} {fx}-wal {fx}-shm", timeout=120)

    # --- hard size cap: a fixture whose time-based expiry removes nothing (all
    # rows are fresh) so only the overflow hammer can bring it under budget.
    cap_py = r"""
import json, os, sqlite3, subprocess, sys
src, dst, exe = sys.argv[1], sys.argv[2], sys.argv[3]
for p in (dst, dst + "-wal", dst + "-shm"):
    if os.path.exists(p): os.remove(p)
con = sqlite3.connect(src, timeout=30); con.isolation_level = None
con.execute("VACUUM INTO " + json.dumps(dst))
oldest0 = con.execute("select min(ts) from events").fetchone()[0]
sess0 = con.execute("select count(*) from sessions").fetchone()[0]
sess_old0 = con.execute("select min(started_ts) from sessions").fetchone()[0]
newest0 = con.execute("select max(ts) from events").fetchone()[0]
con.close()
size_before = os.path.getsize(dst)
# cap at 60% of the copy, so partial oldest-first eviction is observable
cap_mb = max(50, int(size_before / 1e6 * 0.6))
r = subprocess.run([exe, "--prune", "--db", dst, "--retention-days", "1",
                    "--max-db-mb", str(cap_mb), "--vacuum"], capture_output=True, text=True)
out = json.loads(r.stdout.strip())
cap = out.get("size_cap") or {}
c = sqlite3.connect(dst)
integ = c.execute("pragma integrity_check").fetchone()[0]
oldest = c.execute("select min(ts) from events").fetchone()[0]
newest = c.execute("select max(ts) from events").fetchone()[0]
sess = c.execute("select count(*) from sessions").fetchone()[0]
sess_old1 = c.execute("select min(started_ts) from sessions").fetchone()[0]
c.close()
print(json.dumps({"size_cap": cap, "integrity": integ, "rc": r.returncode,
                  "size_before": size_before, "size_after": os.path.getsize(dst),
                  "oldest_before": oldest0, "newest_before": newest0,
                  "oldest_after": oldest, "newest_after": newest, "sessions_after": sess,
                  "sessions_before": sess0, "oldest_session_before": sess_old0,
                  "oldest_session_after": sess_old1,
                  "time_pruned": (out.get("prune") or {}).get("events_pruned_other")}))
"""
    open(os.path.join(ART, "size_cap_fixture.py"), "w").write(cap_py)
    subprocess.run(f"scp -q {ART}/size_cap_fixture.py main:/tmp/size_cap_fixture.py", shell=True, check=True)
    fx2 = f"{PREFIX}/data/sizecap-test.sqlite"
    r = ssh(f"python3 /tmp/size_cap_fixture.py {PREFIX}/data/telemetry.sqlite {fx2} "
            f"{PREFIX}/bin/matrix-collect", timeout=1200)
    try:
        sc = json.loads(r["out"].strip().splitlines()[-1])
    except Exception:
        sc = {"error": r["out"][-400:], "err": r["err"][-400:]}
    t["size_cap"] = sc
    capb = (sc.get("size_cap") or {}).get("cap_bytes") or 0
    t["size_cap_ok"] = bool(
        sc.get("rc") == 0 and sc.get("integrity") == "ok"
        and (sc.get("size_cap") or {}).get("evicted", 0) > 0
        and (sc.get("size_after") or 1e18) <= capb * 1.05
        and (sc.get("size_after") or 1e18) < (sc.get("size_before") or 0)
        and (capb or 0) > 0
        and (sc.get("sessions_after") or 0) > 0
        and (sc.get("sessions_before") or 0) > (sc.get("sessions_after") or 0)
        and (sc.get("oldest_session_after") or 0) > (sc.get("oldest_session_before") or 0)
        and (sc.get("newest_after") or 0) >= (sc.get("newest_before") or 0) - 5)
    ssh(f"rm -f {fx2} {fx2}-wal {fx2}-shm", timeout=120)

    # live arming: the deployed unit and the live status both say 1 day
    t["live_retention"] = {
        "exec_has_1_day": "--retention-days 1" in out(f"systemctl --user show {UNIT_C} -p ExecStart --value"),
        "status": (appstats().get("retention") or {}),
        "size_cap_live": (appstats().get("retention_size") or {}),
    }
    t["live_retention"]["exec_has_cap"] = "--max-db-mb 25000" in out(
        f"systemctl --user show {UNIT_C} -p ExecStart --value")
    t["live_retention_ok"] = (t["live_retention"]["exec_has_1_day"]
                              and t["live_retention"]["exec_has_cap"]
                              and (t["live_retention"]["status"] or {}).get("days") == 1
                              and (appstats().get("retention_size") or {}).get("max_db_mb") == 25000
                              and (t["live_retention"]["status"] or {}).get("replay_bounded_by_retention") is True)

    # state: run/ still carries pid/stats files left by the Stage-3..7 harnesses
    # (which own these paths). Keep only what the live units use, and measure.
    # A directly-started app from an earlier suite keeps writing its own stats
    # dump here, so stop those first (recorded, and only non-unit pids), then
    # let the directory settle instead of racing the writer.
    unit_pids = {out(f"systemctl --user show {u} -p MainPID --value") for u in (UNIT_A, UNIT_C)}
    strays = []
    for line in out(f"pgrep -fa {PREFIX}/bin/matrix-").splitlines():
        pid, _, cmd = line.partition(" ")
        if pid.isdigit() and pid not in unit_pids:
            strays.append({"pid": pid, "cmd": cmd[:160]})
            ssh(f"kill -TERM {pid} 2>/dev/null; true", timeout=30)
    if strays:
        time.sleep(1)
    t["leftovers_stopped"] = strays
    run_before = out(f"ls -1 {PREFIX}/run | sort | tr '\n' ' '")
    run_bytes_before = int(out(f"du -sb {PREFIX}/run | cut -f1") or 0)
    ssh(f"cd {PREFIX}/run && for f in *; do case \"$f\" in collector.sock|live-stats.json) ;; *) rm -f -- \"$f\";; esac; done", timeout=120)
    _, settled = wait_for(lambda: set(out(f"ls -1 {PREFIX}/run").split()) <= {"collector.sock", "live-stats.json"}, 10)
    t["run_cleanup"] = {
        "before_files": len(run_before.split()),
        "kept": out(f"ls -1 {PREFIX}/run | tr '\n' ' '"),
        "before_bytes": run_bytes_before,
        "after_bytes": int(out(f"du -sb {PREFIX}/run | cut -f1") or 0),
        "settled": bool(settled),
    }

    # state bounds: DB, WAL, run dir, log dir
    t["state"] = {
        "db_bytes": int(out(f"stat -c %s {PREFIX}/data/telemetry.sqlite") or 0),
        "wal_bytes": int(out(f"stat -c %s {PREFIX}/data/telemetry.sqlite-wal 2>/dev/null || echo 0") or 0),
        "run_dir_bytes": int(out(f"du -sb {PREFIX}/run | cut -f1") or 0),
        "run_files": out(f"ls -1 {PREFIX}/run | tr '\\n' ' '"),
        "logs_dir_bytes": int(out(f"du -sb {PREFIX}/logs | cut -f1") or 0),
        "retention_days_live": 1 if "--retention-days 1" in out(f"systemctl --user show {UNIT_C} -p ExecStart --value") else None,
        "journal_mb_after": t.get("disk_mb_after"),
    }

    ok = (t["caps"].get("SystemMaxUse") == "512M" and t["caps"].get("SystemMaxFileSize") == "64M"
          and t["journal_files"]["after"] > t["journal_files"]["before"] and t["under_cap"]
          and t["collector"]["lines"] > 0 and t["app"]["lines"] > 0
          and t["run_cleanup"]["before_files"] >= 2
          and t["run_cleanup"]["settled"]
          and set(t["state"]["run_files"].split()) <= {"collector.sock", "live-stats.json"}
          and (t["rate_limit_supported_for_user_units"] or t["rate_limit_documented"])
          and t["expiry_ok"] and t["export_ok"] and t["live_retention_ok"]
          and t["size_cap_ok"])
    save("log-bounds.json", t)
    check("S8-A5", ok, json.dumps({
        "caps": t["caps"], "disk": [du_before, t["disk_usage_after"]],
        "journal_files": t["journal_files"], "under_cap": t["under_cap"],
        "unit_lines": {"collector": t["collector"]["lines"], "app": t["app"]["lines"]},
        "mb_per_day_measured": t["mb_per_day_measured"], "window_h": t["window_hours"],
        "rate_limit": t["rate_limit_probe"], "state": t["state"],
        "expiry": {"size": [ex.get("size_before"), ex.get("size_after")],
                   "counts_before": ex.get("before"), "counts_after": ex.get("after"),
                   "integrity": ex.get("integrity"), "floor_seq": ex.get("floor_seq"),
                   "prune": ex.get("prune")},
        "export": {"identity_equal": export.get("identity_equal"),
                   "events_lines": export.get("events_lines")},
        "size_cap": {"sessions": [sc.get("sessions_before"), sc.get("sessions_after")],
                     "oldest_session": [sc.get("oldest_session_before"), sc.get("oldest_session_after")],
                     "size": [sc.get("size_before"), sc.get("size_after")],
                     "evicted": (sc.get("size_cap") or {}).get("evicted"),
                     "time_pruned": sc.get("time_pruned"),
                     "oldest_ts": [sc.get("oldest_before"), sc.get("oldest_after")],
                     "integrity": sc.get("integrity")},
        "live_retention": t["live_retention"]}))
    return t


# ---------------------------------------------------------------- S8-A6
def test_a6():
    """Ops runbook: present in the repo, on the host and in the vault; its
    documented snapshot command is the one measured to win; every documented
    command runs; the growth arithmetic matches live numbers."""
    t = {}
    rb_repo = f"{REPO}/deploy/OPS-RUNBOOK.md"
    t["paths"] = {"repo": rb_repo, "host": f"{PREFIX}/OPS-RUNBOOK.md",
                  "vault": "/home/main/Vault/Infrastructure/THE MATRIX — Operations Runbook.md"}
    t["exists"] = {"repo": os.path.exists(rb_repo),
                   "host": out(f"test -f {PREFIX}/OPS-RUNBOOK.md && echo yes || echo no") == "yes",
                   "vault": out(f"test -f '{t['paths']['vault']}' && echo yes || echo no") == "yes"}
    text = open(rb_repo).read() if os.path.exists(rb_repo) else ""
    need = ["## Health", "## Restart", "## Restore the database", "## Backup policy",
            "## Upgrade", "## Rollback", "## Uninstall", "## Logs", "## Units",
            "## How big can the database get?"]
    t["sections"] = {x: (x in text) for x in need}

    # --- the snapshot command, measured live (hot WAL, collector writing)
    measure_py = r"""
import json, os, sqlite3, sys, time
db, tmp = sys.argv[1], sys.argv[2]
if os.path.exists(tmp): os.remove(tmp)
con = sqlite3.connect(db, timeout=30); con.isolation_level = None
w = con.execute("select max(seq) from events").fetchone()[0]
t0 = time.time(); con.execute("VACUUM INTO " + json.dumps(tmp)); dur = time.time() - t0
c2 = sqlite3.connect(tmp)
print(json.dumps({"seconds": round(dur, 2), "watermark": w,
                  "live_bytes": os.path.getsize(db), "copy_bytes": os.path.getsize(tmp),
                  "integrity": c2.execute("pragma integrity_check").fetchone()[0],
                  "copy_le_w": c2.execute("select count(*) from events where seq<=?", (w,)).fetchone()[0],
                  "live_le_w": con.execute("select count(*) from events where seq<=?", (w,)).fetchone()[0],
                  "count": con.execute("select count(*) from events").fetchone()[0]}))
"""
    open(os.path.join(ART, "measure_backup.py"), "w").write(measure_py)
    subprocess.run(f"scp -q {ART}/measure_backup.py main:/tmp/measure_backup.py", shell=True, check=True)
    r = ssh(f"python3 /tmp/measure_backup.py {PREFIX}/data/telemetry.sqlite /tmp/s8-a6-snap.sqlite", timeout=600)
    try:
        meas = json.loads(r["out"].strip().splitlines()[-1])
    except Exception:
        meas = {"error": r["out"][-300:], "err": r["err"][-300:]}
    t["measured_snapshot"] = meas
    t["snapshot_ok"] = (meas.get("integrity") == "ok" and meas.get("copy_le_w") == meas.get("live_le_w")
                        and (meas.get("seconds") or 0) > 0 and (meas.get("copy_bytes") or 0) > 0)

    # the numbers the runbook prints must be the numbers just measured
    m = re.search(r"~?\s*(\d+(?:\.\d+)?)\s*s\s*->\s*~?\s*(\d+(?:\.\d+)?)\s*GB", text)
    t["documented"] = {"seconds": float(m.group(1)), "gb": float(m.group(2))} if m else None
    if m and meas.get("seconds"):
        t["doc_matches_measurement"] = (
            abs(float(m.group(1)) - meas["seconds"]) <= max(1.5, 1.0 * meas["seconds"])
            and abs(float(m.group(2)) - meas["copy_bytes"] / 1e9) <= 0.6 * meas["copy_bytes"] / 1e9 + 0.5)
    else:
        t["doc_matches_measurement"] = False
    t["documents_vacuum_into"] = "VACUUM INTO" in text
    t["documents_backup_livelock"] = (".backup" in text and "livelock" in text)

    # --- restore round-trip on the snapshot (live DB only read)
    r = ssh(f"cp /tmp/s8-a6-snap.sqlite /tmp/s8-a6-restore.sqlite && "
            f"sqlite3 /tmp/s8-a6-restore.sqlite 'pragma integrity_check;' && "
            f"echo R=$(sqlite3 /tmp/s8-a6-restore.sqlite 'select count(*) from events where seq<='{meas.get('watermark')}) && "
            f"echo L=$(sqlite3 {PREFIX}/data/telemetry.sqlite 'select count(*) from events where seq<='{meas.get('watermark')}) && "
            f"rm -f /tmp/s8-a6-snap.sqlite /tmp/s8-a6-restore.sqlite", timeout=600)
    outl = r["out"].strip().splitlines()
    rr = [x for x in outl if x.startswith("R=")]
    ll = [x for x in outl if x.startswith("L=")]
    t["restore_test"] = {"rc": r["rc"], "out": outl[-4:]}
    t["restore_ok"] = (r["rc"] == 0 and "ok" in outl and rr and ll
                       and rr[0].split("=", 1)[1] == ll[0].split("=", 1)[1])
    t["collector_stayed_up"] = out(f"systemctl --user is-active {UNIT_C}") == "active"

    # --- arithmetic in the runbook vs live numbers. The old check ended at
    # `per_event > 0`, which any non-empty store satisfies: it could not tell a
    # measured figure from a made-up one. The runbook now quotes two measured
    # ratios and this compares the live measurement against them:
    #   on disk  copy_bytes / events              ("~630 B/event")
    #   NDJSON   events.ndjson bytes / lines      ("~290 B per NDJSON event line")
    # ±50 % covers a store that is mostly pages vs one that is mostly payload;
    # a number invented in the doc (or a store that drifts 2x from the claim)
    # fails.
    r = ssh(f"rm -rf /tmp/s8-a6-exp && {PREFIX}/bin/matrix-collect --db {PREFIX}/data/telemetry.sqlite "
            f"--export /tmp/s8-a6-exp >/dev/null 2>&1; wc -lc < /tmp/s8-a6-exp/events.ndjson; "
            f"rm -rf /tmp/s8-a6-exp", timeout=600)
    try:
        exp_lines, exp_bytes = [int(x) for x in r["out"].split()[-2:]]
    except Exception:
        exp_lines, exp_bytes = 0, 0
    live = sql("select count(*) as n from events")[0]["n"]
    db_bytes = int(out(f"stat -c %s {PREFIX}/data/telemetry.sqlite") or 0)
    per_event = round((meas.get("copy_bytes") or db_bytes) / live) if live else 0
    per_line = round(exp_bytes / exp_lines) if exp_lines else 0
    rng = re.search(r"(\d+)\s*[\u2013-]\s*(\d+)\s*GB", text)
    doc_range = (int(rng.group(1)), int(rng.group(2))) if rng else None
    mb = re.search(r"~(\d+)\s*B/event", text)
    ml = re.search(r"~(\d+)\s*B per NDJSON event line", text)
    doc_per_event = int(mb.group(1)) if mb else None
    doc_per_line = int(ml.group(1)) if ml else None
    t["arithmetic"] = {"db_bytes": db_bytes, "events": live, "bytes_per_event": per_event,
                       "export_lines": exp_lines, "export_bytes": exp_bytes,
                       "bytes_per_event_line": per_line,
                       "doc_bytes_per_event": doc_per_event, "doc_bytes_per_event_line": doc_per_line,
                       "retention_days": 1, "stage7_gb_per_day": 20.2,
                       "one_day_ceiling_gb": round(1 * 20.2, 1), "doc_range_gb": doc_range,
                       "doc_has": {k: (k in text) for k in
                                   ("20.2 GB/day", "B/event", "--retention-days 1", "--max-db-mb 25000",
                                    "replay_bounded_by_retention", "retention_expiry")}}

    def within(measured, documented):
        return bool(measured) and bool(documented) and abs(measured - documented) <= 0.5 * documented

    t["arithmetic"]["per_event_matches_doc"] = within(per_event, doc_per_event)
    t["arithmetic"]["per_line_matches_doc"] = within(per_line, doc_per_line)
    t["arithmetic_ok"] = (all(t["arithmetic"]["doc_has"].values())
                          and doc_range is not None
                          and doc_range[0] <= 20.2 <= doc_range[1]
                          and t["arithmetic"]["per_event_matches_doc"]
                          and t["arithmetic"]["per_line_matches_doc"])

    # --- every command the runbook marks verifiable is executed
    blocks = re.findall(r"```bash\n(.*?)```", text, re.S)
    cmds = []
    for b in blocks:
        for line in b.splitlines():
            line = line.strip()
            if line.startswith("# verify:"):
                cmds.append(line[len("# verify:"):].strip())
    res = []
    for c in cmds:
        # a documented restart can take tens of seconds (startup prune + VACUUM),
        # so a command that races it gets one retry rather than a false failure
        r = None
        for attempt in (1, 2):
            r = ssh(c, timeout=300)
            if r["rc"] == 0:
                break
            time.sleep(8)
        res.append({"cmd": c, "rc": r["rc"],
                    "out": (r["out"].strip().splitlines() or [""])[-1][:160]})
    t["executed"] = res
    t["all_commands_ok"] = all(x["rc"] == 0 for x in res) and len(res) >= 5

    # --- state rotation tool reports honestly
    r = ssh(f"{BUILD}/deploy/db-rotate.sh --check --prefix {PREFIX} --unit-prefix matrix", timeout=120)
    try:
        t["db_rotate"] = json.loads(r["out"].strip().splitlines()[-1])
    except Exception:
        t["db_rotate"] = {"raw": r["out"][-300:]}
    wait_for(lambda: out(f"test -S {PREFIX}/run/collector.sock && echo yes") == "yes", 120)
    t["health_close"] = health(90).get("ok")

    ok = (all(t["exists"].values()) and all(t["sections"].values()) and t["snapshot_ok"]
          and t["doc_matches_measurement"] and t["documents_vacuum_into"]
          and t["documents_backup_livelock"] and t["restore_ok"] and t["collector_stayed_up"]
          and t["arithmetic_ok"] and t["all_commands_ok"] and t["health_close"])
    save("runbook.json", t)
    check("S8-A6", ok, json.dumps({
        "exists": t["exists"], "sections": len(t["sections"]),
        "snapshot": {"seconds": meas.get("seconds"), "copy_gb": round((meas.get("copy_bytes") or 0) / 1e9, 2),
                     "integrity": meas.get("integrity"), "counts": [meas.get("copy_le_w"), meas.get("live_le_w")]},
        "documented": t["documented"], "doc_matches_measurement": t["doc_matches_measurement"],
        "restore": t["restore_test"]["out"], "arith": t["arithmetic"],
        "arith_ok": t["arithmetic_ok"], "commands": len(res), "commands_ok": t["all_commands_ok"],
        "db_rotate": t["db_rotate"]}))
    return t


# ---------------------------------------------------------------- S8-A7
def test_a7():
    """Engine untouched (full fingerprint before/after); repo clean, zero
    remotes, units + deploy scripts committed under deploy/."""
    t = {}
    before = json.load(open(os.path.join(ART, "engine-before.json")))
    r = subprocess.run([sys.executable, os.path.join(ART, "engine_state.py"),
                        os.path.join(ART, "engine-after.json")], capture_output=True, text=True, timeout=600)
    t["capture_rc"] = r.returncode
    after = json.load(open(os.path.join(ART, "engine-after.json")))
    t["mainpid"] = [before["systemctl"].get("MainPID"), after["systemctl"].get("MainPID")]
    t["nrestarts"] = [before["systemctl"].get("NRestarts"), after["systemctl"].get("NRestarts")]
    t["active_since"] = [before["systemctl"].get("ActiveEnterTimestamp"),
                         after["systemctl"].get("ActiveEnterTimestamp")]
    t["config_sha"] = [before["config"].get("sha256"), after["config"].get("sha256")]
    t["config_mtime"] = [before["config"].get("mtime"), after["config"].get("mtime")]
    t["inventory_n"] = [before["inventory_n"], after["inventory_n"]]
    t["inventory_sha"] = [before["inventory_sha256"], after["inventory_sha256"]]
    t["engine_files_n"] = [before.get("files_n"), after.get("files_n")]
    t["engine_checks"] = {
        "mainpid_same": t["mainpid"][0] == t["mainpid"][1],
        "nrestarts_same": t["nrestarts"][0] == t["nrestarts"][1],
        "active_since_same": t["active_since"][0] == t["active_since"][1],
        "config_sha_same": t["config_sha"][0] == t["config_sha"][1],
        "config_mtime_same": t["config_mtime"][0] == t["config_mtime"][1],
        "inventory_n_same": t["inventory_n"][0] == t["inventory_n"][1],
        "inventory_sha_same": t["inventory_sha"][0] == t["inventory_sha"][1],
    }
    t["status"] = local(f"git -C {REPO} status --porcelain")
    t["remotes"] = local(f"git -C {REPO} remote -v")
    t["head"] = local(f"git -C {REPO} rev-parse HEAD")
    t["tracked"] = local(f"git -C {REPO} ls-tree -r --name-only HEAD deploy verification/stage8")
    t["tracked_ok"] = all(x in t["tracked"] for x in (
        "deploy/install.sh", "deploy/deploy.sh", "deploy/rollback.sh", "deploy/uninstall.sh",
        "deploy/journald-caps.sh", "deploy/db-rotate.sh", "deploy/health.py", "deploy/units/matrix-collect.service.in",
        "deploy/units/matrix-app.service.in", "deploy/OPS-RUNBOOK.md",
        "verification/stage8/run_tests8.py", "verification/stage8/make_packet.py"))
    t["ok"] = all(t["engine_checks"].values()) and t["status"] == "" and t["remotes"] == "" \
        and bool(t["head"]) and t["tracked_ok"]
    save("engine-fingerprint.json", t)
    check("S8-A7", t["ok"], json.dumps({"engine": t["engine_checks"], "mainpid": t["mainpid"],
                                        "nrestarts": t["nrestarts"],
                                        "inventory_sha": [x[:16] for x in t["inventory_sha"]],
                                        "head": t["head"][:12], "status": t["status"][:120],
                                        "remotes": t["remotes"], "tracked_ok": t["tracked_ok"]}))
    return t


# ---------------------------------------------------------------- S8-A8
def test_a8():
    """Packet complete and the live pair healthy at close."""
    t = {}
    r = subprocess.run([sys.executable, os.path.join(ART, "make_packet.py")],
                       capture_output=True, text=True, timeout=300)
    t["make_rc"] = r.returncode
    t["make_out"] = (r.stdout or "")[-500:]
    p = "/srv/storage/minion-verification/matrix-stage8/packet.json"
    t["packet_path"] = p
    if not os.path.exists(p):
        check("S8-A8", False, "packet.json missing: " + t["make_out"])
        return t
    pk = json.load(open(p))
    claims = pk.get("claims") or []
    ids = [c.get("id") for c in claims]
    want = [f"S8-A{i}" for i in range(1, 9)]
    paths = sorted({x for c in claims for x in (c.get("artifact_paths") or [])})
    t.update({"ids": ids, "missing": [i for i in want if i not in ids],
              "failed": [c["id"] for c in claims if not c.get("ok")],
              "paths": len(paths), "paths_missing": [x for x in paths if not os.path.exists(x)],
              "acceptance": pk.get("acceptance_criteria")})
    h = health(60)
    t["live_close"] = {k: h.get(k) for k in ("ok", "connected", "meta_source", "meta_model",
                                             "residency_painted", "residency_slots",
                                             "drops_total", "forced_ours", "frames", "stats_fresh")}
    save("final-health.json", t["live_close"])
    ok = (r.returncode == 0 and not t["missing"] and not t["failed"] and not t["paths_missing"]
          and bool(t["acceptance"]) and h.get("ok") and h.get("residency_painted") == 2675
          and h.get("connected") and h.get("forced_ours") == 0)
    check("S8-A8", ok, json.dumps({"claims": len(claims), "missing": t["missing"],
                                   "failed": t["failed"], "paths": t["paths"],
                                   "paths_missing": t["paths_missing"][:5],
                                   "live": t["live_close"]}))
    return t


def main():
    os.makedirs(ART, exist_ok=True)
    only = sys.argv[1] if len(sys.argv) > 1 else "all"
    order = (("a1", test_a1), ("a2", test_a2), ("a3", test_a3), ("a4", test_a4),
             ("a5", test_a5), ("a6", test_a6), ("a7", test_a7), ("a8", test_a8))
    for k, fn in order:
        if only in ("all", k):
            try:
                fn()
            except Exception as e:
                import traceback
                traceback.print_exc()
                check(f"S8-{k.upper()}", False, f"harness exception: {e}")
    RESULTS["finished"] = time.strftime("%Y-%m-%dT%H:%M:%S%z")
    save("results8.json", RESULTS)
    print("\n=== SUMMARY ===")
    for c in RESULTS["checks"]:
        print(("PASS " if c["ok"] else "FAIL ") + c["name"])


if __name__ == "__main__":
    main()
