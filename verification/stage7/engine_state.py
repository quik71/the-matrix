#!/usr/bin/env python3
"""Snapshot the engine's identity (read-only) so Stage-7 A6 can prove it was
never restarted, reconfigured or written to during the stage.

Runs a read-only python snippet on main through ssh stdin (no shell splicing).

Fields: MainPID, NRestarts, ActiveEnterTimestamp, config mtime+sha256, and an
inventory of the engine tree as `path size mtime` lines with the trace/log
excluded (they are the engine's own outputs and change while it serves).

usage: engine_state.py <out.json>
"""
import json
import subprocess
import sys

REMOTE = r'''
import hashlib, json, os, subprocess
TREE = "/home/main/strata-0.2.4.2"
CONF = TREE + "/strata-iq3_s.json"
# fixed argv lists, no shell: nothing here is built from input
out = subprocess.run(["systemctl", "--user", "show", "strata-engine",
                      "-p", "MainPID", "-p", "NRestarts", "-p", "ActiveEnterTimestamp"],
                     capture_output=True, text=True).stdout
sysinfo = {}
for line in out.splitlines():
    if "=" in line:
        k, v = line.split("=", 1)
        sysinfo[k] = v
st = os.stat(CONF)
conf = {"mtime": int(st.st_mtime), "size": st.st_size,
        "sha256": hashlib.sha256(open(CONF, "rb").read()).hexdigest()}
lines = []
for root, dirs, files in os.walk(TREE):
    for f in files:
        p = os.path.join(root, f)
        if f == "routing-trace.bin" or f.endswith(".log"):
            continue
        try:
            s = os.stat(p)
        except OSError:
            continue
        lines.append("%s %d %d" % (p, s.st_size, int(s.st_mtime)))
lines.sort()
print(json.dumps({
    "systemctl": sysinfo,
    "config": conf,
    "inventory_n": len(lines),
    "inventory_sha256": hashlib.sha256("\n".join(lines).encode()).hexdigest(),
    "inventory_sample": lines[:3],
}))
'''


def main():
    p = subprocess.run(["ssh", "main", "python3 -"], input=REMOTE, text=True,
                       capture_output=True, timeout=300)
    if p.returncode != 0:
        print(json.dumps({"error": "ssh failed", "err": p.stderr[:300]}))
        return 1
    rec = json.loads(p.stdout.strip().splitlines()[-1])
    rec["captured_at"] = subprocess.run(["date", "-Is"], capture_output=True, text=True).stdout.strip()
    out = sys.argv[1] if len(sys.argv) > 1 else "engine-before.json"
    json.dump(rec, open(out, "w"), indent=1)
    print(json.dumps(rec, indent=1)[:900])
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
