# ADR-0020 — A fourth honesty tier: **last-known / stale** (the canvas's shadow), with grey's meaning unchanged

**Status:** PROVISIONAL. The *model* is the user's (2026-09-18, T-519, planning log B0.691) and is recorded, not proposed. What is proposed is where the value comes from and how it goes on the wire. A note, not a gate.

**Touches:** CLAUDE.md "The view" (the honesty tiers; grey = genuinely unobserved); ADR-0017 §1 and ADR-0019 (a signal's time extent — unchanged); `docs/16` §4; `docs/api.md` `GET /api/tiles` → `shadow`; `hk_store::Pyramid::last_known_search`, `hk_store::LastKnown::carry_forward`; `hk-api::tiles`; the client's `ui/src/surface/cellrule.ts` (T-520). Use case AWARE-011.

---

## Context

The canvas had three honesty tiers — **live-IQ detail**, **spectrum-history**, **survey overview** — plus grey for *genuinely unobserved*, and T-441's two further marks (*observed but not yet measured*, *unknown whether we looked*). A cell with no measurement **for the viewed (t, f)** rendered grey, even where the radio had swept that band a minute before and then left. The user wants that cell to show the band's **most-recent-known** spectrum, dimly: swept then departed = shadow; never swept = grey; re-swept = bright again.

The pyramid held recency (coverage's `Sampled.last`) and strongest-ever (`overview()`'s max-hold), but not the value last seen.

## Decision

1. **Last-known / stale is a tier of its own.** A shadow value is a real measurement **of an earlier time**, carried forward. It is never a measurement of the cell it is drawn on, so it must be unmistakable from any live tier (T-520 owns the look), and it always travels with **when** it was last true (`last_t_s`, absolute capture time) and **at what resolution** (the source level's cell, stated per run).
2. **Grey's meaning is unchanged.** Grey is still *nothing ever looked here that any retained record or measurement can show*, and it is still decided by the coverage plane alone. The shadow is drawn only where coverage says `unobserved`; a cell with no shadow value stays grey. `unknown` (T-423) and *observed-not-yet-measured* (T-441) keep their own marks. The shadow never replaces a measurement: a cell whose grid holds a value carries no shadow.
3. **Computed at query time, from the pyramid that exists.** No new maintained structure, and nothing on the capture thread (the T-453 constraint). `Pyramid::last_known_search` walks newest-first, fine-to-coarse through the ladder (seconds → minutes → quarter-hours → hours → days), each stage covering only the part of the past the finer one did not, so the whole retained horizon costs a few hundred rows per column; it stops when every column has a value or nothing older is held. Within a tile the value is then carried down the rows and replaced by the tile's own value wherever the tile measures one.
4. **Never carry backward, and say what was not searched.** A coarse cell straddling the instant is used only where the tile's own grid proves its after-part empty; a window over budget, or whose coarse cell is not yet folded, is reported as *unsearched*, never as empty. Nothing is carried past the store's newest frame.

## Consequences

- **The shadow is only as deep as retention.** A band whose last measurement the byte budget has evicted at every level shows grey, although *coverage* records may still say it was looked at. That is honest about the value and conservative about the claim; a "looked, value no longer held" mark is a possible later refinement, not part of this decision.
- **Frequency resolution degrades with age**, because the ladder is welded: a value found at the day level is a 100 kHz cell replicated across finer columns (T-334's safe direction), and the wire says so.
- **Cost is bounded by the tile read's own budgets** (one lock hold per step, at most `TILE_MAX_TOTAL_SOURCE_CELLS` per search), so it can at most double a tile's work; over spectrum no tile holds, it answers from the tile index without reading a cell.
- Option (b) of the planning brief — a maintained per-frequency last-value structure — is deliberately not built; it is the fallback if query-time cost ever measures too high.
