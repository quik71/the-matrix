# THE MATRIX — Stage 7 parity audit vs the legacy Python dashboard

Audit date 2026-10-31. Legacy subject: `~/moe-explorer` on host `main`
(`moe_telemetry/`, 1,905 lines across 11 modules, plus `static/js/{core,app,matrix}.js`
and `static/index.html`), read in full; no legacy process was started or
modified for this audit. Rust subject: `crates/matrix-collect` (event v1 +
SQLite) and `crates/matrix-app` (native viewer), HEAD `c820711`.

Tiers used below:

* **covered** — the Rust stack produces the same information from the same
  source, with an assertion or artifact behind it;
* **estimate-tier** — the information exists only as a labelled derivation
  (never live truth) and both stacks say so;
* **unavailable** — neither stack can produce it from the current engine; the
  app names the gap instead of inventing a value.

## 1. Data-surface table

| # | Data surface | Legacy Python dashboard | Rust collector + app | Tier | Evidence |
|---|---|---|---|---|---|
| 1 | Per-token, per-layer expert ids (top-k) | live, from `routing-trace.bin`, per position; `expert_selected` carries `token_index`, `layer_id`, `expert_ids`, `attribution` | live, from the same trace; `pass_routed` (position counts) + `expert_selected` (`pass_index`, `position`, `layer`, `k`, ids, weights, byte offset) | covered | S3-A2: `records == decoded_records == 48 × positions` per session and per path; S6-A4: per-session counters exact |
| 2 | Routing weights/scores | present in the record layout but all `0.0` on the serve path | same: stored as observed, app labels scores `unavailable` | unavailable | both stacks: "serve-path weights are all 0.0"; app `routing_scores` field + `unavailable[]` entry |
| 3 | Token position / index | `token_index` per position; for foreign traffic it is an **estimated** position budget (`external_ordered`) | `position` derived exactly from pass structure (`idx / 48`), never estimated | covered (better: no estimate) | S3-A2 `positions_sum == decoded positions`; collector's `pass_routed.positions_count` |
| 4 | Session attribution to a client | 5 tiers: `proxy_exact`, `proxy_inflight`, `external_window`, `external_ordered` (estimate), `unattributed` | `windowed` / `unattributed`, bounded by the engine's own `totals.requests`, with `request_delta`, `requests_before/after`, `end_reason` and `unattributed_requests` | covered (different vocabulary, no estimated tiers) | S3-A2 sum-of-deltas vs engine delta; app `sessions_by_tier`, `foreign_sessions`, `unattributed_engine_requests` |
| 5 | Foreign-client history persisted | **only proxy sessions by default**: `keep_experts = origin == "proxy" or cfg.store_external_experts`, so the legacy DB usually has no expert rows for other clients | every session's routing is stored regardless of which client caused it | covered (asymmetry in the Rust stack's favour) | legacy `correlator.py` `keep_experts`; collector `routing` table has no origin filter |
| 6 | Per-expert residency (VRAM / RAM / SSD) | `GET /api/residency`: bitmap-RLE of the first `engine.expert_slots` ranked pairs of `expert-profile.bin`, explicitly labelled a derivation; SSD per cell does not exist | `residency_estimate` event (STRP profile prefix + engine `expert_slots`), painted as a tinted cell set, dump `residency.status:"estimate"`, `confirmed:false`; per-cell SSD count 0 with a reason | **estimate-tier (parity)** | legacy `residency.py` docstring; collector `resident_estimate` body; app dump `residency.{basis,slots,vram_cells,ram_cells,ssd_cells,painted}` |
| 7 | Aggregate cache counters (`hit_rate`, `file_mb`, `pcie_share` per request) | read from `/metrics.recent` rollups and shown in the session bar, whole-session panel and event feed | **not collected**: `perf_sample` carries totals + live `tok_s` only | unavailable in the Rust stream | collector `poll_metrics` body; app `perf` panel fields |
| 8 | GPU / PCIe hardware | `/metrics.hardware`: util, mem used/total, temp, power, `gpu_pcie_rx_mb/tx_mb`, `pcie_gen/width`, CPU, RAM, disk read/write | `hw_sample`: GPU name/util/mem/temp/power, `load1`, `mem_available_mib`, collector RSS; **no PCIe, no disk I/O** | covered for compute/memory; unavailable for PCIe/disk | app `hardware` panel; `hw_sample` body |
| 9 | Model / architecture facts | GGUF-derived: `block_count`, `expert_count`, `expert_used_count`, quant, shard weight bytes, plus `statvfs` | `meta.engine` block from `/metrics` (model, max_context, expert_slots, spec, kv, arena) + trace geometry (48 layers, k=10, expert range 0..511); GGUF weight bytes/quant not surfaced | covered for visualisation; unavailable for weight-size/quant facts | app `model` panel; `model_architecture_detail` and `model_residency_bytes` unavailable entries |
| 10 | Session history / stored replay | SQLite `sessions`, `positions`, `expert_activations` (proxy only), `metrics_snapshots`; HTTP `/api/sessions`, `/api/session/<id>/positions`, `/api/session/<id>/experts`; no replay UI | SQLite `sessions`, `passes`, `routing`, `perf`, `hw`, `status`, `gaps`, `events`; `--replay <session>` re-emits the exact NDJSON, byte-identical to the live slice; `--export <dir>` writes NDJSON archives + queryable summary | covered (Rust adds exact replay and export) | S3-A3 byte-equality; A2 fixture proof: replay sha256 identical before/after compaction; `--export` identity |
| 11 | Export | none (API rows only; no CSV/JSON download) | `--export DIR`: `events.ndjson` + per-class NDJSON + `summary.json` (counts, per-session, expert/layer histograms, perf/hw aggregates, gaps, identity) | covered (new capability) | `proof.json` `export_*`; `summary.json.identity` |
| 12 | Event feed / diagnostics | `/ws` feed (last 50, filterable), `/api/health`: trace counters, poll errors, bus clients, `dropped_for_slow_clients`, DB stats | `telemetry_status` (2 s): rates, subscribers with `depth`/`max_depth`/`sent`/`dropped_droppable`/`replay_truncated`, `queue_cap`, `drops_total`, `skipped_bytes_total`, `gap_bytes_total`, trace offset, DB health; app `stream` panel + footer | covered | S3-A5 backpressure: slow subscriber drops droppable only, counted; `stream` panel fields |
| 13 | Zero-hole accounting | never-wrong-but-silent: bytes are only delivered when a pass validates, but `skipped_bytes` double-books (reparsed span + resync skips) and holes produce **log lines only**; recovery is a re-read crawl | exact: chunked skip-and-scan, one `gap` event per skip with `byte_start/byte_end/bytes/records_unattributed`, `skipped_bytes_total` exact, resyncs counted | covered (the Stage-1 appendix asymmetry) | appendix below; S3-A7: 8,000 injected garbage bytes → `passed_db == expected`, `skipped_db == 8000`, gap reason `resync_skip` |
| 14 | Backpressure / drops | WebSocket bus drops for slow clients (`dropped_for_slow_clients`), no per-type policy | bounded queue (4,096), only `expert_selected` droppable at 3/4 full, per-subscriber and global counters, replay floor + `replay_truncated` | covered (stricter: per-type policy + replay accounting) | S3-A5, S3-A4b |
| 15 | TTFT / inter-token latency | unavailable (engine exposes no latency fields) | unavailable: `perf_sample` has `tok_s` and totals only; app names it in `unavailable[]` | unavailable (both) | app `ttft_and_inter_token_latency` entry |
| 16 | Token text | unavailable (no text in the trace or events) | unavailable, named | unavailable (both) | app `token_text` entry |
| 17 | Expert co-activation / trails | none (per-layer histogram and top experts only) | `co_activation_matrix` (pairs inside each real top-k set) and per-cell historical trails with real event timestamps | covered (new capability) | S6-A4 identities: `records × k`, `records × C(k,2)` |

## 2. The zero-hole asymmetry (Stage-1 appendix, re-checked)

The Stage-1 appendix audited the legacy tailer (`moe_telemetry/trace.py`) and
concluded: emitted data **PASS**, survival **PASS**, accounting **FAIL
(bounded, cosmetic)**. Re-checked against the Rust collector for this audit:

| Property | Legacy `trace.py` | Rust collector | Note |
|---|---|---|---|
| Invalid header handling | `_need_replay`, clears `_cur`/`_carry` before re-read — no duplicated records | replay-from-anchor once, then chunked forward scan; no duplicate records | equivalent correctness |
| Pass validation | all 48 layers, equal per-layer counts; irregular passes counted and dropped | same validation; irregular passes counted into `discarded_passes` and emitted as `gap` when bytes are skipped | equivalent |
| Recovery cost | re-read crawl, one 2 KiB resync hop at a time | chunked skip-and-scan with the hole's exact byte range | Rust is bounded |
| Skipped-byte accounting | **inflated**: line 212 adds `offset - anchor` for a span that is then reparsed, line 234 adds resync skips on top | exact `skipped_bytes_total`, per-hole `gap` with `bytes` and `records_unattributed` | the asymmetry |
| Hole visibility | log lines only, no queue event | `gap` event on the socket and in SQLite, replayed to late subscribers | the asymmetry |
| Silent loss | never (data is correct) | never (data is exact and counted) | both refuse to serve partial data |

Verdict unchanged: the legacy tailer was never wrong, only silent and
over-counting; the Rust collector is exact and explicit. This asymmetry is the
strongest single argument for the replacement, and it is proven by S3-A6 (crash
resume tiling) and S3-A7 (injected garbage) rather than asserted.

## 3. Replacement-readiness verdict

**Ready — with two named gaps, neither of which touches the core observability
claim.**

The Rust pair covers every surface the legacy dashboard used to *answer the
question the project exists for*: which experts fired, in which layer, for
which position, for which session, from which client, with exact byte
accounting, replay and export. On residency it matches the legacy estimate
tier exactly (same derivation, same label) and adds a per-cell rendering. On
foreign-client history it is strictly better (the legacy DB discarded foreign
expert rows by default).

Two surfaces remain legacy-only:

1. **Aggregate cache counters** (`hit_rate`, per-request `file_mb`,
   `pcie_share`). The engine exposes them in `/metrics.recent`; the Rust
   `perf_sample` does not carry them yet. Closing it is additive (three fields
   in `perf_sample` + three panel fields) and is the single highest-value
   follow-up.
2. **PCIe / disk hardware counters and GGUF weight-size facts.** Same story:
   available from `/metrics.hardware` and `stat`/`statvfs`, not surfaced.

Neither gap is needed to watch routing, sessions, residency (estimate) or
performance; both are named in the app's own `unavailable[]` list or in this
table rather than being silently missing. Nothing the legacy dashboard showed
is *wrong* in the Rust stack, and nothing the Rust stack shows is estimated
except the residency set, which is labelled.

**Plain answer: the Rust pair can replace the legacy dashboard today for
routing, sessions, history and diagnostics. It should not be called a full
replacement of the legacy hardware/cache strip until the two aggregate gaps
above are closed.**
