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
2. **The bottom frequency-survey bar** (T-397/T-405). Originally filed as "the same data at
   `rows = 1`". **T-397 raised it to R rows**, because one row stretched down a 64 px bar is not a
   miniature of a spectrogram — it is a spectrum scaled up. It is now a projection of the pyramid,
   not a separate query: `GET /api/timeline?f_lo&f_hi&columns=R&rows=N` over the survey viewport,
   which is the *same route and the same fold* the time navigator uses, differing only in which axis
   is collapsed. **No second axis was needed for the fold**: `RegionHistory::overview` has always
   taken `(nt, nf)` and folds exactly on both (T-338/T-342), so §2's gap is narrower than it reads —
   it is about **tile addressing at several zoom levels**, not about the fold.
   - **What the survey bar still cannot get** is per-(time, frequency) *coverage*.
     `hk_store::Coverage` answers per frequency cell over a window, so it decides a **column** grey,
     while the history grid decides a **cell**. The bar therefore draws a measured value wherever the
     grid holds one and falls back to the column's coverage state where it does not. That composition
     is honest but it is the place the second axis would actually pay: a cell in an observed band that
     the radio was tuned away from at that instant should be grey, and today it reads as
     "sampled, level not retained".
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
- **Device-local, never unioned by accident.** Coverage is a fact about **one front end** (T-259/T-305).
  `by_device` returns one grid per device; a union is a separate call returning `Device::Any` with
  `"named": false`, so it can never wear a radio's identity. **This matters more as SDRs are added,
  which is exactly when the view gets interesting.**
- **The scale must be stated.** T-342: a shade was a 0–1 number normalised against a range the response
  never named, so the same energy read as two strengths on one screen. Every tile carries its fold,
  its scale and its range.
- **Never imply resolution the front end did not capture.** A wide zoom is **survey-history overview**,
  not live IQ, and the view must distinguish them (T-334's `resolution.source`).

## 5. Design questions — §5.1 and §5.2 decided by the user, 2026-09-17

**The two gating questions are settled, Google-Maps-style.** The remaining three are settled *within*
that framework as part of the design work; they do not gate and do not need the user.

**§5.1 Tile addressing — DECIDED: fixed `(zoom, t_index, f_index)` map tiles.** Not the per-axis budget
idiom for this view. (The budget idiom stays correct where it already lives — `/api/timeline`'s
`columns`, `/api/floor`'s `max_steps` — and T-397 showed the *fold* composes on both axes already. This
decision is about how the big view's **tiles are addressed**, not how a fold is computed.)

**§5.2 Downsample timing — DECIDED: precomputed at seal time for the coarse zoom levels, on demand at
the live/finest edge**, with **client-side tile eviction** and a **tiles-in-flight cap** for bounded
browser memory. Storage cost accepted (140 GB free at the time of the decision). This matches how the
history pyramid already tiers, so the coarse levels are a seal-time product like every other tier, and
only the edge — where data is still arriving and cannot be precomputed — is computed on request.

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
