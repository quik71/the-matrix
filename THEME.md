# THE MATRIX — visual system (Stage 6)

This file records what the native app draws, the parameters it draws with, and
the line between decoration and telemetry. The master plan (§8, §9, §14) is the
authority; this is the implementation note that goes with the code.

## Palette (§8.1, ported from the legacy `matrix` theme)

| role | value | where |
|---|---|---|
| background | `#010402` | clear colour |
| panel surface | `#030f08` | every panel (`PANEL_BG`, opacity 1.0 so data stays legible) |
| panel edge | `#123a22` | 1 px borders |
| primary accent | `#4ade80` | titles, connected state, live bars |
| secondary accent | `#10b981` | historical trail bars, pulse rings |
| activation highlight | `#d6ffe2` | lit cell fill, banner text, bright border |
| lattice cell (off) | `#171f1d` | the 48 x 512 lattice at rest |
| alarm / warn | `#ff5c57` / `#ffbd42` | disconnects, gaps, unavailable states |

Typography is the single Noto Sans CJK face at 11 / 13 / 16 / 24 px
(`--font`), uppercase panel titles, thin borders, bloom rings for glow.

## Rain (§8.2)

* one stream per column, each with its own speed, length, start offset and
  mutation period (30–110 ms, independent per stream);
* every mutation re-rolls the leading glyph and 1–2 others, and the stream also
  switches **glyph family** on that tick — katakana → numbers → hex → latin →
  math → binary — so the characters transform while they fall instead of a fixed
  string sliding down;
* `L`, `l` and `#` are excluded from every family: the rain can never spell a
  routing label (`L07E123`, `#hex·t42`) by accident;
* the leading glyph is bright white-green, the tail fades; a redrawn wash tints
  the lattice footprint at a hard-capped alpha (0.14);
* a dim background pass draws the leading glyph of every other stream behind the
  panels, so the ambient layer also lives in the gutters;
* density, speed, glow, wash and idle-factor are the legacy knobs, exposed as
  `--rain-density`, `--rain-speed`, `--glow` and reported in the dump's `theme`
  block.

`--rain off` is the **ambient gate**: it silences the rain, the banner, the
activation bursts and the pulses, because that is what the strict idle rule
measures (a decorative animation that keeps presenting frames is still a frame).

## Banner (§8.4)

`FOLLOW THE WHITE RABBIT`, inside the lattice panel: reveal left-to-right out of
cycling glyphs (1.7 s), hold with a periodic 2-glyph glitch (2.6 s), dissolve
back into glyph noise (1.3 s), then a gap; the interval is configurable
(`--banner-interval`, default 26 s, key `b`, ctl `banner`). It is decorative: it
never depends on inference activity, and its phase/cycle count are in the dump.

## Activation and history (§7.3, §7.4, §8.3)

* a lamp is one `expert_selected` id: flash-in 80 ms → hold 260 ms → ease-out
  fade 1200 ms to a 0.32 floor → trail until 4200 ms, then gone;
* cells above 0.5 alpha get a 1 px bright border plus bloom (capped at 600
  bordered cells so a full-lattice flash cannot multiply the quad count);
* simultaneous activations never overwrite each other (one marker per cell, the
  alpha is a pure function of the cell's own `t0`);
* **historical trails** are a different colour and position: a thin emerald bar
  at the bottom of a cell, length = log of the cell's selection count, alpha =
  recency from the event timestamp. History can therefore never be mistaken for
  a live lamp. `t` toggles them; the dump reports cells-with-history, totals and
  the first/last event timestamps they came from;
* **activation-linked effects**: each real record spawns one rain burst (brighter
  glyphs within 96 px of the cell) and one pulse ring that travels outward and
  fades over 720 ms. Counters are `bursts_total` / `pulses_total`, incremented by
  the event path, never by the render loop.

## Co-activation (view 6)

One `expert_selected` event carries the whole top-k set for a layer/position, so
every pair inside it really co-occurred. The app counts pairs expert-major over
all layers and draws a matrix over the 40 hottest experts, plus a ranked pair
table. No weights exist in the trace (serve path writes 0.0), so the view is
labelled co-**selection**, not routing score.

## Zoom and pan

`+`/`-` zoom (0.5–12x, anchored on the lattice centre), `w/a/s/d` pan, `0`
resets. The viewport (zoom, pan, cell size, drawn-cell count, rect) is written
into every `--stats` dump *after* the renderer has laid it out, so the dump
reports geometry that was really used. Cells outside the panel are not built at
all — at 6x the drawn cell count drops from 24 576 to ~1 600 — and every grid
pass is scissored to the lattice panel.

## The honesty rule in practice (§14)

* decoration: rain, banner, bloom, burst sparks, pulse rings. Gated by the
  ambient flag, spawned only by a real record (sparks) or by the clock (rain,
  banner), never producing an activation;
* data: lamps, layer heat, trail bars, the co-activation matrix, per-panel
  numbers. All of it comes off the collector's v1 socket;
* the only input injection is the `--ctl` control file, labelled
  `INPUT SELF-TEST` in the dump and in the log. It injects the same commands the
  keyboard does (zoom/pan/view/trails/layer/rain/banner/motion) and cannot create
  telemetry;
* unavailable fields stay unavailable and say why: per-expert residency, routing
  scores, token text, TTFT, inter-token latency, prefill rate, NVMe metrics,
  historical playback (Stage 7).
