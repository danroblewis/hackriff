// Review render (T-152; ADR-0013 §3.3, §4.3). While the capture timeline reviews a past instant,
// the waterfall shows `GET /api/history` over the live band instead of live rows. This module is
// pure: the query, the response check, and the grid → texture-row mapping.
//
// T-334 — span-matched resolution (CLAUDE.md "Time, the waterfall, and the live view", invariant
// 4). The request now states the view's own budgets: `max_t` = the rows it will draw, `max_f` =
// the texels across. The backend picks the pyramid level, and it errs coarser than the view, so
// the mapping below normally **replicates** one served cell across several texels/rows rather than
// reducing several into one. Deciding which of several values stands for a pixel would be a
// measurement, and measurements belong in the backend; `resolution.over_resolved` is where the
// server says it could not meet a budget, and that is the only case where the loop reduces.
//
// Row times are read from the grid the server described (`t0_s` is row 0's start, every row is
// `t_cell_s` long — docs/api.md "Row times are contract, not inference"), never guessed from a
// row rate or a wall clock.
//
// T-420 — the span fills the waterfall (CLAUDE.md "Time is zoomable; the waterfall scales to the
// selected span": *"zooming in time re-scales rather than truncates"*). `historyRows` used to emit
// **one texture row per served time cell**, and `Waterfall.setRows` writes those into a ring of
// `WATERFALL_ROWS` whose whole height is always drawn. The pyramid's finest time cell is 1 s, so a
// 20 s window served 21 rows into 512 and painted 4 % of the pane, with black under it; a 10 min
// drag fell to the 60 s tier and painted 2 %. The wider the drag, the thinner the sliver — the
// truncation the invariant names.
//
// It now emits **exactly the rows the view will draw**, laid across the grid's own served extent.
// Where a measured cell is longer than a drawn row (the normal case, since the ladder is discrete
// and errs coarser) that one measured value **repeats** across the rows it covers — T-342's
// `replicated`, and the only honest way to fill: nothing here interpolates between two cells or
// stretches a value over time it was not measured in, and [[historyDetailText]] says out loud how
// many rows each measured cell was drawn across. Where the view is coarser than the grid
// (`resolution.over_resolved` names `max_t`), several cells fold into one row by the same max-hold
// the backend folds with — the one case in which this reduces, exactly as the frequency axis
// beside it already does.
import type * as ax from "../../axis";
import { UNOBSERVED_DB } from "../../waterfall";
import type { TimeCursor } from "../capture/slice";
import { durationText } from "../capture/timeline";

/** The `resolution` block T-334/T-341 serve beside the grid; the fields the render reads. */
export interface HistoryResolution {
  source?: string;
  statement?: string;
  over_resolved?: readonly string[];
}

export interface HistoryGrid {
  f_lo_hz: number; f_cell_hz: number; nf: number; t0_s: number; t_cell_s: number; nt: number;
  max_db: readonly (number | null)[];
  resolution?: HistoryResolution;
}

/** Cursor equality for `store.select` (a new `{live: true}` object is the same cursor). */
export const sameCursor = (a: TimeCursor, b: TimeCursor) =>
  (a.live ? b.live : !b.live && a.tS === b.tS && (a.spanS ?? null) === (b.spanS ?? null));

/**
 * The time window ending at `tS`: `spanS` when the cursor asked for one (T-340 — a region dragged
 * on the time navigator zooms the waterfall to exactly that span), else the span the rows on
 * screen cover at their own period (at least 1 s).
 *
 * `spanS` is never defaulted here. A null span is "nothing asked for a span", and the fallback is
 * measured from the view's own rows rather than from a constant duration.
 *
 * T-420: that fallback is the relationship the default has to keep — `rows / rows_per_s`, the span
 * the waterfall's height is worth at the live row rate, so the default window fills the pane for the
 * same reason a dragged one does. It is a *window*, not a resolution: `max_t` on the request
 * (below) is the row budget, and [[historyRows]] lays whatever comes back across all of those rows.
 */
export function historyWindow(tS: number, rows: number, rowPeriodS: number, spanS: number | null = null): { t0: number; t1: number } {
  const span = spanS !== null && Number.isFinite(spanS) && spanS > 0 ? spanS : Math.max(1, rows * rowPeriodS);
  return { t0: tS - span, t1: tS };
}

/** Cell budget: about one cell per texel (≤ 2048 columns) × the ring's rows, capped as the old
 * history panel caps it. */
export const historyMaxCells = (texW: number, rows: number) =>
  Math.min(500_000, Math.min(2048, Math.max(1, texW)) * Math.max(1, rows));

/**
 * The query for a span. `maxCells` still bounds the response; `maxT`/`maxF` (T-334) are the rows
 * and texels this view will draw, so the backend serves the span at a resolution matched to it
 * instead of returning a grid of some other shape that happens to fit the same product.
 */
export function historyQuery(
  full: ax.View, t0: number, t1: number, maxCells: number, maxT?: number, maxF?: number,
): string {
  const q = new URLSearchParams({
    f_lo: String(Math.max(0, Math.floor(full.loHz))), f_hi: String(Math.ceil(full.hiHz)),
    t0: String(t0), t1: String(t1), max_cells: String(Math.floor(maxCells)),
  });
  if (maxT !== undefined && maxT >= 1) q.set("max_t", String(Math.floor(maxT)));
  if (maxF !== undefined && maxF >= 1) q.set("max_f", String(Math.floor(maxF)));
  return `/api/history?${q}`;
}

/** The grid fields the render uses; null when missing or inconsistent (`max_db.length ≠ nt·nf`). */
export function parseHistory(body: unknown): HistoryGrid | null {
  const b = body as Partial<HistoryGrid> | null;
  if (!b) return null;
  const nums = [b.f_lo_hz, b.f_cell_hz, b.nf, b.t0_s, b.t_cell_s, b.nt];
  if (!nums.every((x) => typeof x === "number" && Number.isFinite(x))) return null;
  if (!(b.f_cell_hz! > 0) || !(b.t_cell_s! > 0) || b.nf! < 0 || b.nt! < 0) return null;
  if (!Array.isArray(b.max_db) || b.max_db.length !== b.nt! * b.nf!) return null;
  return b as HistoryGrid;
}

/**
 * How a served grid is laid across the rows the view draws (T-420) — the whole time-axis
 * relationship, in one place, so the render, the row period and the label cannot disagree.
 *
 * The span is the grid's **served** extent (`nt · t_cell_s` from `t0_s`), not the one requested:
 * the pyramid's cells are global, so a request is snapped outwards to cell boundaries and the
 * picture drawn is of the served window. That is the same "snap to the nearest achievable state"
 * the navigation invariant asks for, and it is what fills the display.
 */
export interface HistoryFill {
  /** Rows the view will draw — the waterfall ring's height. The span is spread over all of them. */
  rows: number;
  /** Start of the served extent, s (the grid's own `t0_s`). */
  t0S: number;
  /** The served extent, s: `nt · t_cell_s`. This is what now fills the waterfall's height. */
  spanS: number;
  /** One drawn row's duration, s (`spanS / rows`) — a display mapping, never a measured period. */
  rowDurS: number;
  /** Measured time cells behind those rows. */
  cells: number;
  /** Rows one measured cell is drawn across (`rows / cells`); ≤ 1 when the view is the coarser. */
  rowsPerCell: number;
  /** Fewer measured cells than rows: one measured value **repeats** across rows (T-342). */
  replicated: boolean;
}

/** [[HistoryFill]] for `grid` drawn into `rows` texture rows; null when there is nothing to lay. */
export function historyFill(grid: HistoryGrid, rows: number): HistoryFill | null {
  const r = Math.max(0, Math.floor(rows));
  if (!(r >= 1) || !(grid.nt >= 1) || !(grid.t_cell_s > 0)) return null;
  const spanS = grid.nt * grid.t_cell_s;
  return {
    rows: r, t0S: grid.t0_s, spanS, rowDurS: spanS / r, cells: grid.nt,
    rowsPerCell: r / grid.nt, replicated: grid.nt < r,
  };
}

/**
 * What the drawn waterfall actually is, said on screen (T-420's honesty constraint): the span it
 * now fills with, which tier answered, and — when the display has more rows than there were
 * measured cells — that one measured value was **repeated** across N rows rather than that N rows
 * were measured. A long span filled from a coarse tier is honest *provided it says so*; the same
 * span filled by stretching a sliver is not, which is why this sits beside the fill it describes.
 */
export function historyDetailText(grid: HistoryGrid, rows: number): string {
  const f = historyFill(grid, rows);
  if (!f) return "";
  const src = grid.resolution?.source === "survey-overview" ? "survey overview" : "spectrum-history overview";
  const cs = f.cells === 1 ? "cell" : "cells";
  const how = f.replicated
    ? `${fmtCell(grid.t_cell_s)} ${cs} repeated across ${Math.round(f.rowsPerCell * 10) / 10} rows`
    : f.cells > f.rows
      ? `${fmtCell(grid.t_cell_s)} ${cs}, ${Math.round(f.cells / f.rows * 10) / 10} folded per row (max-hold)`
      : `${fmtCell(grid.t_cell_s)} ${cs}, one per row`;
  return `${durationText(f.spanS)} · ${src} · ${how}`;
}

/** A measured cell's duration in words — [[durationText]], which keeps whole seconds, plus the
 * sub-second range a finer pyramid scheme could serve. */
const fmtCell = (s: number) => (s < 1 ? `${Math.round(s * 1000)} ms` : durationText(s));

/**
 * Texture rows (oldest first) for a grid over the displayed band `full`, **exactly `maxRows` of
 * them** so the served span fills the waterfall's height (T-420) instead of painting `nt` rows into
 * a ring of `maxRows` and leaving the rest black.
 *
 * Texel j covers `full.loHz + [j, j+1)·(full span / texW)`; row k covers
 * `[t0_s + k·rowDur, t0_s + (k+1)·rowDur)` of the served extent. A cell is the max of the observed
 * cells the row–texel rectangle overlaps (at least the one under it); none observed, or outside the
 * grid → UNOBSERVED_DB. `times[k]` is row k's start on the capture clock, from the served grid's own
 * `t0_s`/`t_cell_s` and the row count — never a row rate or a wall clock (ADR-0017 TM-1).
 *
 * **Nothing is stretched or interpolated.** Normally `maxRows > nt`, and a row then takes the cell
 * its own midpoint falls in — never a blend of the two either side of a cell boundary — so the rows
 * covering a cell are the *same* measured values repeated, on a contiguous run of rows — and, to
 * keep the cost at `nt · texW` rather than `maxRows · texW`, literally the same array pushed again
 * (`Waterfall.setRows` copies before uploading). The only case that reduces is the view being the
 * coarser grid (`resolution.over_resolved` names `max_t`), where several cells fold into a row by
 * the backend's own max-hold — the same rule the frequency loop above already applies, and never a
 * truncation of the window.
 */
export function historyRows(grid: HistoryGrid, full: ax.View, texW: number, maxRows: number): { rows: Float32Array[]; times: number[] } {
  const { nf, nt, f_lo_hz: f0, f_cell_hz: fc, max_db: cells } = grid;
  const fill = historyFill(grid, maxRows);
  if (!fill) return { rows: [], times: [] };
  const du = (full.hiHz - full.loHz) / texW;
  const c0 = new Int32Array(texW), c1 = new Int32Array(texW);
  for (let j = 0; j < texW; j++) {
    const lo = Math.floor((full.loHz + j * du - f0) / fc);
    const hi = Math.max(lo, Math.ceil((full.loHz + (j + 1) * du - f0) / fc) - 1);
    if (hi < 0 || lo > nf - 1) { c0[j] = 0; c1[j] = -1; continue; }
    c0[j] = Math.max(0, lo);
    c1[j] = Math.min(nf - 1, hi);
  }
  const rows: Float32Array[] = [], times: number[] = [];
  const per = fill.rowDurS / grid.t_cell_s; // measured cells spanned by one drawn row
  const clamp = (r: number) => Math.min(nt - 1, Math.max(0, r));
  let prev0 = -1, prev1 = -1, row: Float32Array | null = null;
  for (let k = 0; k < fill.rows; k++) {
    // Rows finer than cells (the normal case): the cell the row's own midpoint falls in, so one
    // measured value repeats cleanly across the rows it covers and no row is a blend of two.
    // Rows coarser than cells (`over_resolved`): every cell the row overlaps, folded by max-hold.
    const r0 = per <= 1 ? clamp(Math.floor((k + 0.5) * per)) : clamp(Math.floor(k * per));
    const r1 = per <= 1 ? r0 : Math.min(nt - 1, Math.max(r0, Math.ceil((k + 1) * per) - 1));
    if (row === null || r0 !== prev0 || r1 !== prev1) {
      row = new Float32Array(texW);
      for (let j = 0; j < texW; j++) {
        let m = -Infinity;
        for (let r = r0; r <= r1; r++) {
          const base = r * nf;
          for (let c = c0[j]; c <= c1[j]; c++) {
            const v = cells[base + c];
            if (typeof v === "number" && v > m) m = v;
          }
        }
        row[j] = m === -Infinity ? UNOBSERVED_DB : m;
      }
      prev0 = r0;
      prev1 = r1;
    }
    rows.push(row);
    times.push(fill.t0S + k * fill.rowDurS);
  }
  return { rows, times };
}
