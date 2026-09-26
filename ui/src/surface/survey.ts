// The coverage survey the surface consults BEFORE it asks for a tile (T-580, docs/16 §8.4a).
//
// **Grey is the normal state of this canvas, not an edge case.** docs/16 measured 55–99 % of a
// 6 GHz canvas unobserved, and until this file the client fetched every tile of it anyway: T-461
// makes each such tile cheap on the server (the coverage map answers it without the history lock),
// but each was still its own round trip on its own connection, for an answer that was known in
// advance to be "the radio never looked here". This is the one request that knows it in advance:
// `GET /api/coverage` over the whole surface, asked once and re-asked on a slow cadence while
// something follows the live edge.
//
// # What a skip is allowed to mean — the honesty constraint
//
// A tile not fetched **because nothing was sampled** and a tile **not fetched yet** must stay two
// different things on screen, and "observed but not yet measured" and "unknown whether we looked"
// (T-441) must keep their own marks. So a skip is granted only on positive evidence, and every
// other answer is "the tile is still owed":
//
//   1. **Only `"unobserved"` skips.** `"unknown"` (we no longer know whether we looked), `"observed"`
//      and `"excluded"` (both sampled) and any state name this client does not know all keep the
//      tile — the same direction as `BiasTee::Unknown` is not `Off`.
//   2. **Unobserved at every instant UP TO the tile's end, not only inside it.** A tile over spectrum
//      the radio left carries the **last-known shadow** (T-519/T-520, ADR-0020) — a real measurement
//      from before the tile, drawn dimly over rows the coverage plane calls unobserved. Skipping such
//      a tile would turn the shadow into THE grey, which is a lie about a band that WAS swept. So the
//      survey reaches back to when recording began (`horizon.recording_began_s`), and a frequency
//      whose column holds any sampled cell in any row before the tile's end is not skippable.
//   3. **A survey that cannot see the whole past skips nothing.** If its window starts after
//      recording began, rows the shadow could come from are outside it. And a row straddling
//      `oldest_record_s` while the server has **lost** tune records (`forgotten`, or records between
//      `recording_began_s` and `oldest_record_s`) is only partly "wholly before the horizon", so the
//      server can call its cells unobserved over an interval it no longer has records for: such a
//      row blocks, exactly as `"unknown"` does.
//   4. **Grey only as far forward as the survey's evidence reaches** (`horizon.as_of_s`, T-532's
//      rule for tiles, applied to the survey). A skipped tile is drawn grey up to that instant and
//      is left as the pane's PENDING ground above it — "not known yet", which is true. The strip is
//      paid for by the next survey, not by a tile request, and the moment a survey says a column was
//      sampled the tile is asked for (`ui/test/surface-survey.test.ts`).
//
// The skipped region is drawn from a **state byte** (one `UNOBSERVED` cell, uploaded once), so the
// standing rule of ui/src/surface/surface.ts — grey comes out of a coverage state and nothing else —
// is unchanged: the state now comes from `/api/coverage` instead of from `/api/tiles`'s own coverage
// plane, and both are the same record-derived map (docs/api.md).
//
// Presentation only (ADR-0013 §1): nothing here decides what was observed; it reads the backend's
// four-state answer and refuses to fetch what that answer already settled.

import type { Box } from "./lattice";

const S_TO_NS = 1e9;

/** Frequency cells of the survey. With [[SURVEY_ROWS]] it stays within the route's 4096-cell cap;
 * over a 6 GHz surface one cell is ~2.9 MHz, so a tile narrower than that near a sampled band is
 * simply fetched (the conservative direction). */
export const SURVEY_CELLS = 2048;
/** Time rows. Two, so a tile in the past over a band first swept LATER can still be skipped, and a
 * horizon-straddling row blocks only half the window rather than all of it. */
export const SURVEY_ROWS = 2;
/** How often a surface that follows the live edge re-asks, ms. The only cost is one request. */
export const SURVEY_EVERY_MS = 2000;

/** The request this client builds for the survey. Asserted in ui/test (T-367: assert the request
 * the client builds, not only what it renders). */
export function surveyUrl(f0Hz: number, f1Hz: number, t0Ns: number, t1Ns: number,
  cells = SURVEY_CELLS, rows = SURVEY_ROWS): string {
  const q = new URLSearchParams({
    f_lo: String(Math.max(0, f0Hz)),
    f_hi: String(f1Hz),
    cells: String(cells),
    rows: String(rows),
    t0: String(t0Ns / S_TO_NS),
    t1: String(t1Ns / S_TO_NS),
  });
  return `/api/coverage?${q.toString()}`;
}

/** The slice of `GET /api/coverage` the survey reads. Structural, only what is used. */
export interface SurveyResponse {
  window?: { t0_s?: number; t1_s?: number } | null;
  grid?: { cells?: number; rows?: number; f_lo_hz?: number; f_cell_hz?: number; t0_s?: number; t_cell_s?: number } | null;
  any?: { cells?: ({ state?: string } | null)[] } | null;
  horizon?: {
    oldest_record_s?: number | null;
    recording_began_s?: number | null;
    forgotten?: string | null;
    as_of_s?: number | null;
  } | null;
}

/**
 * One decoded survey: a `cells × rows` grid of "may this be skipped" bits over `[f0, f1) × [t0, t1)`.
 *
 * `null` from [[decodeSurvey]] means *unreadable*, and an unreadable survey is not an answer: the
 * host then fetches every tile, exactly as before this file existed.
 */
export class Survey {
  constructor(
    readonly f0Hz: number,
    readonly cellHz: number,
    readonly cells: number,
    readonly t0Ns: number,
    readonly rowNs: number,
    readonly rows: number,
    /** Row-major `[t * cells + f]`: true only where the cell is `"unobserved"` and its row is not
     * blocked (rule 3 in the header). */
    private readonly never: Uint8Array,
    /** How far forward the survey's evidence reaches, ns. Grey is drawn no further. */
    readonly throughNs: number,
    /** False when the survey does not reach back to when recording began: then nothing is skipped. */
    readonly complete: boolean,
    /** The window a following survey should start at so it DOES reach back that far, ns. */
    readonly floorNs: number,
  ) {}

  get f1Hz(): number { return this.f0Hz + this.cells * this.cellHz; }

  /**
   * **May the tile over `box` go unfetched?** Returns the instant up to which the survey says the
   * tile's whole frequency span was never sampled — at any time up to the tile's end — or `null`
   * when the tile is owed (any other answer, anywhere, and a survey that cannot see the whole past).
   */
  unobservedThrough(box: Box): number | null {
    if (!this.complete) return null;
    if (!(box.f1Hz > box.f0Hz) || box.f0Hz < this.f0Hz || box.f1Hz > this.f1Hz) return null;
    const c0 = Math.max(0, Math.floor((box.f0Hz - this.f0Hz) / this.cellHz));
    const c1 = Math.min(this.cells - 1, Math.ceil((box.f1Hz - this.f0Hz) / this.cellHz) - 1);
    // Every row that begins before the tile ends: rule 2 — the past up to the tile, not the tile.
    const r1 = Math.min(this.rows - 1, Math.ceil((box.t1Ns - this.t0Ns) / this.rowNs) - 1);
    for (let r = 0; r <= r1; r++) {
      for (let c = c0; c <= c1; c++) if (!this.never[r * this.cells + c]) return null;
    }
    return this.throughNs;
  }
}

const finite = (v: unknown): v is number => typeof v === "number" && Number.isFinite(v);

/** Reads a `GET /api/coverage` answer into a [[Survey]], or `null` when it cannot be read. */
export function decodeSurvey(resp: SurveyResponse): Survey | null {
  const g = resp?.grid, w = resp?.window, h = resp?.horizon ?? null;
  const cells = g?.cells, rows = g?.rows, f0 = g?.f_lo_hz, df = g?.f_cell_hz, t0 = g?.t0_s, dt = g?.t_cell_s;
  if (!finite(cells) || !finite(rows) || !finite(f0) || !finite(df) || !finite(t0) || !finite(dt)) return null;
  if (!(cells > 0) || !(rows > 0) || !(df > 0) || !(dt > 0)) return null;
  const list = resp?.any?.cells;
  if (!Array.isArray(list) || list.length !== cells * rows) return null;
  const t1 = finite(w?.t1_s) ? w!.t1_s! : t0 + rows * dt;
  const began = finite(h?.recording_began_s) ? h!.recording_began_s! : null;
  const oldest = finite(h?.oldest_record_s) ? h!.oldest_record_s! : null;
  // Rule 3a: the survey must reach back to when recording began, or rows the shadow could come
  // from are outside it. `null` began is "nothing here ever recorded": then no past is missing.
  //
  // **T-982: the epsilon must scale with the timestamp, not be a flat 1e-9 SECONDS (1 ns).** `t0`
  // and `began` are Unix epoch seconds (~1.79e9), carried on the client as `SurfaceOrigin.edgeNs`
  // in NANOSECONDS — a plain JS `number`, so `~1.79e18` ns already exceeds `Number.MAX_SAFE_INTEGER`
  // (2^53 ≈ 9.007e15) by ~200x, and `maybeSurvey`'s own `t0Ns / S_TO_NS -> ...server... -> floorNs`
  // round trip (`preview.ts`) re-derives this exact `t0` from that representation on every retry.
  // Measured live (T-982, a second tab opening `/surface.html` while `/` already runs against the
  // same mock capture): `t0` and `began` came back **~200 ns apart** — description of the same
  // instant, corrupted by exactly the ns-as-float64 precision this file's own comment three lines
  // up does not anticipate. A flat 1 ns tolerance is over two orders of magnitude tighter than that
  // unavoidable loss, so `complete` read `false` FOREVER: same frozen `t0`/`began` every retry (a
  // historical surface's bounds never move), same false verdict every time, `setSurvey` never
  // called, and T-580's "coverage first" gate then refuses every tile forever — the exact
  // "0 tiles · N pending" that never resolves. `tilecache.ts`'s `closeEnough` already holds this
  // codebase's answer for a float64 timestamp compare: RELATIVE to magnitude, not a bare constant.
  // The same `1e-9` now scales with `began` (~1.8 s of slack at this magnitude) — nine orders of
  // magnitude past the measured 200 ns error, and still four orders tighter than the seconds-scale
  // gaps this rule exists to catch.
  const complete = began === null || t0 <= began + 1e-9 * Math.max(1, Math.abs(began));
  // Rule 3b: a row straddling the record horizon while records were LOST is partly a row the server
  // no longer has records for, and its cells can read "unobserved" over that part. It blocks.
  const lost = h?.forgotten != null || (began !== null && oldest !== null && oldest > began);
  const never = new Uint8Array(cells * rows);
  for (let r = 0; r < rows; r++) {
    const rt0 = t0 + r * dt, rt1 = rt0 + dt;
    const blocked = lost && oldest !== null && rt0 < oldest && oldest < rt1;
    if (blocked) continue;
    for (let c = 0; c < cells; c++) {
      if (list[r * cells + c]?.state === "unobserved") never[r * cells + c] = 1;
    }
  }
  // Rule 4: grey no further forward than this answer's own evidence. No stated horizon is "no record
  // touches this window", and then the window's end is where the claim stops.
  const asOf = finite(h?.as_of_s) ? Math.min(h!.as_of_s!, t1) : t1;
  const floor = began !== null ? Math.min(t0, began) : t0;
  return new Survey(f0, df, cells, t0 * S_TO_NS, dt * S_TO_NS, rows, never, asOf * S_TO_NS, complete, floor * S_TO_NS);
}
