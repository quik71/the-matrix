# THE MATRIX

Two crates, one workspace:

```
crates/matrix-collect   Stage 3 collector: routing trace -> NDJSON v1 -> unix socket + SQLite
crates/matrix-app       Stage 4 native application: unix socket -> in-app state -> winit/wgpu window
```

```
routing-trace.bin ──tail──▶ pass assembly ──▶ NDJSON events v1 ──┬──▶ unix socket ──▶ matrix-app (wgpu)
                                                                 └──▶ SQLite WAL

$ ls crates
matrix-app  matrix-collect
```

Build (host `main`):

```
rsync -a --exclude .git --exclude target ./ main:~/matrix-build/
ssh main 'cd ~/matrix-build && cargo build --release'
# binaries land in target/release/{matrix-collect,matrix-app}
```

Runtime state lives only under `/home/main/the-matrix/{run,data,logs}`; the
binaries are installed in `/home/main/the-matrix/bin/`.

---

# matrix-collect (Stage 3)

Normalized MoE routing-event collector for the Strata engine. One binary, one
external dependency (`rusqlite`, bundled SQLite — required by the spec).

## Event schema v1

Every line is one JSON object with `v`, `seq` (monotonic per database, never
reused across restarts), `ts` (unix seconds) and `type`. Fields that the engine
does not expose are **absent**, never zero-filled.

| type | key fields |
|---|---|
| `meta` | `collector` (version, pid, run_id, mode, argv), `engine` (the engine's own `/metrics` engine block: model, max_context, version, spec, mtp_max, expert_slots, kv…), `engine_conf` (whitelisted config, never `api_key`), `trace` (path, layers, k, record layout, start offset, resume point), `transport`, `storage` |
| `session_started` | `session`, `requests_before`, `tier_hint`, `trace_byte_start`, `run_id`, `engine_state` |
| `session_finished` | `requests_before/after`, `request_delta`, `tier` (`windowed`\|`unattributed`), `end_reason` (`counter`\|`idle_timeout`\|`shutdown`\|`engine_counter_reset`), `passes`, `records`, `positions`, byte range, `first_seq`/`last_seq` |
| `pass_routed` | `pass_index`, `byte_start`, `byte_end`, `positions_count`, `records`, `layers`, `k`, `skipped_bytes_since_last`, `resyncs_since_last` |
| `expert_selected` | `pass_index`, `position`, `layer`, `k`, `expert_ids` (as observed), `weights` (as observed), `byte_offset`, `byte_len` |
| `perf_sample` | `engine_ok`, `totals` (requests, prompt/output tokens, drafts), `delta`, `live.state`, `live.tok_s` (from `/metrics`) |
| `hw_sample` | `gpu` (nvidia-smi: name, util, mem, temp, power), `cpu.load1`, `mem.available_mib`, `proc.rss_kib` |
| `telemetry_status` | `rates`, `subscribers`, `per_subscriber` (depth, max_depth, sent, `dropped_droppable`, `forced_drops`, `replayed`, `replay_truncated`), `queue_cap`, `drops_total`, `skipped_bytes_total`, `gap_bytes_total`, `db` health, `trace` offsets, `session_active`, `unattributed_requests` |
| `gap` | `reason`, `byte_start`, `byte_end`, `bytes`, `records_unattributed`, `detail` |

## Session windows

A window is bounded by the engine's own `totals.requests` counter (exactly one
increment per completed request; verified in Stage 2). A window opens on the
first pass with no window active, and closes when the counter delta is observed
**and** the trace has been quiet for `--quiet-ms` (600 ms default) so the tail
can catch up. Anything that cannot be attributed is `unattributed`, never
guessed. Requests served with no routing activity are counted in
`telemetry_status.unattributed_requests` and never become invented sessions.

## Transport

`run/collector.sock`, one thread per subscriber, bounded queue (`--queue-cap`,
default 4096):

* `telemetry_status`, `perf_sample`, `hw_sample`, `pass_routed`, `session_*`,
  `meta` and `gap` are **never dropped**;
* `expert_selected` is droppable once a queue is 3/4 full; the drop is counted
  per subscriber and reported in every `telemetry_status`.

A subscriber that connects mid-session first receives the last
`--replay-max` (5000) stored events of the current session, then live events.
The live floor is the last **replayed** seq, so an event is either replayed or
delivered live, never both and never silently skipped; anything the bound cut
off is reported as `replay_truncated`.

`meta` is emitted once, before any subscriber can be registered (the acceptor
starts right after `bind` and the replay window is per session), so a client
that attaches late never receives it. `matrix-app` handles that explicitly —
see "Meta" below — instead of inventing a model name.

## Storage

`telemetry.sqlite` (WAL, `synchronous=NORMAL`): `events` (canonical NDJSON line
per seq, session-scoped and ambient) plus normalized projections `sessions`,
`passes`, `routing`, `perf`, `hw`, `status`, `gaps`, plus `cursor` (resume
anchor) and `runs`. Event rows and projections are written in one transaction,
so a `kill -9` cannot leave a routing row without its event.

`--replay <session>` re-emits the stored session lines (ambient
`telemetry_status`/`hw_sample` excluded) and is byte-identical to the live
session slice.

`--retention-days N` (default 30) prunes `hw` and `status` only; `routing`,
`passes`, `sessions` and `events` are never pruned.

`--no-persist` runs without SQLite (transport still works, replay is off).

## Crash behaviour

The cursor is persisted in the same transaction as the pass rows and points at
the **first byte of the pass being assembled** (or the first unconsumed byte
when no pass is held). After a `kill -9` the next run resumes there, so the
in-flight pass is re-read and completed exactly once. A `gap` event with
`reason=collector_restart` records the resume point, sessions left open by the
crashed run are marked `end_reason=collector_crash`, `tier=unattributed`, and
the union of the pre-crash and post-crash sessions decodes to exactly the same
records as a clean read of the whole byte range.

Zero-hole resync: any unparsable bytes are skipped, counted
(`skipped_bytes`/`skipped_bytes_since_last`) and emitted as `gap`
(`reason=resync_skip`). Nothing is dropped silently.

## Run

```
matrix-collect --path /home/main/strata-0.2.4.2/data/routing-trace.bin \
               --socket /home/main/the-matrix/run/collector.sock \
               --db /home/main/the-matrix/data/telemetry.sqlite \
               --engine 127.0.0.1:8080 \
               --engine-conf /home/main/strata-0.2.4.2/strata-iq3_s.json
```

The engine API key is read from `--engine-conf` at runtime and never logged,
printed or stored.

---

# matrix-app (Stage 4)

Native viewer. `winit` + `wgpu`, instanced quads — the same pipeline the
measured bench-A harness used (`wgpu 30.0.1`, `winit 0.30.13`, `ab_glyph
0.2.32`, `bytemuck 1.25.2`, `pollster 0.4.0`, pinned in
`crates/matrix-app/Cargo.toml`). **No browser, no webview, no HTTP client**: the
only input is the collector's unix socket, and the only external process it
ever starts is `hyprctl` (window placement) and, when explicitly asked,
`sqlite3` (see "Meta").

```
matrix-app --socket /home/main/the-matrix/run/collector.sock \
           --stats /home/main/the-matrix/run/app-stats.json \
           [--meta-db /home/main/the-matrix/data/telemetry.sqlite] \
           [--rain on|off] [--wm-fullscreen on|off] [--width N --height N]
```

Keys: `esc` quit, `r` toggle rain, `f` WM-fullscreen.

## What it draws

* header: title, model name, collector connection chip, telemetry-status chip;
* left panel: current session (id, tier, requests, passes, positions, records),
  engine sample (`perf_sample`), hardware sample (`hw_sample`), stream health
  (seq / gaps / drops / replay / skipped bytes);
* centre: the 48 × 512 routing lattice with the activation lamps driven by real
  `expert_selected` events (flash 80 / hold 220 / fade 1200 / floor 0.35 /
  history 8000 ms) and the Matrix rain;
* right panel: live lamp count, cumulative records, per-layer bars;
* footer: seq, last-event age, rain state, surface geometry, backend.

Panels that have no data say `unavailable`; surfaces not built at this stage
say `not implemented this stage` and name what is missing. The v1 stream has no
token text and no scores, so both are labelled unavailable rather than faked.
Expert *residency* is not in the v1 stream either, so the lattice is drawn as
geometry only (real 48 × 512 dimensions from `meta`) and no residency is
painted.

## Present-on-demand

With rain off the app runs `ControlFlow::Wait`: it redraws only when a collector
line arrives, when the connection state changes, or while a lamp is still
decaying (capped at 60 fps). With no collector and no events it redraws zero
times — `frames` in the stats dump is the proof.

## Meta

`meta` carries the model name and the trace geometry, and the collector emits
it once at startup, before a subscriber can be registered; its replay window is
per session. A client that attaches to a running collector therefore never sees
`meta`.

The app does not invent a model name. If no `meta` arrived, the header says
`MODEL unavailable — no meta event`, and with `--meta-db <collector.sqlite>` it
reads back the collector's own recorded `meta` line (read-only, one row, same
parser) and labels it `collector store` in the UI and `meta_source:
"collector-store"` in the dump. `meta_source: "socket"` means it came over the
wire.

## Stats dump

`--stats <path|->` writes a JSON counter dump every `--stats-ms` (default
1000 ms) from a dedicated thread, so the window can stay idle. It contains
`events_by_type`, `passes`, `positions`, `records`, per-session counters and
their `request_delta`, `gaps_seen`/`gap_reasons`, `drops` (ours and
collector-wide), `replay_seen`/`replay_truncated_seen`, seq totals and gaps,
the latest `perf`/`hw`/`status` samples, per-layer activation totals, the
hottest experts, and `frames`/`redraw_requests`.

## Connection lifecycle

The socket reader reconnects every 50 ms while the collector is down, reports
the disconnect the moment the socket closes (`kill -9` included), and on
reconnect consumes the collector's replay preload. Replayed events whose seq is
already counted are ignored (`events_ignored_dup`) instead of double counted; a
seq jump is counted as `seq_gaps`/`seq_missing` and the collector's own
`replay_truncated` for our subscriber is surfaced verbatim. A collector
restarted against a fresh database re-bases `seq`; the app detects that from the
`run_id` in `meta` or from fresh timestamps (`seq_rebaselines`) rather than
mistaking the new stream for duplicates.
