#!/usr/bin/env python3
"""Stage-7 soak: >=60 min of real mixed inference traffic against the live pair.

Runs on blackie, drives host `main` over ssh. Own traffic goes through the
proxy (:8091); an independent client process on main talks to the engine
directly (:8080) so the collector sees a second, unmanaged client path; any
engine request that neither driver accounts for is reported as foreign and
never guessed away.

Samples every SAMPLE_S seconds:
  app     RSS, fd count, frame-interval histogram (p95 of each interval)
  coll    RSS, CPU (utime+stime), fd count, queue depth / drops, trace offset
  db      db file + WAL bytes, row counts per class (routing, events by kind,
          passes, sessions, perf, hw, status, gaps)
  engine  totals.requests, live state

Writes samples.jsonl + soak.json under the artifact dir; all numbers in the
report come from those samples.
"""
import json
import os
import shlex
import subprocess
import sys
import threading
import time
import urllib.request

ART = os.path.dirname(os.path.abspath(__file__))
R = "/home/main/the-matrix"
DB = f"{R}/data/s7soak.sqlite"
SOCK = f"{R}/run/collector.sock"
STATS = f"{R}/run/s7soak-stats.json"
CTL = f"{R}/run/s7soak-ctl.txt"
TRACE = "/home/main/strata-0.2.4.2/data/routing-trace.bin"
ENGINE_CONF = "/home/main/strata-0.2.4.2/strata-iq3_s.json"
PROXY = "http://192.168.1.204:8091/v1/chat/completions"
DIRECT = "http://192.168.1.204:8080/v1/chat/completions"
HYPR = "efb50993780079460b0cbed1363e2166a2de1d9f_1793365610_1423497934"
ENV = (f"export XDG_RUNTIME_DIR=/run/user/1000 WAYLAND_DISPLAY=wayland-1 "
       f"HYPRLAND_INSTANCE_SIGNATURE={HYPR}; ")
MONITOR = "DP-1"
SAMPLE_S = 15.0
DURATION_MIN = float(os.environ.get("SOAK_MIN", "66"))
SAMPLE_PATH = os.path.join(ART, "samples.jsonl")

PROMPTS = [
    "Explain in three short sentences why expert routing in a mixture-of-experts model matters for inference cost.",
    "Write a dense technical paragraph of about 140 words on how block-sparse expert caches interact with speculative decoding.",
    "List five failure modes of a local inference server and one mitigation each.",
    "Summarise the difference between prefill and decode latency in two sentences.",
    "Describe how a top-k router load-balances experts, in about 90 words.",
    "Give a short checklist for monitoring a GPU inference host without a vendor tool.",
]

STOP = threading.Event()


def ssh(cmd, timeout=120):
    p = subprocess.run(["ssh", "main", cmd], capture_output=True, text=True, timeout=timeout)
    return {"rc": p.returncode, "out": p.stdout, "err": p.stderr}


def out(cmd, timeout=120):
    return ssh(cmd, timeout)["out"].strip()


def engine_key():
    return out("python3 -c \"import json;print(json.load(open('%s'))['api_key'])\"" % ENGINE_CONF)


def metrics():
    r = ssh(f"python3 {R}/bin/metrics.py")
    if r["rc"] != 0:
        raise RuntimeError("metrics: " + r["err"][:200])
    return json.loads(r["out"])


def proc_stat(pid):
    """RSS KiB, utime+stime ticks, fd count from /proc."""
    cmd = (f"cat /proc/{pid}/status 2>/dev/null | grep -E 'VmRSS|VmHWM' ; "
           f"cat /proc/{pid}/stat 2>/dev/null | cut -d' ' -f14,15 ; "
           f"ls /proc/{pid}/fd 2>/dev/null | wc -l")
    o = out(cmd)
    rss = hwm = 0
    ticks = 0
    fds = 0
    for line in o.splitlines():
        line = line.strip()
        if line.startswith("VmRSS:"):
            rss = int(line.split()[1])
        elif line.startswith("VmHWM:"):
            hwm = int(line.split()[1])
        elif line and not line.startswith("Vm") and len(line.split()) == 2 and line.split()[0].isdigit():
            ticks = int(line.split()[0]) + int(line.split()[1])
        elif line.isdigit():
            fds = int(line)
    return {"rss_kib": rss, "hwm_kib": hwm, "ticks": ticks, "fds": fds}


def db_counts():
    sql = ("select (select count(*) from routing),(select count(*) from events),"
           "(select count(*) from events where kind='expert_selected'),"
           "(select count(*) from passes),(select count(*) from sessions),"
           "(select count(*) from perf),(select count(*) from hw),(select count(*) from status),"
           "(select count(*) from gaps)")
    r = ssh(f"sqlite3 {DB} \"{sql}\"")
    if r["rc"] != 0:
        return {}
    v = [int(x) for x in r["out"].strip().split("|")]
    d = dict(zip(["routing", "events", "expert_events", "passes", "sessions", "perf", "hw", "status", "gaps"], v))
    pc = ssh(f"sqlite3 {DB} 'pragma page_count'")["out"].strip()
    ps = ssh(f"sqlite3 {DB} 'pragma page_size'")["out"].strip()
    fl = ssh(f"sqlite3 {DB} 'pragma freelist_count'")["out"].strip()
    d["page_count"] = int(pc or 0)
    d["page_size"] = int(ps or 0)
    d["freelist"] = int(fl or 0)
    d["db_bytes"] = d["page_count"] * d["page_size"]
    d["db_file_bytes"] = int(out(f"stat -c %s {DB} 2>/dev/null") or 0)
    d["wal_bytes"] = int(out(f"stat -c %s {DB}-wal 2>/dev/null") or 0)
    return d


def dbstat():
    r = ssh(f"sqlite3 {DB} \"select name,sum(pgsize) from dbstat group by name order by 2 desc\"")
    d = {}
    for line in r["out"].splitlines():
        if "|" in line:
            n, b = line.split("|", 1)
            d[n] = int(b)
    return d


def stats():
    r = ssh(f"cat {STATS}")
    if r["rc"] != 0 or not r["out"].strip():
        return {}
    try:
        return json.loads(r["out"])
    except Exception:
        return {}


def app_pid():
    v = out("pgrep -x matrix-app | head -1")
    return int(v) if v else None


def coll_pid():
    v = out("pgrep -x matrix-collect | head -1")
    return int(v) if v else None


def p95_between(h0, h1):
    """p95 of the frame intervals that fall in [h0,h1) (hist diff)."""
    diff = [max(0, b - a) for a, b in zip(h0, h1)]
    n = sum(diff)
    if n == 0:
        return None, 0
    target = int(0.95 * n) + 1
    acc = 0
    for i, c in enumerate(diff):
        acc += c
        if acc >= target:
            return (i + 1) * 5.0, n
    return 165.0, n


class Traffic(threading.Thread):
    """Own driver: alternating proxy/direct generations at a steady cadence."""

    def __init__(self):
        super().__init__(daemon=True)
        self.sent = []
        self.i = 0
        self.key = engine_key()

    def one(self, path):
        prompt = PROMPTS[self.i % len(PROMPTS)]
        self.i += 1
        # brief: own generations request max_tokens=150, alternating proxy/direct
        max_tokens = 150
        body = json.dumps({"model": "qwen3.8-flash-next-iq3_s",
                           "messages": [{"role": "user", "content": prompt}],
                           "max_tokens": max_tokens, "temperature": 0}).encode()
        headers = {"Content-Type": "application/json"}
        url = PROXY if path == "proxy" else DIRECT
        if path == "direct":
            headers["Authorization"] = "Bearer " + self.key
            headers["X-Matrix-Client"] = "soak-independent"
        req = urllib.request.Request(url, data=body, headers=headers)
        t0 = time.time()
        try:
            with urllib.request.urlopen(req, timeout=300) as r:
                resp = json.loads(r.read())
            rec = {"t0": t0, "t1": time.time(), "path": path, "ok": True,
                   "usage": resp.get("usage", {})}
        except Exception as e:  # noqa: BLE001
            rec = {"t0": t0, "t1": time.time(), "path": path, "ok": False, "error": str(e)[:200]}
        self.sent.append(rec)
        return rec

    def run(self):
        # stagger so the first generation starts a few seconds in
        time.sleep(8)
        while not STOP.is_set():
            path = "proxy" if self.i % 2 == 0 else "direct"
            self.one(path)
            # a little jitter keeps the load from being perfectly periodic;
            # 22-38 s between generations gives a sustained ~15-25% duty cycle
            # (long enough to keep the collector, DB and renderer under load
            # without turning the soak into a benchmark)
            STOP.wait(22 + (self.i % 5) * 4)


EXT_SRC = r'''#!/usr/bin/env python3
"""Independent client on main: direct engine path, its own process, no
collector knowledge. Writes its request count to a file so the soak report can
separate "own driver", "independent client" and "foreign" engine requests."""
import json, time, urllib.request
conf = json.load(open("/home/main/strata-0.2.4.2/strata-iq3_s.json"))
key = conf["api_key"]
n = 0
while True:
    body = json.dumps({"model": "qwen3.8-flash-next-iq3_s",
                       "messages": [{"role": "user", "content": "In one sentence, what does a top-k router do?"}],
                       "max_tokens": 48, "temperature": 0}).encode()
    r = urllib.request.Request("http://127.0.0.1:8080/v1/chat/completions", data=body,
                               headers={"Content-Type": "application/json",
                                        "Authorization": "Bearer " + key,
                                        "X-Matrix-Client": "independent"})
    try:
        urllib.request.urlopen(r, timeout=240).read()
        n += 1
    except Exception:
        pass
    open("/home/main/the-matrix/run/s7ext-count.txt", "w").write(str(n))
    time.sleep(300)
'''


def start_ext():
    """Run the independent client as a detached process on main."""
    ssh("mkdir -p ~/the-matrix/run")
    # write the source through ssh stdin, then launch detached
    p = subprocess.run(["ssh", "main", "cat > ~/the-matrix/bin/ext_client.py"],
                       input=EXT_SRC, text=True, capture_output=True, timeout=60)
    if p.returncode != 0:
        return {"ok": False, "err": p.stderr[:200]}
    out("rm -f ~/the-matrix/run/s7ext-count.txt")
    out(f"cd {R} && python3 bin/launch.py run/s7ext.pid logs/s7ext.log python3 bin/ext_client.py")
    return {"ok": True}


def stop_ext():
    n = out("cat ~/the-matrix/run/s7ext-count.txt 2>/dev/null || echo 0")
    ssh("pkill -f 'ext_clien[t].py'; true")
    try:
        return int(n)
    except Exception:
        return 0


def start_pair():
    ssh("pkill -x matrix-app; pkill -x matrix-collect; sleep 0.5; true")
    ssh(f"rm -f {DB} {DB}-wal {DB}-shm {SOCK} {STATS}")
    out(f"cd {R} && python3 bin/launch.py run/s7soak-col.pid logs/s7soak-col.log ./bin/matrix-collect "
        f"--path {TRACE} --socket {SOCK} --db {DB} --engine 127.0.0.1:8080 --engine-conf {ENGINE_CONF} "
        f"--retention-days 30 --event-line-hours 1")
    t0 = time.time()
    while time.time() - t0 < 15:
        if out(f"test -S {SOCK} && echo up") == "up":
            break
        time.sleep(0.2)
    ssh(f": > {CTL}")
    out(f"cd {R} && {ENV} python3 bin/launch.py run/s7soak-app.pid logs/s7soak-app.log ./bin/matrix-app "
        f"--socket {SOCK} --stats {STATS} --rain on --banner on --trails on --view 0 --monitor {MONITOR} --ctl {CTL}")
    t0 = time.time()
    while time.time() - t0 < 30:
        s = stats()
        if s and s.get("connected"):
            return True
        time.sleep(0.3)
    return False


def restore_live_pair():
    """Put back the pair the brief says was running (default socket/db)."""
    ssh("pkill -x matrix-app; pkill -x matrix-collect; sleep 0.5; true")
    out(f"cd {R} && python3 bin/launch.py run/collector.pid logs/collector.log ./bin/matrix-collect "
        f"--path {TRACE} --socket {R}/run/collector.sock --db {R}/data/telemetry.sqlite "
        f"--engine 127.0.0.1:8080 --engine-conf {ENGINE_CONF}")
    t0 = time.time()
    while time.time() - t0 < 15:
        if out(f"test -S {R}/run/collector.sock && echo up") == "up":
            break
        time.sleep(0.2)
    out(f"cd {R} && {ENV} python3 bin/launch.py run/app.pid logs/app.log ./bin/matrix-app "
        f"--socket {R}/run/collector.sock --stats {R}/run/live-stats.json --rain on --view 0 --monitor {MONITOR}")
    return out("pgrep -a -x matrix-collect; pgrep -a -x matrix-app")


def cpu_window(pid, secs):
    a = proc_stat(pid)
    t0 = time.time()
    time.sleep(secs)
    b = proc_stat(pid)
    clk = int(out("getconf CLK_TCK") or 100)
    return round(((b["ticks"] - a["ticks"]) / clk) / max(0.001, time.time() - t0) * 100.0, 3)


def app_vram(pid):
    o = out(f"nvidia-smi --query-compute-apps=pid,used_memory --format=csv,noheader,nounits | grep '^{pid},' || true")
    try:
        return int(o.split(",")[1].strip())
    except Exception:
        return None


def vram_used():
    o = out("nvidia-smi --query-gpu=memory.used --format=csv,noheader,nounits")
    try:
        return int(o.split()[0])
    except Exception:
        return None


def wait_idle(stable_s=3.0, timeout=120):
    t0 = time.time()
    last = None
    since = None
    m = metrics()
    while time.time() - t0 < timeout:
        m = metrics()
        if m.get("state") == "idle" and last is not None and m.get("requests") == last:
            if since is None:
                since = time.time()
            elif time.time() - since >= stable_s:
                return m
        else:
            since = None
        last = m.get("requests")
        time.sleep(0.4)
    return m


def main():
    started = time.strftime("%Y-%m-%dT%H:%M:%S%z")
    rep = {"started": started, "duration_min_target": DURATION_MIN, "sample_s": SAMPLE_S,
           "traffic": {}, "samples": [], "errors": []}
    if not start_pair():
        rep["errors"].append("collector/app pair did not come up")
        json.dump(rep, open(os.path.join(ART, "soak.json"), "w"), indent=1)
        print("PAIR_FAILED")
        return 1
    print("pair up", flush=True)
    rep["pids"] = {"app": app_pid(), "collector": coll_pid()}
    rep["engine_at_start"] = {k: metrics().get(k) for k in ("requests", "state")}
    rep["dbstat_start"] = dbstat()
    rep["db_counts_start"] = db_counts()

    tr = Traffic()
    tr.start()
    rep["independent_client"] = start_ext()

    sample_f = open(SAMPLE_PATH, "w", buffering=1)
    t_end = time.time() + DURATION_MIN * 60
    prev = None
    n = 0
    try:
        while time.time() < t_end and not STOP.is_set():
            s = {"ts": time.time(), "elapsed_min": round((time.time() - (t_end - DURATION_MIN * 60)) / 60.0, 2)}
            try:
                ap, cpp = app_pid(), coll_pid()
                s["app"] = proc_stat(ap) if ap else {}
                s["coll"] = proc_stat(cpp) if cpp else {}
                st = stats()
                s["frames"] = st.get("frames")
                s["frame_ms"] = st.get("frame_ms", {})
                s["residency_painted"] = (st.get("residency") or {}).get("painted")
                tstat = st.get("status") or {}
                s["queue"] = {"depth": tstat.get("our_sub", {}).get("depth"),
                              "max_depth": tstat.get("our_sub", {}).get("max_depth"),
                              "drops_total": tstat.get("drops_total"),
                              "sub_dropped": tstat.get("our_sub", {}).get("dropped_droppable"),
                              "forced": tstat.get("our_sub", {}).get("forced_drops"),
                              "trace_offset": tstat.get("trace_offset"),
                              "events_total": tstat.get("events_total_collector")}
                s["records"] = st.get("records")
                if prev and prev.get("frame_ms") and s["frame_ms"]:
                    s["p95_interval_ms"], s["p95_samples"] = p95_between(
                        prev["frame_ms"].get("hist", []), s["frame_ms"].get("hist", []))
                m = metrics()
                s["engine"] = {"requests": m.get("requests"), "state": m.get("state"),
                               "tok_s": m.get("live", {}).get("tok_s")}
                if n % 4 == 0:
                    s["db"] = db_counts()
                rep["samples"].append(s)
                sample_f.write(json.dumps(s) + "\n")
                prev = s
            except Exception as e:  # noqa: BLE001
                rep["errors"].append({"ts": time.time(), "error": str(e)[:200]})
            n += 1
            time.sleep(SAMPLE_S)
    finally:
        STOP.set()
        sample_f.close()

    rep["finished"] = time.strftime("%Y-%m-%dT%H:%M:%S%z")
    rep["own_gens"] = tr.sent
    rep["own_ok"] = sum(1 for g in tr.sent if g.get("ok"))
    rep["independent_ok"] = stop_ext()
    rep["engine_at_end"] = {k: metrics().get(k) for k in ("requests", "state")}
    rep["dbstat_end"] = dbstat()
    rep["db_counts_end"] = db_counts()
    rep["db_counts_mid"] = None

    # ---- post-soak budgets on the SAME long-lived process (rain on)
    ap = app_pid()
    s0 = stats()
    rep["budget_post_soak"] = {"pid": ap, "uptime_s": (s0 or {}).get("uptime_s"),
                               "rss_kib": proc_stat(ap)["rss_kib"] if ap else None,
                               "fds": proc_stat(ap)["fds"] if ap else None,
                               "frame_ms": (s0 or {}).get("frame_ms")}
    idle = wait_idle(3.0, 180)
    rep["budget_post_soak"]["engine_idle_requests"] = idle.get("requests")
    rep["budget_post_soak"]["cpu_pct_60s_rain_on"] = cpu_window(ap, 60) if ap else None
    vs = []
    for _ in range(60):
        v = app_vram(ap) if ap else None
        if v is not None:
            vs.append(v)
        time.sleep(0.5)
    rep["budget_post_soak"]["app_vram_peak_mib"] = max(vs) if vs else None
    s1 = stats()
    rep["budget_post_soak"]["frames_during_cpu_window"] = (s1.get("frames", 0) - s0.get("frames", 0))
    rep["budget_post_soak"]["frame_ms_after"] = s1.get("frame_ms")

    # ---- rain-off idle budget (fresh instance, no collector: strict idle)
    ssh("pkill -x matrix-app; pkill -x matrix-collect; sleep 0.5; true")
    out(f"cd {R} && {ENV} python3 bin/launch.py run/s7soak-app-off.pid logs/s7soak-app-off.log ./bin/matrix-app "
        f"--socket {SOCK} --stats {R}/run/s7soak-off-stats.json --rain off --banner on --trails on "
        f"--view 0 --monitor {MONITOR}")
    t0 = time.time()
    while time.time() - t0 < 20:
        if out(f"test -f {R}/run/s7soak-off-stats.json && echo up") == "up":
            break
        time.sleep(0.2)
    time.sleep(2)
    off0 = json.loads(out(f"cat {R}/run/s7soak-off-stats.json"))
    pid_off = app_pid()
    time.sleep(18)  # let the connection-refused retry settle
    off1 = json.loads(out(f"cat {R}/run/s7soak-off-stats.json"))
    rep["budget_post_soak"]["cpu_pct_60s_rain_off"] = cpu_window(pid_off, 60)
    off2 = json.loads(out(f"cat {R}/run/s7soak-off-stats.json"))
    rep["budget_post_soak"]["rain_off"] = {
        "frames": off2["frames"] - off1["frames"],
        "frames_spurious": off2.get("frames_spurious", 0) - off1.get("frames_spurious", 0),
        "frames_anim": off2.get("frames_anim", 0) - off1.get("frames_anim", 0),
        "connected": off2.get("connected"),
    }
    rep["pids_after"] = {"app_off": pid_off}
    restore = restore_live_pair()
    rep["restored_pair"] = restore
    json.dump(rep, open(os.path.join(ART, "soak.json"), "w"), indent=1)
    print("SOAK_DONE", json.dumps({"own_ok": rep["own_ok"],
                                   "samples": len(rep["samples"]),
                                   "errors": len(rep["errors"])}))
    return 0


if __name__ == "__main__":
    sys.exit(main())
