# THE MATRIX — Operations Runbook

Collector (`matrix-collect`) + viewer (`matrix-app`) for the Strata MoE routing
trace, running as **systemd user units** on host `main` under the autologin
Hyprland session.

| item | value |
|---|---|
| prefix | `/home/main/the-matrix` |
| binaries | `~/the-matrix/bin/matrix-collect`, `~/the-matrix/bin/matrix-app` |
| units | `matrix-collect.service`, `matrix-app.service` (systemd **user** manager) |
| socket | `~/the-matrix/run/collector.sock` |
| app stats dump | `~/the-matrix/run/live-stats.json` |
| database | `~/the-matrix/data/telemetry.sqlite` (WAL) |
| trace (input) | `/home/main/strata-0.2.4.2/data/routing-trace.bin` |
| engine | `127.0.0.1:8080`, config `/home/main/strata-0.2.4.2/strata-iq3_s.json` |
| repo of record | blackie `/srv/storage/work/the-matrix` (zero remotes) |
| deploy scripts | repo `deploy/`, copied to `main:~/matrix-build/deploy/` |
| health check | `python3 ~/the-matrix/bin/health.py --prefix ~/the-matrix --wait 15` |

The engine (`strata-engine`, a user unit, shared with other work) is **never**
touched by any command in this runbook: no restart, no write under
`/home/main/strata-0.2.4.2`.

---

## Health

The app's `--stats` dump is the only end-to-end proof: it knows whether the
socket is connected, whether a `meta` event arrived over the wire, whether the
residency estimate was actually painted, and whether anything was dropped.

```bash
# verify: python3 /home/main/the-matrix/bin/health.py --prefix /home/main/the-matrix --wait 15
```

Healthy output is one JSON object with `"ok": true` and, for the live
deployment, `connected: true`, `meta_source: "socket"`, `residency_painted:
2675` (== `residency_slots`), `forced_ours: 0`, `frames > 0`.

Manual spot checks:

```bash
# verify: systemctl --user is-active matrix-collect.service matrix-app.service
# verify: systemctl --user is-enabled matrix-collect.service matrix-app.service
# verify: test -S /home/main/the-matrix/run/collector.sock && echo socket-ok
# verify: systemctl --user show matrix-app.service -p After -p WantedBy | tr '\n' ' '
```

`systemctl --user status matrix-app.service` shows the last ~10 journal lines;
`journalctl --user -u matrix-collect -f` follows the collector.

What "healthy" does **not** mean: `expert_residency` is an **estimate** derived
from the model structure (`expert-profile.bin` + `engine.expert_slots`), never
live engine truth — no per-expert residency truth exists in this engine (static
profile prefill, no eviction). The UI says so and the dump carries
`residency.confirmed: false`.

---

## Restart

Both units have `Restart=on-failure`, so a crash or `kill -9` is repaired
automatically in 2–3 s. Manual restarts, in this order (collector first — the
app reconnects on its own within 50 ms and replays the current session):

```bash
# verify: systemctl --user restart matrix-collect.service && sleep 2 && systemctl --user is-active matrix-collect.service
systemctl --user restart matrix-app.service
```

Stopping the pair (e.g. before running the acceptance suites, which own the
socket and database paths):

```bash
systemctl --user stop matrix-app.service matrix-collect.service
# ... work ...
systemctl --user start matrix-collect.service
systemctl --user start matrix-app.service   # collector must be up first
```

The app may also be started/stopped by hand while the unit is disabled; the two
must never run at the same time (two windows would both hold the socket and
double the GPU load).

---

## Restore the database

The database is **regenerable telemetry**: every row is a projection of the
routing trace plus the engine's own counters. There is no authoritative state
in it that is not reproducible by re-tailing the trace, so a restore is a
recovery convenience, not a disaster-recovery requirement.

Restore from a backup copy (stop the collector first so nobody writes):

```bash
systemctl --user stop matrix-collect.service
cp ~/the-matrix/data/telemetry.sqlite ~/the-matrix/data/telemetry.sqlite.bad
sqlite3 ~/the-matrix/data/telemetry.sqlite.bad "pragma integrity_check;"
# ok -> keep it; not ok -> take the newest good copy:
cp /path/to/backup/telemetry.sqlite ~/the-matrix/data/telemetry.sqlite
rm -f ~/the-matrix/data/telemetry.sqlite-wal ~/the-matrix/data/telemetry.sqlite-shm
systemctl --user start matrix-collect.service
```

**The documented snapshot command is `VACUUM INTO`.** It writes one consistent
online copy in a single read transaction and does not contend with the
collector's writes. Measured on the live database with the 1-day window armed
(Stage 8, 2026-11-01, collector running, 8,250 events):

```
$ sqlite3 ~/the-matrix/data/telemetry.sqlite "VACUUM INTO '/tmp/telemetry-snap.sqlite'"
0.01 s   ->  0.005 GB copy,  pragma integrity_check = ok,  event count identical
             at the watermark recorded before the copy (8,250 == 8,250)
```

The store is now a 1-day window (see "How big can the database get?"), so the
copy is the size of that window: a few MB when idle, up to the 20–35 GB/day
ceiling under sustained generation. The 2 s / ~2 GB figure this section used to
carry described the pre-Stage-8 unbounded store.

`sqlite3 .backup` is **not** the documented command here: against the hot WAL
database the backup API restarts page-by-page whenever the collector commits
(1–2 rows/s) and livelocks — measured in Stage 8 at 890 GB of reads on the same ~2 GB file before it was killed. Copying `.db` + `-wal` by hand is never
acceptable (torn WAL copies are how corrupt restores are born).

Snapshot, no downtime:

```bash
sqlite3 ~/the-matrix/data/telemetry.sqlite "VACUUM INTO '/tmp/telemetry-snap.sqlite'"
sqlite3 /tmp/telemetry-snap.sqlite "pragma integrity_check;"   # expect: ok
# verify: rm -f /tmp/s8-verify.sqlite && sqlite3 /home/main/the-matrix/data/telemetry.sqlite "VACUUM INTO '/tmp/s8-verify.sqlite'" && sqlite3 /tmp/s8-verify.sqlite "pragma integrity_check;" && rm -f /tmp/s8-verify.sqlite
```

```bash
# verify: sqlite3 /home/main/the-matrix/data/telemetry.sqlite "pragma integrity_check;"
```

Runtime knobs that bound the file: `--retention-days 30` (default; prunes
`hw_sample`/`telemetry_status`, never routing/passes/sessions), duplicate
`expert_selected` lines compacted on start when the routing projection provably
covers them, WAL checkpointed by SQLite. To reclaim space after a prune:
`sqlite3 telemetry.sqlite "vacuum;"` (needs ~2× the file size free).

---

## How big can the database get?

`--retention-days` is a **real ceiling**, not a hw/status-only prune. At startup
the collector deletes everything older than the window — `passes`, `sessions`,
`routing`/expert lines, `perf` samples and the emitted event lines — then
`VACUUM`s the freed pages back to the filesystem. The deployed unit runs with:

```
--retention-days 1
```

Only `meta`, `gap` and `residency_estimate` lines are kept beyond the window:
they are a few hundred bytes per collector run and they are the audit trail that
explains restarts and replay floors.

**Steady-state ceiling (arithmetic, not vibes):**

| duty | measured rate | 1-day ceiling |
|---|---|---|
| extreme (Stage-7 soak: 45 tok/s continuous) | 20.2 GB/day measured; 34.8 GB/day with line-cache compaction; 119 GB/day if nothing were compacted | **20–35 GB** |
| this store, measured in Stage 8 (1-day window, 8,250 events, 5.2 MB packed incl. routing) | ~630 B/event on disk, ~290 B per NDJSON event line (both measured) | at 3.3 M events/day: 1-2 GB |
| quiet day | a fraction of the above | < 1 GB |

**The hard bound is `--max-db-mb 25000` (25 GB):** if the 1-day window still
leaves the file over budget, the oldest sessions are evicted until it fits
(oldest-first, then VACUUM), and the app sees `retention_size` plus a
`retention_size_evict` gap event. So the designed ceiling is
`max(1-day volume, 25 GB)` — with 35 GB free that is a budget, not luck.
`db-rotate.sh` remains as a manual, archive-preserving option (run it before
the file passes ~8 GB, or run long generation sessions with `--no-persist`).

**Honesty consequences of a 1-day window:**

* the **replay floor moves**: after a restart, the collector replays only what
  is inside the window, so a client that reconnects more than a day later gets
  fewer replayed lines than before. `telemetry_status.retention` carries
  `days`, `cutoff_ts`, `floor_seq`, `oldest_ts`, `expired_events` and
  `replay_bounded_by_retention:true`, and a `gap` event with
  `reason:"retention_expiry"` is emitted whenever startup deleted rows, so the
  app can explain the shorter replay instead of looking broken.
* **NDJSON is the keep-longer escape hatch.** If a slice of history matters,
  export it before it ages out — the export is a plain directory of NDJSON plus
  `summary.json`, and it is the only form meant to outlive the window:

```bash
~/the-matrix/bin/matrix-collect --db ~/the-matrix/data/telemetry.sqlite --export /mnt/keep/matrix-$(date +%Y%m%d)
```

**Rotation (belt and braces):**

```bash
./deploy/db-rotate.sh --check                 # size, threshold, what it would do
./deploy/db-rotate.sh --prefix ~/the-matrix   # archive with VACUUM INTO + fresh DB
```

## Backup policy

| data | class | policy |
|---|---|---|
| `data/telemetry.sqlite` | **regenerable telemetry** | **on demand only.** It is a projection of the trace; there is no nightly backup and none is needed. Snapshot before an upgrade you might regret, before `--prune`, or when you want a forensic slice. |
| routing trace `.bin` | engine-owned, input | backed up with the engine's own policy (`strata-0.2.4.2/data`), not by this project. |
| `run/live-stats.json` | ephemeral | overwritten every second; never backed up. |
| `run/collector.sock`, `*.pid` | ephemeral | recreated on start; never backed up. |
| `etc/matrix-install.json`, `etc/deploy-log.jsonl` | config/audit | small; keep (they are what `uninstall.sh` reads). |
| binaries `bin/*` + `bin/prev/*` | deploy state | keep the last pair; that is the rollback set. |

Suggested on-demand snapshot before risky work:

```bash
systemctl --user stop matrix-collect.service
sqlite3 ~/the-matrix/data/telemetry.sqlite ".backup ~/the-matrix/data/backup-$(date +%Y%m%dT%H%M).sqlite"
systemctl --user start matrix-collect.service
```

(Both units are journald-only; there are no log files to back up.)

---

## Upgrade

Source of truth is the git tree on blackie. Deploy = rsync source → build on
`main` → staged install → atomic rename → unit restart → health gate.

```bash
# from the repo (or ~/matrix-build/deploy on main)
./deploy/deploy.sh --from blackie --src /srv/storage/work/the-matrix \
    --prefix ~/the-matrix --unit-prefix matrix --auto-rollback
```

`--from array` is the documented build-host form (same script, rsync from the
build host instead of blackie). The script:

1. rsyncs the tree into `~/the-matrix/build` (excludes `.git`, `target/`);
2. `cargo build --release`, warning count reported into `etc/deploy-build.log`;
3. installs to `bin/.stage/`, then **renames** into place (no missing-path window);
4. copies the replaced pair to `bin/prev/<UTC>-<sha8>/` with a manifest;
5. restarts the units, waits for the socket, runs `health.py` for 25 s;
6. on a failed gate with `--auto-rollback`, calls `rollback.sh` automatically.

It never writes `data/`. `etc/deploy-log.jsonl` records every deploy.

---

## Rollback

```bash
./deploy/rollback.sh --list
./deploy/rollback.sh                      # most recent previous pair
./deploy/rollback.sh --to ~/the-matrix/bin/prev/20261101T003841Z-867dd2e6
```

Restores both binaries from the chosen `prev/` directory **and the unit files
that pair was deployed under** (`prev/<stamp>/units/`, saved by `deploy.sh`),
because a unit can carry a flag an older binary does not parse — Stage 8 added
`--max-db-mb`, so a binaries-only rollback to a Stage-7 pair leaves the collector
unable to start. Then restarts collector → app and runs the same health gate.
Expected exit codes: `0` ok, `3` health gate failed, `5` no previous pair.

A pair older than the current stage may still fail the gate on purpose: the
Stage-8 gate requires the residency estimate to be painted, which a pre-Stage-7
app cannot do. Rollback inside a stage (the normal case: one release back) is
what this proves; crossing a stage boundary also needs the unit snapshot.

---

## Uninstall

```bash
# stop, disable, remove units; KEEP data (prints the path and its size)
./deploy/uninstall.sh --prefix ~/the-matrix --unit-prefix matrix

# purge (two locks: --purge and the exact confirmed data dir)
./deploy/uninstall.sh --prefix ~/the-matrix --unit-prefix matrix \
    --purge --confirm-data-dir ~/the-matrix/data
```

`--purge` refuses if the confirmed path is not exactly the data directory
recorded by `install.sh`, and refuses the **live** data directory unless
`--purge-live` is also given. `--remove-bin` additionally deletes `bin/`
(including the rollback set); it is off by default.

---

## Logs

Both units log to the journal only (`StandardOutput=journal`,
`SyslogIdentifier=matrix-collect|matrix-app`), so rotation and size are
journald's job:

* `~/the-matrix/logs/` is legacy (Stage 3–7 file logs) and not written by the
  units; it can be deleted.
* per-unit log rate limiting is **not available for user units** on systemd 262
  (`systemd.exec(5)`: *"only available for system services, not in per-user
  services"*; the old `[Service] RateLimit*` keys were removed). This was tested
  — a 3000-line burst from a user unit with `LogRateLimitBurst=20` was logged in
  full.
* the bound is therefore journald's size/file/rotation caps, installed with:

```bash
sudo ./deploy/journald-caps.sh install     # /etc/systemd/journald.conf.d/60-matrix-bounds.conf
```

| setting | value | meaning |
|---|---|---|
| `SystemMaxUse` | 512M | hard ceiling for all journals on this host |
| `SystemMaxFileSize` | 64M | no single journal file grows past this |
| `MaxFileSec` | 1day | daily rotation even when idle |
| `SystemKeepFree` | 4G | never consumes the last 4 GiB |
| `MaxRetentionSec` | 14day | age cap |

Measured at install: the journal was **733 MB** before the cap and dropped to
**480 MB** after journald was restarted with `SystemMaxUse=512M` (enforcement,
not just configuration). Rotation was exercised with
`sudo systemctl kill -s SIGUSR2 systemd-journald`. The pair's own ingest is
tiny (see `log-bounds.json`: bytes/line counts for both units over the measured
window) — the caps exist for the rest of the host as much as for this project.

```bash
# verify: journalctl --user -u matrix-collect.service -n 3 --no-pager >/dev/null && echo journal-ok
journalctl --user -u matrix-app -u matrix-collect --since "1 hour ago" --no-pager | tail -50
journalctl --user --disk-usage
```

`~/the-matrix/run/` holds the socket, the 1 s stats dump (fixed size,
overwritten) and no growing files.

---

## Units

```bash
systemctl --user cat matrix-collect.service matrix-app.service
systemctl --user show matrix-app.service -p After -p WantedBy -p Restart -p MainPID -p NRestarts
```

**Autostart path (why user units, not `exec-once`).** The session is started by
sddm autologin → `uwsm start Hyprland`, and uwsm imports the compositor
environment into the systemd user manager before activating
`graphical-session.target`:

```
$ systemctl --user show-environment | grep -E 'WAYLAND_DISPLAY|XDG_SESSION_TYPE'
WAYLAND_DISPLAY=wayland-1
XDG_SESSION_TYPE=wayland
```

so a user unit `WantedBy=graphical-session.target` inherits a working Wayland
environment (`XDG_RUNTIME_DIR`, `WAYLAND_DISPLAY`,
`HYPRLAND_INSTANCE_SIGNATURE`) and additionally gets a restart policy, a journal
stream, an enable/disable state and dependency ordering — none of which
Hyprland's `exec-once` provides (it starts a one-shot child with no supervision
and no way to inspect or restart it from systemd). `exec-once` was therefore
rejected on evidence, not taste.

The collector is `WantedBy=default.target` (it does not need the compositor and
must survive a compositor restart); the app is `WantedBy=graphical-session.target`,
`After=graphical-session.target matrix-collect.service`, `PartOf=graphical-session.target`.

**Pending-boot honesty:** enable state, ordering, environment inheritance and
three live stop/start cycles are proven without a reboot
(`units.json`). What is *not* proven is the login-session activation itself: no
reboot and no logout has been performed, so the first real evidence is the next
login — expected result is both units active with `matrix-app` painted on
`DP-1`. Check with the Health section above after the next login.

### Failure playbook

| symptom | first check | action |
|---|---|---|
| health `connected: false` | `systemctl --user status matrix-collect` | start/restart collector; app reconnects in ≤50 ms |
| `matrix-app` exited (window gone, unit inactive, status 0) | journal: `signal: STOP observed` | user pressed `esc`; `systemctl --user start matrix-app.service` |
| unit "start request repeated too quickly" | `systemctl --user show matrix-app -p NRestarts` | fix the cause, then `systemctl --user reset-failed matrix-app.service` |
| no window on `DP-1` | `placement` block in `live-stats.json` (`verified`) | `--monitor DP-1` is in ExecStart; check the monitor name with `hyprctl monitors` |
| app alive, `frames: 0`, rain off | expected with `--rain off` and no traffic (present-on-demand) | none |
| DB growing unexpectedly | `residency`/retention in `log-bounds.json` and `sqlite3 ... "pragma page_count"` | run with `--retention-days 30` (default); `--prune` at start; `vacuum` after |
