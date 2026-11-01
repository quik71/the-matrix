# Stage-8 harness notes: every failure root-caused (no retries without a diagnosis)

Stage 8 is operational hardening: systemd user units, failure recovery,
upgrade/rollback/uninstall, bounded logs and state, and an ops runbook. The
suites this stage re-runs (S3..S6 from their own harnesses, S7 for the data
lifecycle) are the *same* suites the earlier stages used, copied from
`/srv/storage/minion-verification/matrix-stage7/artifacts` by
`run_tests8.py a1`. Every `ok:false` seen in this session is root-caused here,
with the measured numbers that distinguish assertion drift from a real
regression.

Two things are true before any of the details:

* **one real regression was found and fixed in the product**
  (`--retention-days 0` wiped the store — §2.3);
* **one check is blocked by the machine's console state, not by the code**
  (S8-A1's S4/S5/S6 visual leg — §3). It is reported as blocked, not retried
  into a green.

## 1. The six harness bugs carried in from the predecessor's list

Each was verified against the code and the artifacts before being fixed; none
was "fixed" by loosening a threshold.

| # | bug | what it did | fix | evidence |
|---|---|---|---|---|
| 1 | `health.py --wait` returned on a *fresh* dump instead of a *healthy* one | after a restart the app writes its first stats line in ~1 s, before the socket connect completes, so a healthy pair was reported `connected:false` | `--wait` now polls the full predicate (connected, meta over socket, residency painted, no forced drops, no uncounted drops, frames, fresh) until it holds or the deadline expires | the docstring carries the measurement; deploy/rollback/install gates use it |
| 2 | maintenance JSON parsed with `json.loads(last_line)` | the `--prune` output embeds real newlines inside its strings, so the last line is never a JSON document → every parse raised | parse the whole stdout | `expiry_fixture.py` and the size-cap fixture both parse `r.stdout` and return `prune`/`size_cap` objects |
| 3 | `prev/manifest.json` opened with a local `open()` | `prev` is a path **on main**; the local open raised `FileNotFoundError` and the rollback assertion could never be evaluated | read it over ssh (`cat`) | `deploy.json` → `rollback_manifest` |
| 4 | A4's sha comparison | it asserted the re-deployed pair equalled `sha0` recorded before the upgrade, i.e. assumed a rebuild in `$PREFIX/build` is byte-identical to `~/matrix-build`. Measured today: **the builds ARE byte-identical** (reproducible), so "sha changed" was false for the opposite reason the predecessor assumed — and `sha256sum matrix-*` in the build dir also matched `matrix-app.d`/`matrix-collect.d`, which made `installed == built` permanently false | compare installed-vs-just-built **by binary name**, and perform a real upgrade (previous compatible release → committed build) so "changed" means something | §2.2; `deploy.json` → `deploy_pair.named_equal` |
| 5 | A2/A3 races | (a) `reconnects_with_replay` was read from the app's stats dump before the collector's 2 s per-subscriber snapshot, so it still held the pre-kill value; (b) `systemctl start` on a unit that a previous `kill -9` left in `failed` state was refused | wait for the counter to move; `reset-failed` before start | §2.4; `recovery.json` |
| 6 | A5's measurement defects | (a) the journald-caps and disk-usage regexes were double-escaped in raw strings (`\\S+`, `\\d`), so `caps` was always `{}` and `under_cap` was computed from nothing; (b) per-unit log volume was measured from the session stamp (minutes) instead of a fixed window; (c) the rate-limit probe had no unique `SyslogIdentifier`, so it counted lines from *previous* probe runs | single escapes; a fixed 3 h window that is a superset of the session; a unique identifier per probe run; and the man-page sentence is recorded as the reason rate limiting cannot apply to user units | `log-bounds.json` → `caps`, `window_h`, `rate_limit_probe` |

## 2. Root causes found in this session

### 2.1 S8-A1's S7 leg never ran, and the "green" was pre-seeded

`run_tests8.py a1` launched the S7 suite as
`ssh(f"cd {s7} && STAGE7_ART={s7} python3 run_tests7.py all")`, where `{s7}` is
a path on **array**. On main that `cd` fails, so the suite exited 255 — and
`results7.json` had just been filled with a copy of the **S6** result file, so
`len(checks) >= 8 and no failures` was true and the packet would have claimed a
re-execution that never happened.

Fixed: the suite runs here (`subprocess` with `cwd=s7` and
`STAGE7_ART=s7`), `results7.json` is removed first, the `VAULT_ARTIFACT` the
Stage-7 run used is passed in (S7-A4 asserts that file exists), and the check
requires all eight `S7-A*` names, not just a count.

### 2.2 S8-A4: a rollback that cannot start the pair it restored

`prev/` held the Stage-5 pair (`867dd2e6`). The Stage-8 unit's `ExecStart`
carries `--max-db-mb 25000`, and that collector does not parse it (measured:
`matrix-collect --help | grep -c max-db-mb` → 0 on the old binary), so
"rollback succeeded" meant "the collector is now dead". That is a real
deployment bug, not a harness detail: the unit file is part of the deployment.

Fixes:

* `deploy.sh` snapshots the unit files that were in force for the pair it
  replaces into `prev/<stamp>/units/` and records `units_saved` in the manifest;
* `rollback.sh` restores them (with `daemon-reload`) before restarting and
  reports `units_restored`;
* the harness performs the rollback an operator actually faces: it installs the
  previous **compatible** release (checked by asking the binary whether it
  parses `--max-db-mb`), upgrades to the committed build, then rolls back to the
  pair that was just replaced — binaries **and** units;
* the throwaway target starts from that same compatible pair, because starting
  it from the Stage-5 pair proves only that an old binary cannot run a new unit.

Measured (final run): upgrade rc 0, health ok, installed == just-built,
rollback rc 0, restored shas == manifest, units restored
`[matrix-app.service, matrix-collect.service]`, normalised to the reference
build, throwaway upgrade changed the sha, rollback restored it, `db-rotate`
archived 7.3 MB and brought the unit back, wrong purge confirmation refused,
exact purge removed the data, live DB same inode and same data-dir listing.

### 2.3 `--retention-days 0` wiped the store (real regression, found by S3-A7)

S3-A7 restarts the collector with `--retention-days 0` and asserts its routing
and passes survive ("retention prunes hw/status only, never routing"). With the
new real ceiling, `days = 0` meant `cutoff = now`, so **every** routing row,
pass, session and event line was deleted at startup. Failing on all three
attempts with the retention log line showing it:

```
{"days":0,"routing_pruned":1200,"passes_pruned":8,"events_pruned":1221,...}
before {r:1200,p:8,e:1225,h:1,s:3} -> after {r:0,p:0,e:24,h:10,s:2}
```

That is a footgun in the product (0 reads as "no window" to any operator), so
the fix is in the collector, not in S3: `days <= 0` **disables** the time
window, the size cap remains the only bound, and `telemetry_status.retention`
says so (`replay_bounded_by_retention:false` plus a note) instead of claiming a
0-day window.

After that fix S3-A7 still failed, on a different conjunct: `"hw_pruned" in
log` — the startup retention line was only printed when something had been
pruned, so a disabled window produced no line at all. The collector now always
prints its retention pass (counts, cutoff, freelist, vacuum, bytes), which is
the same honesty as the status block, one line per start. S3 then went 10/10 on
attempt 1.

The S3-A7 assertion itself was left exactly as it was.

### 2.4 S8-A2 read `/proc/<MainPID>/environ` of a stopped unit

`env_inherited:false` with every other conjunct green (enabled, ordering,
exec, graphical-session.target active, dep edge, three healthy cycles, painted
2675). Cause: A2 was run after a suite that left the units stopped, so
`MainPID` was `0`, `/proc/0/environ` was empty, and the check measured a stopped
process. Fixed: A2 now ensures a healthy pair (starting it in the documented
order if needed) before reading the pid, and records `health_at_start`.
Measured after the fix: `env_inherited:true`, 3/3 cycles healthy.

### 2.5 S8-A5's expiry fixture asserted `sessions == 0`

The policy keeps an **unfinished** session ("an active window is never
expired"). The live store had exactly one at copy time, so a correct prune left
one session and the fixture called it a failure. Fixed: the fixture records
`open_sessions_before` from the copy and the check requires
`sessions_after == open_sessions_before`, plus `routing == 0`, `passes == 0`,
integrity ok, size halved, vacuumed, and a non-null replay floor. Measured:
293 562 events / 10 sessions → 51 events / 0 sessions, 181.4 MB → 0.44 MB.

Also in A5: `run/` collects stats dumps from directly-started test apps. A
stray `matrix-app` from an earlier suite kept rewriting `run/s4-stats2.json`
right after the cleanup, so the "only the live files remain" assertion raced a
writer. A5 now stops non-unit processes from `<prefix>/bin` first (recorded in
`leftovers_stopped`), then cleans, then waits for the directory to settle.

### 2.6 S8-A6's `per_event > 0` was vacuous — restated from measurement

`per_event > 0` is satisfied by any non-empty store. The runbook now states two
measured ratios and the check compares the live measurement against them:

| quantity | documented | measured (final run) |
|---|---|---|
| on disk (`VACUUM INTO` copy / events) | ~630 B/event | 618 B/event |
| NDJSON (`events.ndjson` bytes / lines) | ~290 B per event line | 293 B per event line |

Both must match within ±50 %, and both are measured in the run (the export is
made with the deployed collector against the live store). The runbook's snapshot
figure was also re-measured: `0.19 s -> 0.18 GB` at 293 584 events, replacing
`2 s -> ~2 GB`, which described the pre-Stage-8 unbounded store. The
"20.2 GB/day" extreme-duty rate and the `20–35 GB` one-day ceiling are unchanged
because their measurement has not changed.

## 3. What is blocked, and why (S8-A1)

The graphical session on main is at a **real password lock**:

```
$ omarchy-shell lock status
{"locked":true,"requested":true,"pending":false,"sessionLocked":true,"secure":true,
 "realScreens":2,"passwordPam":true,...}
$ grim -o DP-1 /tmp/lock-probe2.png   # shows the "Enter Password" prompt
green_frac of that 1920x1080 capture: 0.000
```

The Stage-7 note that the lock is "a fullscreen ghostty screenshot that exits on
SIGTERM" no longer holds: no `omarchy-screensaver` or `hyprlock` process exists;
the lock is an `ext-session-lock` surface held by quickshell (`pid 3356700`),
and `omarchy-shell lock` has no unlock IPC (unlock is PAM-authenticated by
design). **It was not bypassed**: a verification run does not release a locked
session, and no password was available or sought.

Consequences, measured:

* `S4-A5` fails on its `not locked` conjunct — and on nothing else. The window
  it rejected was valid on its own terms: `cpu_pct 3.826`, `60.08 fps`,
  `3612 frames / 60.12 s` with rain on, `cpu_pct 0.383`, `0 frames`,
  `frames_spurious 0` with rain off. The guard exists because a locked session
  *can* withhold presents; here it demonstrably did not, but the guard is a
  Stage-4 decision and was left as it is.
* `S5` and `S6` screenshot checks capture the lock surface, so their pixel
  census has nothing to measure (green_frac 0.000 on the same monitor). They
  cannot pass while the console is locked, and the chain stops at the first
  non-green suite, so they were not reached.

What was proven before the block, with the same binaries that are deployed:

* `S3` **10/10 on attempt 1** (`chain-blocked/s3-results.json`), including the
  crash/gap-tiling check and the read-only-trace proof;
* `S4` **9/10** (`chain-blocked/s4-results.json`): the only failure is
  `S4-A5`/`not locked` above; `S4-A1` (documented-deleting allow-list) is green
  after registering this stage's three collector commits by subject, with their
  justification, exactly as the mechanism intends — the guard itself is
  unchanged.
* the zero-warning rebuild (`build.log`: `warnings=0 errors=0`).

Environment held off for the retry (so that a future run does not lose 20
minutes to the idle action): `~/.local/state/omarchy/toggles/screensaver-off`
and `~/.local/state/omarchy/indicators/stay-awake`. They do not release an
existing lock.

**Resume path (needs a human at the console):** unlock main, then

```bash
/srv/storage/minion-verification/matrix-stage8/run_a1.sh   # writes logs/a1.rc
cd /srv/storage/work/the-matrix/verification/stage8 && python3 run_tests8.py a7
python3 make_packet.py && python3 run_tests8.py a8
```

`run_a1.sh` clears the partial chain artifacts, runs the build plus S3..S6 plus
S7, and writes an exit sentinel; nothing in this stage's code or data needs to
change for it to go green once the session is unlocked.

## 4. A1..A8 as measured in this session

| id | state | evidence |
|---|---|---|
| S8-A1 | **blocked** (console lock) | S3 10/10 `chain-blocked/s3-results.json`; S4 9/10 `chain-blocked/s4-results.json` (only `S4-A5`/`not locked`); build `warnings=0 errors=0`; S7 re-executed locally by the fixed leg (§2.1) — not reachable while S4 is red |
| S8-A2 | PASS | `units.json`: enabled/enabled, After/Wants/PartOf as specified, exact ExecStart, `env_inherited:true`, `graphical-session.target` active, 3/3 cycles healthy, painted `[2675,2675,2675]`; boot activation honestly pending |
| S8-A3 | PASS | `recovery.json`: collector `kill -9` → unit restart 2.2 s, app reconnects with a new `run_id`, replay + `reconnects_with_replay` incremented; app `kill -9` → restart 3.3 s, meta over socket, painted 2675, `run_id` unchanged; fresh subscriber sees `meta` first then replay |
| S8-A4 | PASS | `deploy.json`: upgrade from the previous compatible release (rc 0, health, installed==just-built, sha changed), rollback to that pair (rc 0, shas == manifest, units restored, health), normalised to the reference build, throwaway install/upgrade/rollback/rotate/uninstall/purge (wrong confirmation refused), live DB same inode and same data-dir listing |
| S8-A5 | PASS | `log-bounds.json`: caps 512M/64M/KeepFree 4G/daily/14-day applied, `under_cap:true`, rotation observed (38→40 files), unit volume 22.7 MB/day over a fixed 3 h window, rate limiting reported unsupported for user units with the man-page sentence, expiry 293 562 → 51 events / 181.4 MB → 0.44 MB with integrity ok and a replay floor, size cap evicted 240 731 events until it fit, export identity held |
| S8-A6 | PASS | `runbook.json`: repo/host/vault present, 10 sections, snapshot `0.19 s -> 0.18 GB` at 293 584 events (identical counts at the watermark), restore round-trip ok, 9 documented commands all rc 0, arithmetic `618 B/event` vs `~630` and `293 B/line` vs `~290`, `db-rotate --check` reports honestly |
| S8-A7 | PASS | `engine-fingerprint.json`: MainPID 996095, NRestarts 0, ActiveEnterTimestamp unchanged, config mtime 1793410092 + sha `62368d5e…`, inventory 7900 files + sha `145050f6…`; repo clean, zero remotes, deploy/ + verification/stage8 tracked |
| S8-A8 | blocked with A1 | `final-health.json`: live pair healthy at close (connected, meta over socket, painted 2675, forced 0); packet complete except the A1 claim |

## 5. Where the artifacts are

* `/srv/storage/minion-verification/matrix-stage8/artifacts/` — one file per
  check (`units.json`, `recovery.json`, `deploy.json`, `log-bounds.json`,
  `runbook.json`, `engine-fingerprint.json`, `final-health.json`,
  `results8.json`, `build.log`);
* `artifacts/chain-blocked/` — the chain results captured before the block
  (`s3-results.json`, `s4-results.json`) plus the snapshots of every complete
  S3/S4 attempt the watcher saw;
* `/srv/storage/minion-verification/matrix-stage8/logs/` — the per-suite logs
  (`a2.log`, `a4.log`, `a5.log`, `a6.log`, `chain-s3.log`, `chain-s4.log`,
  `a1.log`) and the runner logs with timestamps;
* `/srv/storage/minion-verification/matrix-stage8/packet.json` — the packet.
