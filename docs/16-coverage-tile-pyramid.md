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

### 6.1 `LevelConfig` gains a `t_factor`

Today `t_cell` of level *n+1* is forced to `t_block_ns()` of level *n*. Add a `t_factor: u32`
alongside `f_factor`, defaulting (for scheme 1) to the finer level's `t_cells_per_block` so the
existing ladder is expressed unchanged and no stored tile moves. With it, a square ladder becomes
config.

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

## 7. What to do next, in order

A buildable sequence. Each step says what it unblocks and which consumer it serves.

> **Sequence state, 2026-09-17.** Steps **1, 2, 3 and 6 have landed** (T-421, T-423, T-419, T-406).
> Step 6 ran early because it only ever depended on step 1. The next step is **4**, which is where the
> storage cost lands and therefore the step to measure disk on; steps **5** (the tile route) and **7**
> (the big view client) follow it and are not yet filed. Update this line when a step lands, so the
> state of the sequence is readable without reading the whole section.

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
4. **`LevelConfig::t_factor` and the view scheme** (§6.1, §6.2) — **NEXT; filed as T-434.** Sealed coarse levels produced by the
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
