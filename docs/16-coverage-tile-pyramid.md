# 16 — The coverage tile pyramid, and the full-spectrum-over-time view

**Status: design, not built.** Proposed by the user on 2026-09-16. §5.1 and §5.2 were decided by the
user on 2026-09-17; §5.3–§5.5 are settled here, inside that framework. This document exists so that
three features already in flight are built against one piece of infrastructure rather than three
private ones.

## 1. The user's ask

> A large plot: **Y axis = time, X axis = the whole spectrum** (device range), zoomable into portions
> of the dataset **Google-Maps-style with multiple zoom levels** (a tile pyramid). Occasional
> downsampling of the search space so the browser doesn't blow memory — hence multi-resolution tiles.
> Most of it is **grey = unobserved, and that grey is the point: you see WHERE you've scanned.**
> Observed regions are filled; **confirmed identified signals are highlighted.** As more SDR devices
> are supported, more of the spectrum fills in.

And the architectural instruction that makes this document necessary:

> The multi-resolution, coverage-aware **tile pyramid is one piece of backend infra** that serves
> **all** of: the big zoomable view, the bottom frequency-survey navigator bar, and the
> iterative-scan accumulation. **The bottom survey bar is a miniature of the big view.** Same coverage
> map, same max-hold downsampling, same grey = unobserved semantics.

## 2. What exists, and where the gap actually is

Most of it exists. The work is mostly *connecting* rather than building — but §2 read the gap too
generously in its first draft, and T-397 plus a reading of `hk-store` narrow it to three specific
things. Both corrections are below the table.

| Piece | Where it already is |
|---|---|
| Tiered, multi-resolution history | the spectrum-history pyramid (`hk-store`), tile format 6 (`history/codec.rs`) |
| **Fixed `(scheme, level, f_block, t_block)` tile addressing** | `hk_model::TileKey`; on disk `…/s<scheme>/L<level>/f<f_block>/t<t_block>.tile` — the address *is* the index |
| Coverage: observed vs never-observed | `hk_store::Coverage` (T-368), `Coverage::of` refuses to spell "never looked" as "quiet" |
| Per-(time, frequency) *measurement* coverage | `CellStats::coverage` per (time cell, frequency cell), backed by `Tile::obs_s` and the tile's observed bitmap |
| Max-hold fold with stated semantics | T-342's `grid.semantics` on `/api/timeline`, `shade` block on `/api/coverage` |
| A fold that already works on **both** axes | `RegionHistory::overview(window, freq, nt, nf)` — T-338/T-342, and T-397 confirmed it |
| Which device looked where | T-378 (observation log) + T-314/T-377 (the chain of custody) |
| A time-axis budget that never truncates the window | T-342's `resolution.budget`, sibling of `/api/floor`'s `max_steps` |
| Per-frequency-cell resolution tiers | `Resolution` / `resolution.source` (T-334): `live-iq` / `spectrum-history` / `survey-overview` |
| Byte-budgeted rolling eviction | `PyramidConfig::byte_budget` (8 GiB), `enforce_budget`, `RetentionOverride` (T-116/T-126) |

**Correction 1 — map tiling is not foreign to this repo; it is the store's native idiom.** The first
draft framed §5.1 as a choice between "map tiles" and "the per-axis budget idiom this repo already
uses". That is only true at the **API** layer. The **store** has always been a fixed tile pyramid:
`hk_model::TileKey` is `(scheme, level, f_block, t_block)`, `Geometry::parent(level, f_block, t_block)`
walks the ladder, and one file per tile makes the address the index. The budget idiom
(`/api/timeline`'s `columns`, `/api/floor`'s `max_steps`, `/api/history`'s `max_t`/`max_f`) is a
**projection** the API performs *over* those tiles. So the user's §5.1 decision does not import a
foreign idiom — it **exposes the one already underneath**, and the work is a route, not a re-modelling.

**Correction 2 — the gap is not "there is no second axis". It is that the two axes are welded
together at the wrong ratio.** The default ladder (`PyramidConfig::default`, scheme 1):

| Level | Frequency cell | Time cell | Tile | Ratio to the level below |
|---|---|---|---|---|
| 0 | 6.25 kHz | 1 s | 1024 × 60 cells = 6.4 MHz × 1 min | — |
| 1 | 12.5 kHz | 1 min | 1024 × 15 = 12.8 MHz × 15 min | f ×2, **t ×60** |
| 2 | 25 kHz | 15 min | 1024 × 4 = 25.6 MHz × 1 h | f ×2, **t ×15** |
| 3 | 50 kHz | 1 h | 1024 × 24 = 51.2 MHz × 1 day | f ×2, **t ×4** |
| 4 | 100 kHz | 1 day | 1024 × 7 = 102.4 MHz × 1 week | f ×2, **t ×24** |

Across the whole ladder frequency coarsens **×16** and time coarsens **×86 400**. That is not a map
pyramid; it is a ladder tuned for a different question — *one band over a long time*, which is what
`/api/history` and the coverage report ask. The big view asks the opposite: *the whole spectrum, at a
time resolution you can see*. No level in scheme 1 answers it. To draw 6 GHz × 30 days on a
2048 × 1024 canvas you need roughly 3 MHz × 45 min cells; level 3 is 60× too fine in frequency and
level 4 is 30× too coarse in time. **There is no level with both.**

And the coupling is structural, not a configuration accident. `PyramidConfig::geometry` computes
`t_cell = prev.t_block_ns()` — *the next level's time cell is one whole tile of this level* — so the
time ratio between levels is forced to equal the finer level's `t_cells_per_block`. A square ladder
(both axes ×2) is expressible only by making tiles two rows tall, which defeats tiling. **`LevelConfig`
needs a `t_factor` independent of `t_cells_per_block`.** That one field is the second axis, made
buildable; §6 spends it.

**Correction 3 — there are two coverage answers, from different evidence, and neither alone is what
the big view needs.**

| Answer | Evidence | Granularity | Device | Horizon |
|---|---|---|---|---|
| `CellStats::coverage` (the tile's `obs_s`) | **measurement** — frames actually landed here | per (time cell, frequency cell) | no (only per-tile, via `Origin`/`ProvenanceSummary`) | the pyramid's byte budget |
| `hk_store::Coverage` (T-368) | **record** — the ring journal and the observation log say the radio was tuned here | per frequency cell **over one window** | **yes** (`by_device`, T-378) | ring retention (120 s) and observation log (180 days since T-406; was 30) |

A measurement is proof we looked. A record is proof we looked. They are not redundant: the tile knows
nothing about a dwell whose frames never reached the history, and the record has no time axis, so it
cannot correct a *cell*. **The second axis that actually pays is giving the record-derived answer a
time axis so the two can be composed per cell**, which is exactly what §3's survey-bar note was
circling. §6 states the composition.

## 3. The three consumers, and what each needs from one pyramid

1. **The big zoomable view (§1).** Tiles at several zoom levels over (time × frequency), grey where
   unobserved, confirmed emitters highlighted on top.
2. **The bottom frequency-survey bar** (T-397/T-405), and its sibling the **left time navigator**
   (T-411). Originally filed as "the same data at `rows = 1`". **T-397 raised the survey bar to R rows
   and shipped it**: `/api/timeline` now serves `rows × columns` through `RegionHistory::overview`, so
   *"the bar is a miniature of the big view"* is now **literally true at the data level**, not only
   visually — the same route, the same fold, the same scale, differing only in which axis is collapsed
   and how hard. **No second axis was needed for the fold**: `overview` has always taken `(nt, nf)` and
   folds exactly on both (T-338/T-342), so §2's gap was never about the fold.
   - **What the bars still cannot get** is per-(time, frequency) *coverage from the record*.
     `hk_store::Coverage` answers per frequency cell over a window, so it decides a **column**, while
     the history grid decides a **cell**. Each bar therefore draws a measured value wherever the grid
     holds one and falls back to the column's coverage state where it does not. That composition is
     honest but it is the place the second axis pays: a cell in an observed band that the radio was
     tuned away from *at that instant* should be grey, and today it reads as "sampled, level not
     retained".
3. **Iterative-scan accumulation** (T-406). A sweep retains what each step saw **into the pyramid**,
   so the big view and the bars fill in as it runs. No private accumulator. Its dwell retention is the
   **second consumer of §5.4's horizon decision**, and the one that feels it most — see §7 step 6.

If these are built separately they will disagree — and they will disagree *visibly*, because the user
will have the survey bar and the big view on one screen showing the same spectrum.

## 4. The invariants this must not break

These are not new; they are the ones the coverage and fold work already established, restated because
a pyramid is where they are easiest to lose.

- **Grey means genuinely unobserved.** `Coverage::of` refuses a zero span count, a non-positive
  sampled duration, or a non-positive window, and returns `Unobserved` rather than a zeroed `Sampled`.
  `Sampled` is `#[non_exhaustive]` so there is no second door. On the wire an unobserved cell carries
  **no measurement keys at all** — structurally absent, not null.
- **Max-hold must not manufacture observation.** T-397's rule, now the stated contract of the fold on
  both axes: *the max of nothing is unknown, not zero, and not the bottom of the scale.* A tile that
  pools observed and never-observed children is **partially covered** and must say so.
- **Folding never lowers a value.** That is what makes a peak survive downsampling — and it is
  precisely the property the user was missing (T-405: "yellow/red peaks not showing because
  averaging washes them out").
  - **Where that was actually broken, and it was not the fold** (T-397). Every fold on the way out
    *was* a max: `Tile::add_value`, `Tile::fold_child` (max-of-max), `OverviewCell::fold`,
    and both routes' `"fold": "max-hold"` declarations. What was averaged was the **input**:
    `hk-pipeline`'s history reader set `WelchConfig::holds = false`, so `Spectrum::max_hold` was
    empty, `FrameInput::from_dsp` set `peak: None`, and the store's
    `frame.peak.unwrap_or(frame.psd)` substituted the Welch-averaged PSD. A history frame averages
    `K ≈ fs / (hop · rows_per_s)` segments — order a thousand at 20 Msps — so a one-segment burst
    lost ~10·log10(K) ≈ 30 dB before the first max ran. **A wire that declares its fold is not
    evidence the values it folded were measurements**; `hk_pipeline::history::history_welch`'s test
    measures it end to end instead.
  - **A pyramid is where that mistake is easiest to repeat, because each level inherits its parent's
    claim.** Two guards, both cheap:
    1. **Prove a level against level 0, never against its parent.** A parent-vs-child test proves only
       that the fold is a max. The property that matters is that a coarse tile's value equals the max
       of the **finest available** cells in its box, computed independently. That catches a bad input
       at any depth; the parent-vs-child form catches it at none.
    2. **A tile carries the provenance of its input statistic, not just its fold.** Level 0 already
       knows whether a frame's value came from `frame.peak` (a measured max-hold) or from the
       `unwrap_or(frame.psd)` substitution. Carry that up the ladder and refuse to silently merge
       children whose input statistics differ — a level that pooled measured maxima with substituted
       averages must say so rather than declaring `"fold": "max-hold"` on its own authority.
- **A ratio is not a foldable statistic, and two live sites already fold one.** This is the T-397
  lesson transposed onto the coverage axis, and it is the finding that matters most for a view whose
  entire point is *"you see WHERE you've scanned"*.
  - **`Tile::fold_child` rounds coverage up.** Its rule is documented as *"coverage = the best-observed
    child frequency cell"* — literally `self.obs_s[i] = best_obs`, a **max across the `f_factor`
    child frequency cells**. So a parent cell whose left half was observed for the full minute and
    whose right half was never observed reads **fully covered**. Fit for its current purpose (a
    narrow-band query asking *is there history here*); **unfit for the big view**, where four folds at
    `f_factor = 2` let a level-4 cell claim full coverage on the evidence of one sixteenth of its
    frequency extent. A pyramid built on this rule paints the spectrum as scanned. The correct rule
    for a coverage plane is **sum `obs_s` over the children and divide by the parent's own extent** —
    `obs_s` is foldable, `duty` is not.
  - **`OverviewCell::fold` averages coverage over source cells, not over extent.** It accumulates
    `coverage +=` then divides by `sources`, which is exact only when every source cell has equal
    duration *and* lies wholly inside the output cell. The first holds (one level answers one query);
    the second does not, because `overview` lays a **fractional** grid on the requested window
    (`t_cell_ns = (t1 − t0)/nt`) and assigns a source cell to every output cell it overlaps, unweighted.
    A source cell overlapping an output cell by 1 % counts as much as one overlapping by 100 %.
    **Fixed map tiles fix this**, and it is a real point in the user's §5.1 favour: when the output
    grid is an exact coarsening of the source grid, the mean *is* over extent and the approximation
    disappears.
- **Device-local, never unioned by accident.** Coverage is a fact about **one front end** (T-259/T-305).
  `by_device` returns one grid per device; a union is a separate call returning `Device::Any` with
  `"named": false`, so it can never wear a radio's identity. **This matters more as SDRs are added,
  which is exactly when the view gets interesting.**
- **The scale must be stated.** T-342: a shade was a 0–1 number normalised against a range the response
  never named, so the same energy read as two strengths on one screen. Every tile carries its fold,
  its scale and its range.
- **Never imply resolution the front end did not capture.** A wide zoom is **survey-history overview**,
  not live IQ, and the view must distinguish them (T-334's `resolution.source`).

## 5. Design questions — all five now settled

**§5.1 and §5.2 were decided by the user on 2026-09-17.** §5.3–§5.5 are settled here, with reasons,
inside that framework; the user was explicit that they do not gate and do not need them.

### 5.1 Tile addressing — DECIDED: fixed `(zoom, t_index, f_index)` map tiles

Google-Maps-style, not the per-axis budget idiom for this view. (The budget idiom stays correct where
it already lives — `/api/timeline`'s `columns`, `/api/floor`'s `max_steps` — and T-397 showed the
*fold* composes on both axes already. This decision is about how the big view's **tiles are
addressed**, not how a fold is computed.)

**§2's correction 1 applies:** this is the store's existing addressing, promoted to the wire. The key
is `(scheme, level, f_block, t_block)` and it already exists as `hk_model::TileKey`.

### 5.2 Downsample timing — DECIDED: precomputed at seal time, on demand at the live edge

Precomputed at seal time for the coarse zoom levels, computed on request at the live/finest edge, with
**client-side tile eviction** and a **tiles-in-flight cap** for bounded browser memory. Storage cost
accepted (140 GB free at the time of the decision). This matches how the history pyramid already tiers,
so the coarse levels are a seal-time product like every other tier, and only the edge — where data is
still arriving and cannot be precomputed — is computed on request.

### 5.3 Confirmed-signal highlighting — SETTLED: the client overlays; a tile carries no emitters

**A tile carries measurement and coverage. It never carries emitters.** The client draws highlights
from a separate query — `GET /api/events`, which is already exactly this question — over the tile-snapped
viewport box, in the **big view's own time frame**, reading nothing from Explore's cursor.

Five reasons, in order of how hard they are to argue with:

1. **Identity gating is per-caller; a tile is not.** `/api/events` gates identities (`withheld`,
   `identity_access`) per request. A tile shared between callers and cached on disk cannot carry a
   gated field without either leaking it or being cached per token, which destroys the cache. That
   alone forbids emitters in tiles, before any other consideration.
2. **A tile is immutable once sealed; an emitter set never is.** The whole value of §5.2's
   precomputation is that a sealed tile's content can never change, so it is cacheable forever. The
   emitter set over the same box changes constantly: promote and delete (T-078), every append to the
   `emitter_observation` ledger, a revoked relationship claim, a re-classification, a merge. Baking
   emitters into tiles would invalidate tiles on every inventory write — spending the exact property
   the storage was bought for. **Put the mutable thing on the cheap query and keep the expensive thing
   immutable.**
3. **Whose frame: the view's own box, and T-386 settled why.** The big view is a **history** surface.
   T-386 kept the durable catalogue independent of the Explore view cursor with four reasons, three of
   which carry over verbatim: CLAUDE.md puts the durable record in a *separate* surface; workflow #3
   is *"choose a region and see what activity was seen there over time"*, so choosing the period **is**
   the function; and the Explore window is bounded by the IQ ring's 120 s retention while this view
   exists precisely for the periods beyond it, so following that cursor would make most of the record
   unreachable from the surface built to reach it. The highlight's frame is therefore the tile-snapped
   `(t0, t1, f_lo, f_hi)` of the viewport, and nothing else.
4. **The unit drawn is the event, not the emitter.** CLAUDE.md invariant 1: a signal is a
   time–frequency region with a time extent. `/api/events` already returns one row per presence
   interval with `t_start_s`/`t_end_s`/`f_center_hz`, and already refuses to invent a timespan from an
   emitter's `first_seen`/`last_seen` hull (`emitters_no_interval` discloses the rows it declined to
   expand). An emitter with no event inside the box draws nothing, however famous it is elsewhere.
5. **Two layers, and the default must not hide the unknowns.** `state` is already an `/api/events`
   filter. `confirmed` events are the prominent layer, answering the user's "confirmed identified
   signals are highlighted"; `candidate` events are a second, weaker layer. Neither may be the *only*
   layer by default, because **unknown signals are the priority to surface** (CLAUDE.md) — a view that
   highlights only what it has already explained is a view that hides the interesting part.

**The honest hard part: an event can be sub-pixel, and must not be inflated.** At the coarsest zoom a
cell spans hours × megahertz and a 40 ms burst is far smaller than a pixel. **Do not scale the box up
to be visible** — that fabricates a timespan, the precise thing `/api/events` refuses when it computes
`duration_s` and `in_window_s` itself. Two honest renderings, and the view should use the second at
coarse zoom: a minimum-size **marker** that is visibly a marker and not a box, or an **event count**
per cell. The count is the one that reads correctly at a month-wide zoom; the fat box is the seductive
wrong answer.

**A consequence worth filing: `/api/events` cannot serve the coarse zooms as it stands.** It caps at
500 expanded emitters and pages events; a zoom-0 box is 6 GHz × 30 days and will hit that cap on every
pan. The coarse zooms need an **aggregate form** — events folded to a count per (t, f) cell on the
same grid as the tile. It must **not** become a tile channel: the counts change with every ledger
append, so putting them in a sealed tile re-imports reason 2. It is computed on demand from
`emitter_observation`, which is small compared to the spectra and already indexed on
`(emitter_id, t_start, t_end)` (migration 0012). New work, named rather than assumed.

### 5.4 Retention horizon per zoom level — SETTLED: one named source per level, and it must outlive the level

There is no single global answer, because there are three real horizons with three different lengths,
and — importantly — **they are not ordered the way the ladder is**:

| Horizon | Length | Bounded by |
|---|---|---|
| IQ ring | **120 s** default (`DEFAULT_RETENTION_S`), `hk serve --iq-retention` | a byte quota derived from retention × rate |
| Spectrum history | **not an age at all** — every level's `max_age` is `None` in the default scheme | a rolling **8 GiB byte budget**, so the realised horizon depends on how busy the spectrum was |
| Observation log | **180 days** and **2 GiB**, device-named since T-378 (**T-406 raised both** from 30 days / 512 MiB; see the note under the recommendation below) | depends on the policy — see T-406's measurement |

**The rule: each zoom level names exactly one coverage source, and that source's horizon must be at
least as long as the level's own time extent.** The reason is the consequence the brief puts first —
**grey must mean unobserved at that level's horizon** — and the failure it prevents is worse than a
wrong colour: it is *grey that moves*. If a level covering a month took its coverage from the ring,
the month would go grey behind the user at ring-rotation speed, and a sealed tile's picture of the past
would change as the present advanced. **A sealed tile's grey is a fact about a past interval and must
be immutable once sealed.**

| View zoom band | Time extent per cell | Coverage answered from | Grey there means |
|---|---|---|---|
| finest (hands off to the live waterfall below it) | seconds–minutes | IQ-ring journal: segment-accurate, device-named, one segment per provenance change | *this front end was not tuned here, then* |
| middle | minutes–hours | observation log (`DwellRecord`/`SweepRecord`, device-named since T-378) | *no dwell and no sweep hop covered this (t, f)* |
| coarsest | hours–days | observation log, **up to its 30-day age**; beyond it, see below | as above, within 30 days |

**The fourth state, and why three are not enough.** The pyramid has no age limit and the observation
log does, so the two cross: on a quiet installation the 8 GiB budget can hold **measurements older
than 30 days for which no coverage record survives**. Today's three states cannot say that.
`observed` needs a span; `unobserved` claims we never looked. A surviving measurement is itself proof
we looked, so a cell with a value stays **`observed`** — but a cell with *no* value, beyond the record
horizon, is **not** unobserved. It is *we no longer know whether we looked*. Rendering that as grey
spells "never looked" for spectrum whose records we merely discarded — `Coverage::of`'s sin, one
horizon out. So the tile wire needs a fourth state, `"state": "unknown"`, carrying **no measurement
keys** by the same rule as `unobserved`, and drawn distinctly from grey. The vocabulary is already the
house one: `bias_tee: "unknown"` ≠ `"off"`, `Device::Unknown` ≠ a wildcard — **nothing said is never
permissive.**

Note this is a *wire* state, not necessarily a third `Coverage` variant. `Coverage` is deliberately
two-variant with `Sampled` `#[non_exhaustive]` and one construction site; adding a variant is a
`core_interface` change and should be made deliberately, if at all.

**The better fix, recommended: make the coverage record the longest horizon, not the middle one.** A
coverage record is an interval and a band; a spectrum cell is a measurement with a histogram. The
record is orders of magnitude cheaper per unit of time covered, so it is backwards for it to expire
first. Raising the observation log's `max_age` (and its 512 MiB cap with it) until it exceeds the
pyramid's *realised* span makes the fourth state unreachable in practice while leaving it defined for
the case where it is not. **This is the cheapest correctness purchase in the whole design**, and it is
also what T-406 needs most (§7 step 6). The arithmetic is order-of-magnitude and **unverified**: at one
dwell record per 10 s and a few hundred bytes per JSONL line, a month is ~100 MB, so the 512 MiB cap
binds at roughly five months and the 30-day age binds long before it.

**LANDED (T-406), and the arithmetic above is now measured.** The defaults are **180 days** and
**2 GiB** (`hk_store::observation::{DEFAULT_MAX_AGE_NS, DEFAULT_MAX_BYTES}`), both overridable per
run (`ObservationLogConfig::with_retention`,
`ScanPlan.extra.pipeline.observation_retention_days` / `observation_max_mb`).
`hk-pipeline/tests/iterative_scan.rs::a_dwell_records_line_cost_decides_which_retention_bound_binds`
encodes a real scan record through the production codec and measures **618 bytes** per line — inside
"a few hundred" — and derives both bounds from it. It also states the thing this section did not:
**which bound binds depends on the policy.**

- **Dwelling** (T-406's iterative scan) writes one line per step: at the 10 s floor of the user's
  range, ~5.3 MB/day, so 2 GiB holds ~400 days and the **age** binds first. That is the intended
  order, and it is what makes the fourth state unreachable in practice.
- **Sweeping** at 50 ms hops writes one aggregated record per pass or 60 s, but each carries up to
  ~1500 hop visits — two orders of magnitude denser per day — so there the **byte quota** binds
  long before 180 days. Raising the age alone would not have moved that horizon at all, which is
  why both had to move.

Still open for the user (ADR-0012 §12 Q3): whether 2 GiB is acceptable on the device's disk. It is a
ceiling reached after months rather than an allocation, and both bounds are settings.

### 5.5 Eviction — SETTLED: LRU over a byte budget, two pins, and two separate caps

The user fixed the shape (client-side eviction, tiles-in-flight cap). What is evicted, on what signal,
and what the cap is for:

**What is evicted: whole tiles, never parts of one.** The tile is the unit of decode, the unit of
transfer and the unit of memory. Evicting drops the decoded grid and its texture and keeps only the key.

**The signal: a budget on resident decoded bytes.** A tile *count* is only an honest proxy for bytes if
tiles are uniform — and **under the current scheme they are not**: level 0 holds 1024 × 60 = 61 440
cells, level 2 holds 4 096, level 3 holds 24 576. A count-based budget over that ladder is off by 15×
between levels. **So the view scheme must fix `nt` as well as `nf`** (§6 proposes 256 × 256), which
makes count and bytes the same statement. That uniformity is a requirement the user's §5.1 decision
creates and the existing store does not satisfy — worth stating plainly rather than assuming.

**The policy: LRU, with two pins and a level-distance tiebreak.**
- **Pin every tile intersecting the current viewport at the current zoom.** Never evict what is on
  screen.
- **Pin the parent level's tiles covering the viewport** — one level coarser, a quarter the count, and
  the thing that makes a zoom-out draw instead of flash.
- Break LRU ties by **distance from the current zoom**: a user three levels in is not returning to
  level 0 within a frame budget.

**The rule that matters more than the policy: "not loaded" is not "unobserved".** While a finer tile is
missing, the view draws the **coarser parent upscaled and says so** — never grey. Grey is reserved for
*genuinely unobserved*; a loading or evicted tile is a third thing and must render as a third thing.
Get this wrong and the eviction policy manufactures unobserved spectrum out of a memory-pressure event,
which is §4's first invariant broken by the memory manager. This is the single most important sentence
in §5.5.

**The cap is two caps, and conflating them is the bug.** Say which is which:
1. **Request concurrency / queue.** Bounds outstanding fetches. Its job is *latency fairness*: a fast
   pan enqueues hundreds of tiles for viewports the user has already left, and under FIFO the tiles
   that finally arrive are for the wrong place. So the queue is **LIFO with viewport-change
   cancellation** — abandon requests whose tiles no longer intersect the viewport, serve newest first.
   FIFO is what makes map clients feel laggy.
2. **Resident memory.** Bounds decoded bytes. This is the eviction budget above, and it is a different
   number from (1).
3. **Server backpressure — the one a map client does not have.** §5.2 puts the finest, live-edge tiles
   on the on-demand path, and those reads take a history lock. The report builder already reads in
   ≤ 256-row chunks under short locks specifically so that *"a report never locks ingest out for its
   whole build"*. An unbounded tile fan-out at the live edge is exactly that failure. So cap (1) is
   **also** an ingest-protection control, and its value has to be chosen against `hk-store`'s lock
   behaviour, not against a browser's connection limit. See §5.6.

### 5.6 What the user's two decisions cost — stated plainly

A design doc that only agrees with its brief is not worth writing. Four consequences, none fatal, two
of which change the plan.

1. **Fixed tiles force the viewport to snap, and that is visible.** A budget grid lays exactly on the
   window the caller asked for. Tiles quantise: a viewport that is not a whole number of cells is
   served by tiles that overhang it. For pixels that is invisible; **for a max-hold it is not** — an
   edge cell folds over the tile's extent, not the viewport's, so a peak from *outside* the requested
   window bleeds into the edge. The fix is to **snap the displayed window to cell boundaries and label
   the box actually shown**, which means a drag-selected window will not be honoured exactly. That is
   defensible — CLAUDE.md already requires navigation to discretise to achievable states and snap — but
   it is a behaviour the user should expect rather than discover.
2. **Precomputation needs an invalidation path, and this project writes to the past.** A sealed tile is
   already on disk when a replay, a late frame, or an imported SigMF recording changes the interval it
   covers. Without invalidation the big view shows a month that no longer matches the history it was
   folded from, and **the disagreement is silent**. A map server never faces this; this one imports
   recordings routinely. So: a write into an interval a sealed tile covers must mark that tile and its
   ancestors stale, and the coarse levels must be re-foldable on demand. That is work the "storage cost
   accepted" decision does not cover.
3. **Precomputation moves the cost onto the busiest moment.** It removes read latency from the coarse
   levels and concentrates the on-demand work at the live edge — where ingest is running and the lock
   is contended. §5.5's cap (3) exists because of this decision.
4. **The accepted storage is a second pyramid, not extra levels on the existing one.** §2's correction 2
   means the view ladder cannot be scheme 1 with more levels — the axes are welded. It is a **new
   scheme**, which the config already models (`scheme: u16`, per-scheme directory, a tile whose header
   disagrees with its config is refused). The good news, and it is genuinely good: **the view pyramid is
   cheap**, because its finest level is deliberately coarse (§6) — the expensive fine levels stay in
   scheme 1, where they already are, and the big view hands off to `/api/history` and the live waterfall
   below its own finest zoom rather than duplicating them.

## 6. The second axis, concretely

Three changes. None is a new subsystem; the largest is one struct field.

### 6.1 `LevelConfig` gains a `t_factor` — LANDED (T-434)

`t_cell` of level *n+1* was forced to `t_block_ns()` of level *n*. `LevelConfig` now carries
`t_factor: Option<u32>` alongside `f_factor`, where `None` is that weld — the producer's whole
`t_cells_per_block` — so scheme 1 is expressed unchanged and no stored tile moves.

Two things came with it that §6.1 did not foresee.

**A `from: Option<usize>`, because a de-welded pyramid is a DAG, not a ladder.** A node that folds
frequency alone and a node that folds time alone both need the *same* producer, so a level names the
finer level it is folded from rather than assuming `i − 1`. A producer must have a lower index, so one
seal pass in index order still folds a producer before its consumers, and a sealed tile is folded into
**every** consumer — one seal path, no second producer. `MAX_LEVELS` rises 8 → 64, because an 8 × 8
span of the two axes is 64 nodes. A ladder is the special case where every node has exactly one
consumer, which is why scheme 1's behaviour is bit-identical.

**The weld was buying the percentiles.** A tile keeps one histogram per frequency cell over the
*whole tile*, which is the parent cell's histogram **only** when a child tile is exactly one parent
time cell — the weld. Fold time by 2, or fold frequency alone, and one histogram row now spans several
parent cells with no way to split it, and a per-cell histogram is unaffordable (128 groups × 256 cells
× 440 bins is 58 MB of accumulator per open tile). So a de-welded fold writes **no** percentiles:
`p_low`/`p_high` stay unknown and reach the wire as `unknown`, by the same rule as `Coverage::of`
returning `Unobserved` rather than a zeroed `Sampled`. A view tile answers *where did I look, and how
strong was it*; the noise-floor distribution stays a scheme-1 question, and scheme 1 keeps its weld.

**Addressing.** The store key stays `(scheme, level, f_block, t_block)` — `hk_model::TileKey`,
unchanged. The per-axis coordinates are **derived from the geometry**, not carried beside it:
`Geometry::f_axis`/`t_axis` are the distinct cell widths on each axis, `axes_of(level)` gives a
level's `(level_f, level_t)`, and `level_at(level_f, level_t)` gives the level serving a pair, or
`None`. So a client keyed on `(level_f, level_t, f_block, t_block)` — which is what §8.3's shared
tile-texture LRU wants — maps onto the store exactly, and the route (§7 step 5) translates at its
edge.

This is defined for **every** scheme, which is the useful part: a welded ladder comes out as the
**diagonal** `(n, n)` and `level_at(3, 0)` is `None`. That is the honest statement of what a ladder
is — a lattice you can only move through by coarsening both axes at once — and it means the route
can answer *this scheme has no such node* instead of silently serving a level whose time cell is a
day when a second was asked for.

### 6.2 A view scheme: square ratios, uniform tiles, coarse at the bottom

Proposed, not decided — the numbers are arguable, the shape is not. 256 × 256 cells per tile at every
level, ×2 on both axes, eight levels (`MAX_LEVELS` is 8):

| View level | Frequency cell | Time cell | Tile covers |
|---|---|---|---|
| V0 (finest) | 100 kHz | 128 s | 25.6 MHz × 9.1 h |
| V1 | 200 kHz | 256 s | 51.2 MHz × 18.2 h |
| V2 | 400 kHz | 512 s | 102 MHz × 1.5 d |
| V3 | 800 kHz | 1 024 s | 205 MHz × 3.0 d |
| V4 | 1.6 MHz | 2 048 s | 410 MHz × 6.1 d |
| V5 | 3.2 MHz | 4 096 s | 819 MHz × 12.1 d |
| V6 | 6.4 MHz | 8 192 s | 1.64 GHz × 24.3 d |
| V7 (coarsest) | 12.8 MHz | 16 384 s | 3.28 GHz × 48.5 d |

At V7 the HackRF's whole 1 MHz–6 GHz range is **two tiles wide** and 30 days is **one tile tall** — the
zoomed-out picture the user asked for, in two requests. At V0 the view hands off: below 100 kHz × 128 s
the question stops being *where have I scanned* and becomes *what is in this band*, which
`/api/history` and the live waterfall already answer better than any tile would.

Uniform 256 × 256 tiles are what make §5.5's count-based memory budget honest, and the ×2 ratio makes
`fold_child` a 2 × 2 max on both axes.

### 6.3 Coverage as a third tile plane, folded by its own rule

Each tile carries three things per cell: the **measurement** (max-hold, folded by max), the
**occupancy**, and a **coverage** plane. The coverage plane is the second axis paying off, and it is
the composition §2's correction 3 named:

- **Per level-0 cell, coverage is the union of both answers.** `observed_s` = the seconds of the cell
  covered by *either* a frame that landed here (`Tile::obs_s`, already computed) *or* an interval record
  whose band covers this frequency cell and whose time span intersects this time cell. The record side
  is the new part, and it is not new data: the ring journal's segments and the observation log's
  `ObservedWindow`/`HopVisit` are already `(t_start, t_end, f_lo, f_hi, device)` tuples — today they are
  rasterised onto the frequency axis only and collapsed on time. **Per-(t, f) coverage is that same
  rasterisation, with the time axis kept.** `ObservedWindow::covered()` already removes the DC notch, so
  a notched dwell contributes two bands and the notch stays honestly grey.
- **`Coverage::of`'s refusal carries over unchanged, applied per cell instead of per column**: zero
  spans, non-positive observed duration, or a non-positive window is `Unobserved`, never a zeroed
  `Sampled`. No new type is needed — `Sampled` already carries `spans`, `observed_ns`, `duty`, `last`,
  `center_hz`, `sample_rate_hz`, and all six are per-cell computable.
- **The fold is a sum, not a best-of** — §4's finding. A parent cell's `observed_s` is the **sum** over
  its four children, and `duty` is recomputed as `observed_s / parent extent`, never averaged from the
  children's duties. **`observed_s` is foldable; `duty` is not.** In a fixed pyramid the children
  partition the parent exactly, so the sum is exact, and `duty < 1` *is* the partial-coverage statement
  §4 requires — the existing field already expresses it once it is computed against the parent's own
  extent.
- **Device stays in the key, not in the cell.** Coverage is device-local (T-259/T-305), so the tile key
  extends to `(device, scheme, level, f_block, t_block)` with `any` as the default — and `any` keeps
  `"named": false` so it can never wear a radio's identity. This maps onto machinery that exists:
  `OriginFilter` and `/api/history`'s `source` filter already scope the pyramid by front end. It is also
  why *"fills in as more SDRs are added"* works without new concepts: another SDR is another coverage
  plane, and the union plane grows.

### 6.4 What it costs on disk — measured (T-434)

§7 step 4 said this is where the storage cost lands, and §5.2 accepted that cost sight-unseen. It is
now measured, through the production codec, on a real `Pyramid` fed real frames and sealed through
the real seal path: `crates/hk-store/src/history/tests/lattice_cost.rs`. Three findings, two of them
things §6 did not say.

**A 256 × 256 level-0 tile costs `1548 + 1.24 × cells` bytes** (least squares over five fill
fractions, a noise floor with carriers on it, zstd level 3). A full tile is ~83 kB, not the ~918 kB
its fourteen bytes a cell would suggest — zstd returns 11× on spectrum data. The fixed part is under
2 % of a full tile, which is what makes §5.5's **count**-based client budget an honest proxy for
bytes. Tile *size* is not free either: the same data in 64 × 64 tiles cost **2.30** B/cell against
**1.26**, so §6.2's 256 × 256 is the cheaper tile as well as the uniform one.

**Finding 1 — the level count is multiplicative; the bytes are not.** A welded ladder's levels shrink
×4 a step and sum to ≈1.33 × its finest. A lattice's shrink ×2 a step *on one axis*, so it sums to
`(Σ2⁻ⁱ)(Σ2⁻ʲ) → 4`. Measured over a 4 × 4 lattice fed the same data: the whole lattice is **4.8× its
finest node** and **3.2× the welded ladder embedded in it as the diagonal** — the same data, folded by
the same code, in the same run, so nothing is modelled. Sixty-four nodes cost a handful of finest
levels, not sixty-four of them.

**Finding 2 — a coarse cell costs *more* than a fine one, which §6 assumed away.** Bytes ∝ cells
predicts a fold halves a node. It does not: level 0 came out at **2.30 B/cell** and every coarse node
at **3.4–3.6 B/cell**, a **1.5× penalty**. A level-0 tile's neighbouring cells are a slowly varying
noise floor sampled a second apart and zstd eats them; a folded cell is a max over children and its
neighbours are much less alike. **Folding destroys the correlation the compressor was living on.**
Pricing a pyramid as bytes-per-cell × cells understates it by half as much again.

**Finding 3 — neither axis is the cheap one.** A pure frequency fold halves the cells and so does a
pure time fold; the two arms of the lattice came out within 2 % of each other. There is no axis to
materialise preferentially and none to leave to read-time folding on cost grounds.

### 6.5 Which horizon binds, at which `(level_f, level_t)`

Derived from the measured per-cell cost the way T-406 derived both of the observation log's bounds
from one measured 618 B/line — and, as there, **which bound binds depends on the policy**. Policy:
one front end dwelling continuously on a 20 MHz window, the HackRF's practical live extent.

| Scheme | Finest cell | Bytes/day | 8 GiB byte budget binds after |
|---|---|---|---|
| Scheme 1 (welded ladder) | 6.25 kHz × 1 s | **771 MB** | **11 days** |
| View lattice (§6.2) | 100 kHz × 128 s | **1.26 MB** | **6 842 days** |

**The finding: the view lattice is so cheap that its byte budget stops being the binding horizon, and
the observation log's 180-day age binds instead — at every `(level_f, level_t)`.** §5.4 argued the
fourth state (`"unknown"`: a surviving measurement whose coverage record has expired) would be
unreachable in practice, because T-406 lengthened the record until it outlived the pyramid's
*realised* span. That was measured against scheme 1, which exhausts 8 GiB in a couple of weeks. A
view lattice is three orders of magnitude sparser and does not fill it for years. **T-423's fourth
state is reachable again**, on any installation that runs half a year — it is defined, on the wire,
and now has a date. The cheap correctness purchase §5.4 recommended is a longer observation log, and
this says how much longer it would have to be to keep the state unreachable.

**The corollary, and it is the real price of de-welding.** The budget is shared, so what the lattice
spends on coarse summaries the finest node does not get: under a ladder the finest level holds 1/1.33
of the bytes, under the lattice 1/4.8. **The same byte budget buys the live edge about a third of the
history it used to.** Bounded and stated, not a surprise — and for the view lattice it is moot, since
the budget does not bind there at all. It is not moot for scheme 1, which is why scheme 1 stays a
ladder.

## 7. What to do next, in order

A buildable sequence. Each step says what it unblocks and which consumer it serves.

> **Sequence state, 2026-09-17.** Steps **1, 2, 3, 4 and 6 have landed** (T-421, T-423, T-419,
> **T-434**, T-406). **§8 (the unified surface) changes what steps 5 and 7 are for** — step 4's
> `t_factor` was the de-welding the spike needs, and steps 5 and 7 are re-planned around T-437's
> outcome. Step 6 ran early because it only ever depended on step 1. The next step is **5** (the tile
> route, T-438); step **7** (the big view client) follows it. The disk cost step 4 existed to measure
> is in **§6.4**: cheaper in total than §6 assumed, more expensive per cell, and it moves which
> retention horizon binds. Update this line when a step lands, so the state of the sequence is
> readable without reading the whole section.

1. **Per-(t, f) coverage rasterisation in `hk-store`. LANDED (T-421, `6a37221`).** The record-derived answer gains a time axis:
   `Coverage` computed on an `(nt, nf)` grid from the same ring-journal and observation-log intervals,
   reusing `Sampled` and `Coverage::of` unchanged. Pure backend, no wire change, no new type.
   **Serves:** nothing visibly yet. **Unblocks:** everything below, and it is the only step that closes
   §2's actual gap.
2. **Coverage on the grid routes that already exist. LANDED (T-423, `ed5521b`).** `/api/timeline` grows a per-cell coverage state;
   `/api/coverage` grows a time axis. `docs/api.md` and `crates/hk-cli/tests/api_contract.rs` together,
   per T-079, asserting the **field's value** and not the response shape (T-315's standard).
   **Serves:** T-405 (the survey bar stops reading "sampled, level not retained" for a cell the radio
   was tuned away from) and T-411 (the left time navigator, same fix on the other axis).
   **Unblocks:** the invariant gets a test home *before* any tile exists, which is where it is cheapest.
3. **Fix the two coverage folds. LANDED (T-419, `eaef9e6`).** (§4): `Tile::fold_child` sums `obs_s` across frequency children
   instead of taking the best; `OverviewCell::fold` weights by extent, or states its exactness
   condition. Land the two guards from §4 — prove a level against level 0, and carry the input
   statistic's provenance up the ladder. **Serves:** every consumer, silently. **Unblocks:** trusting
   any coarse level at all. *Do this before building the view scheme, not after* — a pyramid founded on
   a coverage fold that rounds up will paint the spectrum as scanned, which is the one thing the user's
   feature must not do.
4. **`LevelConfig::t_factor` and the view scheme** (§6.1, §6.2) — **LANDED (T-434).** See §6.4 for
   what it cost and §6.5 for what it bought. Sealed coarse levels produced by the
   existing seal path, uniform 256 × 256 tiles, the coverage plane from step 1, the input-statistic
   provenance from step 3. No new view yet. **Serves:** nothing visible. **Unblocks:** steps 5 and 6.
   This is where the storage cost lands, so it is the step to measure disk on.
5. **The tile route.** `GET /api/tiles/…` addressed by `(device, zoom, t_index, f_index)`, coarse levels
   served from seal, the finest level computed on request under the chunked-lock discipline, with
   §5.5's cap (3) as ingest backpressure. Plus the events **aggregate** form from §5.3 for the coarse
   zooms. **Serves:** the big view's backend, and — because the bars are projections of the same
   pyramid — it is what stops T-405/T-411 and the big view ever disagreeing on one screen.
6. **T-406's accumulation. LANDED.** The sweep already writes observation-log records and history
   frames; the new work is that a dwell step must write **one record per step with its true band and
   interval**, not one coarse record spanning the sweep, or step 1's rasteriser cannot see the shape
   of what was scanned. This is the **second consumer of §5.4**, and the one that feels the horizon
   decision most: a coverage record is what makes *"a region the sweep cleared last week"*
   distinguishable from *"a region the sweep has not reached"*. §5.4's recommendation to lengthen the
   observation log is T-406's dwell retention. **Serves:** T-406.

   **What landed, and the one decision that carries it.** `hk_core::scheduler::IterativeScan` is a
   **dwell policy over the scheduler that already exists** — a `ScanPolicy::DwellOnly` plan over the
   device's tunable ranges with a configurable `region_dwell_ns` (default 15 s; 10–30 s is the range
   the user named and `IterativeScan::recommended` reports it). No new scheduler, no new
   accumulator. Reachable as `--survey-dwell SECONDS` on `hk run`, `hk replay` and `hackriffd`, and
   carried inside the plan itself (`extra.scheduler.region_dwell_s`) so a `--plan` file round-trips
   the policy.

   The load-bearing decision is the step **purpose**. A `DwellOnly` hop emits
   `Purpose::RegionDwell`, and `ObservationRecorder` writes one `DwellRecord` per step with that
   step's own `window` and `observed`; only `Purpose::Sweep` aggregates, into a record that spans a
   pass. Implementing the scan as long sweep hops would have produced exactly the coarse record this
   step warns about. The record → `CoverageSpan` mapping moved into `hk-store`
   (`hk_store::spans_from_records`) beside the rasteriser that consumes it, so there is one place
   that decides what shape a record takes on the grid, and `/api/coverage` calls it rather than
   keeping a copy.

   **Proved end to end, not per layer.** `hk-pipeline/tests/iterative_scan.rs` runs the chain
   scheduler → recorder → observation log → coverage spans → `grid_over` and asserts the pass
   rasterises as a **diagonal**: at the instant the tune was on hop *k*, hop *k* is `Observed` and
   every other hop is `Unobserved` — observed-and-quiet kept apart from not-yet-reached, per cell —
   while the same spans collapsed to one row say the whole band *was* cleared over the pass. It also
   asserts the fourth state (rows before the record horizon), and that a notched dwell reaches the
   rasteriser as two bands. `hk-pipeline/tests/iterative_scan_device.rs` runs it **through the mock
   SDR device**: the scan retunes the radio, every record names the `device_id`, coverage spreads
   across the walked range and stops there, and the spectrum-history **pyramid** — the same one this
   document's big view and survey bar read — holds frames across it (140/140 frequency cells
   measured).
7. **The big view client.** Tiles, the `/api/events` overlay, LRU eviction with the two pins, the LIFO
   cancellable queue, and the parent-upscaled-and-labelled fallback that keeps grey meaning unobserved.
   **Serves:** the user's ask.
8. **Only if wanted: per-device tile planes beyond `any`.** The key already has the slot; the common
   question is *have I scanned here*, and *which radio saw it* is a drill-down.

**And the standing instruction, unchanged: do not let T-405 and T-406 foreclose this.** T-405 takes its
fold and shading from the pyramid rather than inventing one; T-406 accumulates into it rather than into
a private accumulator.

## 8. The unified surface — one canvas, one pyramid (user decision, 2026-09-17)

**Status: decided by the user; the de-risking spike is T-437.** This section supersedes the separate
live-waterfall / history-view / edge-navigator design. It is not a new idea bolted on — it is the
completion of what §3 already implied when it said *the survey bar is a miniature of the big view*.

### 8.1 The decision

Collapse the **live waterfall, the history view and both edge navigators** into **one surface**: a
single virtual canvas with **X = frequency across the full device range (1 MHz–6 GHz)** and **Y =
time**, rendering only data we actually have, backed by the coverage tile pyramid this document
already specifies (`hk-store`, `TileKey`, `Coverage`, the `Resolution` tiers).

**Grey = genuinely unobserved, and that grey is the point.** It stops being an edge case in a
renderer and becomes the main information the surface carries: across 6 GHz and a retention window,
most of the canvas is honestly empty, and the shape of what is *not* grey is the survey.

**The leap: the live view is also just a viewport into this surface** — "live" is the finest-level
growing edge where hardware is currently tuned. There is no separate live-versus-history UI any more,
and therefore no seam between them to keep consistent.

### 8.2 The backend change: de-weld the axes

Frequency (6 GHz) and time (the retention window) are different quantities with wildly different
extents, so **no single pixels-per-unit can serve both**. Each axis zooms **independently** and snaps
to **its own** pyramid level.

**§5.2's Correction 2 already named this defect** — the default ladder coarsens frequency ×16 and time
×86 400 across its levels, which is a ladder for *one band over a long time*, not a map. This section
turns that observation into the requirement: **`level_f` and `level_t` are independent coordinates.**

"Constant pixel density everywhere" means **uniform in-level density per pane** — the slippy-map
invariant — **not** one global scale. This also replaces the earlier asks to make the two scrubbers'
densities match and to let each scrubber's wheel scale its own density: same behaviour, obtained once
from one renderer instead of twice from two widgets.

**This is the backend half of §7 step 4** (`LevelConfig::t_factor` and the view scheme, filed as
T-434), which is now a prerequisite of the spike rather than an independent step.

### 8.3 The renderer: one canvas element, one WebGL2 context

- **Split panes** via `gl.viewport` + `gl.scissor`. Each pane carries its own
  `(center_f, span_f, center_t, span_t)`, resolves to its own per-axis pyramid level, and draws only
  the tiles inside its box.
- **A shared tile-texture LRU** keyed by `(level_f, level_t, f_block, t_block)`, uploaded on demand
  under a budget. **A tile visible in two panes uploads once** — and that sharing is precisely why
  this must be *one* context rather than one per pane.
- **A zoomable minimap** is simply another viewport at a coarse level, overlaying rectangles for where
  each pane is looking and lit segments for where each SDR is currently live.
- **The honesty tiers stay visually distinct** — `live-iq`, `spectrum-history`, `survey-overview` —
  so a wide or deep zoom never fakes resolution the hardware did not capture. §4's rule is unchanged
  and now has one place to be enforced instead of three.

### 8.4 Multiple SDRs, and what panes are actually for

Several tuned ranges populate at once and **may be far apart** — 100 MHz and 2.4 GHz are not
watchable in one viewport at any useful density. So panes exist to **split the canvas and
independently pan, zoom and follow-live different sections of the same surface**.

- **Per-pane follow-mode** pins that pane to the growing edge.
- **Pause freezes a pane's view only.** Capture, the ring and detection remain always-on — the
  invariant is unchanged, and T-347 has just made pause per-viewer rather than per-run, which is the
  same direction arrived at independently.
- **Panning a pane to an un-tuned frequency offers or triggers a retune**, reusing T-343's gated,
  typed device action with `device_id` recorded. One capture at a time, settle gap honoured.

**This subsumes T-380** ("one view window, even with multiple SDRs"). T-380's invariant was that extra
front ends widen *coverage* and never split the *view*; under this design the surface is still one
surface and the panes are viewports onto it, so the invariant survives in a stronger form: there is
exactly one thing being looked at, and panes are where you look from.

### 8.4a How the pane model expresses pause, follow and level (T-442, 2026-09-17)

Built in `ui/src/surface/panes.ts`, over T-440's renderer. Four decisions worth recording, because
each one is a place the obvious implementation reintroduces a defect the repo has already paid for.

**A pane's pause IS its time window — there is no flag.** T-347 retired `/api/control/pause` because
a run-wide boolean cannot represent N viewers; a per-pane boolean beside a per-pane window is the
same defect scoped smaller, because the two can disagree. So `TimeWindow` is a discriminated union
whose arms **carry different data**: a following pane has *no centre of its own* (it borrows the
growing edge, re-derived every frame), and a frozen one has one. "Scrubbed but not paused" — the
third state T-347 refused — is therefore not a state the type can spell. Freezing is a **coordinate
change, not a mode change**: it writes down the window the pane was already showing, so the frame
you pause on is identical to the frame before it, and pausing an already-scrubbed pane is a no-op
rather than a jump to the live edge.

**Pause is unable to reach anything.** Every pane operation — pan, zoom, pause, resume, split, close
— is arithmetic over this client's own view state, so *on the wire, pausing is nothing*. That is
what makes one pane unable to affect another pane, another browser, or the radio, and it is asserted
the way T-340 asserts its own control: a spy `fetch` sees an **empty call list** after the whole
gesture vocabulary has been exercised. Capture, the ring and detection are never consulted; the live
edge is *reported in* to `views(edgeNs)`, never controlled from here.

**Panning in time freezes first, and never silently re-follows.** A pane pinned to the edge that
also carries an offset from it is exactly the third state; so a scrub converts the anchor. Dragging
forward clamps at the edge and *stays frozen* — re-entering follow is an explicit act, not a
consequence of a gesture ending near the edge (the T-407 lesson, one axis over).

**The level is stated per pane** (§8.5a's correction). `paneStatuses()` joins pane state to the
`PaneReport` the renderer actually drew with, so the stated level is the drawn level rather than a
second calculation that could disagree with the pixels, and `levelDivergenceNote()` is the sentence
shown when panes differ: *a coarser cell is the maximum over more cells, so the same energy
legitimately reads differently — same ramp, same scale, stated level.* Stating it is the fix; hiding
it is what invites the bug report.

**T-380's invariant, operationalised:** `split` hands back two panes on the **identical** box, and
they diverge only when the user moves one; the last pane cannot be closed, because with no viewport
there is nowhere to look *from* and the surface does not stop existing because the window did. A
pane's `device` selects **whose coverage plane decides its grey** (`any` = the union) — a coverage
selector, not a second subject.

### 8.4b Retune-on-pan: the gesture, and why it cannot fire from a drag (T-444, 2026-09-17)

Built in `ui/src/surface/retune.ts`. §8.4's third bullet — *panning a pane to an un-tuned frequency
offers or triggers a retune* — resolves to **offers**, and the choice is forced rather than
preferred: T-340's control drags ±1.0 of the whole 6 GHz surface through a spy client and asserts an
**empty call list**, and T-442 re-asserted the same shape over the entire pane vocabulary. A pane's
pan is a pan. So the retune is a **discrete, explicit act on a separate control** — T-343's
`edgeOffer` and T-392's region-select-on-release are the precedents this repo has already argued
through — and the whole of the gesture design is that *the commit is not a gesture*.

**T-407's lesson, taken one surface over.** T-407 found two ways a finger could retune the radio,
both latent until T-392 removed a confirmation step: the drag threshold was the mouse's 6 px, so a
fat-fingered tap was a drag; and travel was measured as `clientX + clientY`, so a stroke *across* a
bar counted as travel *along* it. The shape of both is **a continuous pointer stream misread as a
committing act**, and neither was introduced by the ticket that exposed them. A better threshold is
not the answer to that. What is: `acceptPaneRetune` **re-derives the offer from the pane's state at
the instant of the commit and refuses (`"moved"`) if the planned `(centre, span)` has changed**. A
pan therefore *invalidates* a pending offer instead of silently re-aiming it — the radio goes where
the button said, or it goes nowhere — and a finger still dragging the pane cannot command a
frequency the control was never labelled with. (A pan too small to move the snapped configuration is
not a moved target; refusing that would protect nothing and make the control unusable.)

**Nothing is re-derived.** The capture configuration is `retunePlan`'s: T-341's `snapCenter` for the
achievable-centre grid, `smallestCoveringSpan` for the narrowest window that still covers the pane,
and T-418's derived off-DC placement (`span/4`, the midpoint of the usable half-band, maximally far
from the LO spike at DC and the anti-alias roll-off at Nyquist), bounded by the pane staying inside
the window and by the snap's own half-step, **without ever widening the window to buy the dodge**.
The module decides *whether* to offer; it does not decide *where*, and a source assertion keeps it
that way.

**Three ways there is nothing to offer, each a statement rather than an omission:** a live window
already contains the pane (coverage is containment, not overlap, and a pane pinned to one front end
is not covered by another's window); the pane is **not showing the growing edge**, since a retune
changes only what is captured from now on and offering would imply the past could be re-observed;
and no grid was reported, because not knowing what the front end can do is not evidence that it can
do this.

**At the band edge the offer is disabled, not clamped** — T-409's rule, and the reason `retunePlan`
tests `containsCenter` rather than `snapCenter`, which would walk an out-of-band request *inward*
and call every out-of-range centre achievable. A pane past the top of the tunable range gets a
stated refusal (`center_out_of_range`) and a control that does nothing when pressed; a pane wider
than one capture window is survey overview (`span_too_wide`) and says so. A clamped retune that went
somewhere other than the label is the control that lies, and it is the one thing this must not do.

**The one client-side addition the spike asked for (§5.2):** after a retune the front end **took**,
the growing edge's tiles are invalidated. They were computed *on request* from the tuning that has
just ended, so a cached one is an observation claim about a tuning that no longer exists — which
makes this the grey-honesty rule, not a freshness nicety. Two details: **coarser levels go too**, or
the upscaled-ancestor fallback keeps drawing the old tuning underneath the new one; and an
**in-flight** fetch is marked stale rather than merely aborted, because it lands *after* the retune
and the cache would otherwise accept it into the empty slot it just made. `applyDeviceAction` now
returns whether the front end took the action, because "only after a real retune" cannot be read off
a toast.

### 8.5 What this removes

The two bespoke edge-scrubber widgets collapse into canvas pan/zoom plus the minimap. The
live-versus-history split disappears. And the recurring scrubber defect family goes with them —
sliver-of-data (T-420), box-jump (T-388), fill and resolution (T-397/T-411), axis and colormap
divergence (T-397), wheel-zoom mismatch (T-412) — because **one renderer with one set of semantics
cannot disagree with itself.** Every one of those was two implementations of the same idea drifting
apart.

**Freeze, per the user:** no further polish investment in the separate left-time and bottom-frequency
navigator widgets. Genuine backend bugs behind them stay (T-347 pause-is-global, T-348 paused-view
CPU) — those are not scrubber polish, they are correctness.

### 8.5a What the spike proved, and the three places §8 and §6 were wrong (T-437, 2026-09-17)

**Verdict: YES for the renderer, NO for the system as it stands** — and two of the blockers are
outside the renderer entirely. None of the findings is a reason to abandon the design; all four are
cheaper to fix than the scrubbers were to keep. Evidence: `spikes/t437-unified-surface/`, 31
automated checks on real WebGL2, with a re-run recipe.

**The renderer is the easy half, by two orders of magnitude.** 48 panes at **p95 2.2 ms**, flat in
pane count — what scales is draw calls (~450 at 48 panes) and 450 trivial quads is nothing. The
shared LRU works exactly as §8.3 claims: **95 distinct keys, 95 uploads, 8 panes sharing one tile,
18.68 MB resident against 149.44 MB for one cache per pane.** Upload is 0.026 ms/tile, so **size the
budget by memory, not upload time.**

**The real cost is tile PRODUCTION, three orders of magnitude larger** — the stub built a tile in
~500 ms mean over 313 tiles, 50–110 s to fill a screen; even at a hypothetical 10 ms server-side,
208 tiles is 2 s. **That is the number T-438/T-440 design against, not the 2 ms of rendering.**

**F1 — §6.2's V0 floor is wrong, and it re-welds the axes §8.2 de-welds.** §6.2's V0 time cell is
**128 s** and the IQ retention window is **120 s**, so **the entire live view fits inside one time
cell**. "Live is a viewport onto the finest growing edge" is not coarse there, it is
*unrepresentable*. Worse, every realistic pane — seconds to tens of minutes across ~840 px — is finer
than 128 s, so **`level_t` pins at 0 and the de-welding buys nothing on the axis it was introduced
for**. §6.2 must extend the ladder **downward in time to the store's own level-0 floor**
(6.25 kHz × 1 s), which is what the spike ran on and where everything works. **The floor is the
decision, not the ratio.**

**F2 — `/api/history` treats a per-axis budget as a LEVEL SELECTOR, not a fold target.** Same window,
same band, only `max_f` changed: `max_f=384` served 38 784/38 784 cells observed (100 %);
`max_f=256` served 384/576 (**67 %**). Tightening the *frequency* budget 1.5× cost **34× of time
resolution and turned a third of the window grey**. That is a **grey-honesty violation caused by
level choice, which §4 does not name** — §4 guards the fold, and the fold is fine; here a cell reads
*unobserved* while level 0 holds the measurement. `/api/timeline` does not have this defect (it folds
onto exactly `nt × nf` and walks finest-ward when a tier is empty, per T-426). **So T-438's route is
`/api/timeline`'s engine plus `/api/history`'s `t0`/`t1`.**

**F3 — an undersized client tile budget makes the surface LIE.** Below `budget ≈ working set` the
cliff is sharp and predictable, but the consequence is not performance: **101–198 tiles per frame
render grey**, and grey is this surface's load-bearing claim that the radio never looked there. **A
memory budget must never be able to manufacture that claim** — T-440/T-441 must distinguish
*not-resident* from *unobserved*.

**F4 — after a retune the pyramid stops recording, permanently.** Filed as **T-446**. The ring stays
healthy and the record-derived coverage plane says `observed, duty 1.0`, while the measurement plane
writes nothing and never recovers. It falsifies §8's central claim the moment you use §8.4's primary
gesture.

**The key needs `device` and `scheme`.** §6.3 already says coverage is device-local; §8.3's four-part
key omits it, and retrofitting a key is the expensive kind of change. T-434 confirms the per-axis
coordinates are *derived from the geometry* (`axes_of`, `level_at`) with `TileKey` unchanged, and that
`level_at` returns `None` where a scheme has no such node — **a welded ladder is the diagonal**, so
the route must answer "no such node" rather than snap to a level whose time cell is a day.

**§8.5 overstates the anti-divergence claim, and the overstatement invites a reopened bug.** Measured,
pane A, pane B and the minimap at the *same* level sample bit-identical pixels. But the guarantee is
**"same ramp, same scale, stated level"**, not "same picture": two viewports at different
`(level_f, level_t)` legitimately differ, because a coarser cell is a max over more cells. Since the
minimap is 6 GHz wide it is nearly always at a different level. **The fix is to state the level per
pane, not to hide the difference.**

### 8.6 The spike, and its exit criterion

**T-437**, on replay/synthetic through the mock SDR, and it **must not block the live path**. Four
things to prove end to end:

1. **De-welded per-axis tile addressing** served over a route — independent `level_f` and `level_t`.
2. **One WebGL2 context, N scissored panes**, a shared tile LRU, grey-for-missing, honesty tiers
   visibly distinct.
3. **The live edge** writing finest tiles for tuned ranges, **per-pane follow-mode**, and
   pan-to-untuned → retune.
4. **Split panes plus the zoomable minimap**, with pane-viewport rectangles and per-SDR live segments.

**Exit criterion:** it convincingly replaces **both** edge scrubbers **and** the separate history view.
If it proves out, this section becomes the UI direction and §7's remaining steps are re-planned around
it.

## Sources

- User direction, 2026-09-16 (recorded in the session memory `user-full-spectrum-zoomable-history-view`);
  §5.1/§5.2 decision, 2026-09-17.
- `CLAUDE.md`, "Time, the waterfall, and the live view" — the coverage rule and the navigation-honesty rule.
- T-368 (coverage map), T-342 (fold semantics and scale), T-334/T-341 (resolution tiers), T-338 (the
  timeline as an overview waterfall), T-378/T-314/T-377 (device chain of custody), T-386 (the durable
  catalogue is an independent surface, with its four reasons), T-397 (max-hold must not manufacture
  observation; the survey bar as a real mini-waterfall), T-264 (`/api/events`), T-116/T-126 (retention
  overrides and tile trimming).
- Code read for this design: `crates/hk-store/src/history/{config,tile,codec,query,store}.rs`,
  `crates/hk-store/src/coverage.rs`, `crates/hk-store/src/observation/log.rs`,
  `crates/hk-store/src/iqbuffer.rs`, `crates/hk-api/src/coverage.rs`,
  `crates/hk-model/src/repo/{inventory,presence}.rs` and `migrations/0001_init.sql`.
