// Review render (T-152; ADR-0013 §3.3, §4.3). While the capture timeline reviews a past instant,
// the waterfall shows `GET /api/history` over the live band instead of live rows. This module is
// pure: the query, the response check, and the grid → texture-row mapping. The mapping takes the max
// of the served `max_db` cells under each texel; `null` cells become UNOBSERVED_DB, drawn grey and
// never as quiet. Values are the server's, only resampled for display.
import type * as ax from "../../axis";
import { UNOBSERVED_DB } from "../../waterfall";
import type { TimeCursor } from "../capture/slice";

export interface HistoryGrid {
  f_lo_hz: number; f_cell_hz: number; nf: number; t0_s: number; t_cell_s: number; nt: number;
  max_db: readonly (number | null)[];
}

/** Cursor equality for `store.select` (a new `{live: true}` object is the same cursor). */
export const sameCursor = (a: TimeCursor, b: TimeCursor) => (a.live ? b.live : !b.live && a.tS === b.tS);

/** The time window ending at `tS` that fills `rows` rows of `rowPeriodS` (at least 1 s). */
export function historyWindow(tS: number, rows: number, rowPeriodS: number): { t0: number; t1: number } {
  return { t0: tS - Math.max(1, rows * rowPeriodS), t1: tS };
}

/** Cell budget: about one cell per texel (≤ 2048 columns) × the ring's rows, capped as the old
 * history panel caps it. */
export const historyMaxCells = (texW: number, rows: number) =>
  Math.min(500_000, Math.min(2048, Math.max(1, texW)) * Math.max(1, rows));

export function historyQuery(full: ax.View, t0: number, t1: number, maxCells: number): string {
  const q = new URLSearchParams({
    f_lo: String(Math.max(0, Math.floor(full.loHz))), f_hi: String(Math.ceil(full.hiHz)),
    t0: String(t0), t1: String(t1), max_cells: String(Math.floor(maxCells)),
  });
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
 * Texture rows (oldest first, the newest `maxRows` only) for a grid over the displayed band `full`:
 * texel j covers `full.loHz + [j, j+1)·(full span / texW)` and takes the max of the observed cells
 * it overlaps (at least the cell under it); none observed, or outside the grid → UNOBSERVED_DB.
 * `times[k]` is row k's start time.
 */
export function historyRows(grid: HistoryGrid, full: ax.View, texW: number, maxRows: number): { rows: Float32Array[]; times: number[] } {
  const { nf, nt, f_lo_hz: f0, f_cell_hz: fc, max_db: cells } = grid;
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
  for (let r = Math.max(0, nt - Math.max(0, maxRows)); r < nt; r++) {
    const row = new Float32Array(texW), base = r * nf;
    for (let j = 0; j < texW; j++) {
      let m = -Infinity;
      for (let c = c0[j]; c <= c1[j]; c++) {
        const v = cells[base + c];
        if (typeof v === "number" && v > m) m = v;
      }
      row[j] = m === -Infinity ? UNOBSERVED_DB : m;
    }
    rows.push(row);
    times.push(grid.t0_s + r * grid.t_cell_s);
  }
  return { rows, times };
}
