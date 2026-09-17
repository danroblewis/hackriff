# 16 — The coverage tile pyramid, and the full-spectrum-over-time view

**Status: design, not built.** Proposed by the user on 2026-09-16. This document exists so that three
features already in flight are built against one piece of infrastructure rather than three private
ones.

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

## 2. Why this is not a new subsystem

Most of it exists. The work is mostly *connecting* rather than building.

| Piece | Where it already is |
|---|---|
| Tiered, multi-resolution history | the spectrum-history pyramid (`hk-store`), tile format 6 |
| Coverage: observed vs never-observed | `hk_store::Coverage` (T-368), `Coverage::of` refuses to spell "never looked" as "quiet" |
| Max-hold fold with stated semantics | T-342's `grid.semantics` on `/api/timeline`, `shade` block on `/api/coverage` |
| Which device looked where | T-378 (observation log) + T-314/T-377 (the chain of custody) |
| A time-axis budget that never truncates the window | T-342's `resolution.budget`, sibling of `/api/floor`'s `max_steps` |
| Per-frequency-cell resolution tiers | `Resolution` / `resolution.source` (T-334): `live-iq` / `spectrum-history` / `survey-overview` |

**The gap is the second axis.** Today the pyramid is tiered in *time* and the coverage map answers per
*frequency cell over a window*. The big view needs tiles addressed in **both** axes at several zoom
levels, which is a different indexing problem from either.

## 3. The three consumers, and what each needs from one pyramid

1. **The big zoomable view (§1).** Tiles at several zoom levels over (time × frequency), grey where
   unobserved, confirmed emitters highlighted on top.
2. **The bottom frequency-survey bar** (T-405). The same data at `rows = 1` — *a miniature of the big
   view*, which is exactly how T-342 already describes `/api/timeline` with `rows = 1` relative to the
   full overview. It should be a **projection of the pyramid, not a separate query.**
3. **Iterative-scan accumulation** (T-406). A sweep retains what each step saw **into the pyramid**,
   so the big view and the survey bar fill in as it runs. No private accumulator.

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
  precisely the property the user is missing today (T-405: "yellow/red peaks not showing because
  averaging washes them out").
- **Device-local, never unioned by accident.** Coverage is a fact about **one front end** (T-259/T-305).
  `by_device` returns one grid per device; a union is a separate call returning `Device::Any` with
  `"named": false`, so it can never wear a radio's identity. **This matters more as SDRs are added,
  which is exactly when the view gets interesting.**
- **The scale must be stated.** T-342: a shade was a 0–1 number normalised against a range the response
  never named, so the same energy read as two strengths on one screen. Every tile carries its fold,
  its scale and its range.
- **Never imply resolution the front end did not capture.** A wide zoom is **survey-history overview**,
  not live IQ, and the view must distinguish them (T-334's `resolution.source`).

## 5. Open design questions — to settle before building

1. **Tile addressing.** Fixed `(zoom, t_index, f_index)` like a map, or per-axis budgets like
   `/api/timeline`'s `columns` and `/api/floor`'s `max_steps`? The budget idiom is already established
   here and composes with "a budget never truncates the window"; map tiling is more cacheable. They are
   not obviously compatible — pick one.
2. **What downsamples, and when.** Precomputed at seal time (like the history pyramid's tiers) or on
   demand? Precomputation bounds read latency and costs storage in a system that has been at 9.5 GB free
   today; on-demand is the reverse trade.
3. **Confirmed-signal highlighting.** Emitters are time-scoped to a view window in Explore, but the big
   view is a **history** surface — the durable catalogue, which T-386 established should stay
   independent of the view cursor. So: which emitters does a tile show, and in whose time frame?
4. **Retention.** The IQ ring is seconds to minutes; spectrum history is longer and lossy; the
   observation log is 30 days. The big view's grey is only meaningful against a stated horizon.
   **Say which horizon a zoom level answers for.**
5. **Bounded browser memory** is the user's stated reason for tiles. That implies an eviction policy in
   the client and a cap on tiles in flight — a UI concern, but one the tile size and zoom ratio decide.

## 6. What to do next, in order

1. **Do not let T-405 and T-406 foreclose this.** Both are filed with a note pointing here. T-405 must
   take its fold and shading from the pyramid rather than inventing one; T-406 must accumulate into it.
2. **Settle §5.1 and §5.2 first** — addressing and downsample timing determine everything else.
3. Then the pyramid's second axis, then the three consumers as projections of it.

## Sources

- User direction, 2026-09-16 (recorded in the session memory `user-full-spectrum-zoomable-history-view`).
- `CLAUDE.md`, "Time, the waterfall, and the live view" — the coverage rule and the navigation-honesty rule.
- T-368 (coverage map), T-342 (fold semantics and scale), T-334 (resolution tiers), T-378/T-314/T-377
  (device chain of custody), T-386 (history is an independent surface), T-397 (max-hold must not
  manufacture observation).
