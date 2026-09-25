// What the preview host needs to know before it can draw anything: **the lattice, the surface's
// extent, and where on it there is anything to look at** (T-450).
//
// Every number below came off a backend route. This file does arithmetic over them — which is the
// same class of work `ui/src/navigators.ts` does when it unions `ranges_hz` into an axis — and it
// decides nothing about the radio, the pyramid or a level.
//
// ## The orientation problem this file exists for
//
// T-437 measured the first screen of this surface at **99.4 % grey** before history accumulates,
// settling to 55.2 %. That is docs/16 §8 working exactly as designed — *the shape of what is not
// grey is the survey* — but a user who opens a preview onto 1 MHz – 6 GHz of nothing will read it
// as broken, and file it. Two things follow, and neither of them is "seed the view with a
// frequency we know about":
//
//  1. **Open where the radio actually looked**, asked of `GET /api/coverage` — the record-derived
//     observation map, not a band plan and not `frequency.current`. [[observedExtent]] is the
//     bounding box of the cells the backend reported as `observed`; opening there is a measurement
//     following a measurement, and on a server that sampled nothing it honestly finds nothing.
//  2. **Say the number.** [[orientationNote]] states what fraction of the surface was ever sampled
//     and where the view was opened, so a nearly-empty screen arrives already explained. A grey
//     screen with a sentence saying *"0.6 % of this surface was ever sampled"* is a finding; the
//     same screen without one is a bug report.
//
// The alternative — opening on a known band because we expect signals there — is the exploration
// -first rule inverted (CLAUDE.md: the known-signal database "is never the starting point"), so it
// is not on the table even for a preview.

import { spectrumExtent, type Range } from "../navigators";
import type { CenterGrid } from "../navigation";
import type { Box, Lattice } from "./lattice";
import { ANCHOR_SPAN_DB } from "./surface";
import type { FreqWindow } from "./panes";

/** The slice of `GET /api/navigation` this file reads. Optional throughout: a replay reports no
 * front end (`frequency: null`) and a server with no spectrum history reports no `time`. */
export interface NavigationSlice {
  frequency?: (CenterGrid & { max_live_span_hz?: number | null }) | null;
  time?: { latest_s?: number | null } | null;
}

/** The slice of a `GET /api/tiles` answer this file reads: the record horizon, which is global. */
export interface HorizonSlice {
  coverage?: { horizon?: { oldest_record_s?: number | null } | null } | null;
}

/** The slice of `GET /api/coverage` this file reads (`any` — the union over every front end). */
export interface CoverageSlice {
  grid?: {
    cells?: number; rows?: number;
    f_lo_hz?: number; f_cell_hz?: number;
    t0_s?: number; t_cell_s?: number;
  } | null;
  any?: {
    cells?: readonly { state?: string }[] | null;
    /**
     * T-964: the route's own **time-collapsed** census — `{cells, observed_cells, excluded_cells,
     * unobserved_cells, unknown_cells}` over the frequency axis, where a cell counts as sampled if
     * any row in the window sampled it. This is the survey question, and it is the only share a
     * sentence about the survey may quote: see [[orientationNote]].
     */
    bands?: {
      cells?: number;
      observed_cells?: number;
      excluded_cells?: number;
      unobserved_cells?: number;
      unknown_cells?: number;
    } | null;
  } | null;
  /**
   * The route's own display scale for this region (T-470): `range_db`, with a `normalisation` that
   * reads *"0 at `range_db.lo`, 1 at `range_db.hi`, linear in dB and clamped"* — which is the
   * surface shader's `(v - uLo) / (uHi - uLo)`, stated on the wire.
   */
  shade?: { range_db?: { lo?: number; hi?: number } | null } | null;
}

const S_TO_NS = 1e9;
const finite = (v: unknown): v is number => typeof v === "number" && Number.isFinite(v);

/** The whole frequency axis the view lattice can address: `cells` cells of the coarsest level,
 * twice over — the route's own rule is that the top level is "wide enough to put 1 MHz–6 GHz in
 * two tiles". Used only when the server reports no front end to take a range from. */
export const latticeSpanHz = (lat: Lattice): number => lat.f0Hz * 2 ** (lat.levelsF - 1) * lat.cells * 2;

/** Where each half of [[surfaceBounds]] came from, so the chrome can say it rather than imply it. */
export interface BoundsProvenance {
  readonly freq: string;
  readonly time: string;
}

export interface SurfaceOrigin {
  readonly bounds: Box;
  /**
   * The newest capture instant this preview will ever draw, **fixed for the session**.
   *
   * This is a *historical* view (T-450's hard scope: no dependency on T-439's live-edge tiles), so
   * the edge does not advance and no pane follows it. Freezing it here is what makes that
   * structural rather than disciplined: `PaneModel` derives a following pane's box from whatever
   * edge it is handed, and handing it the same one every frame is a view that cannot creep.
   */
  readonly edgeNs: number;
  readonly provenance: BoundsProvenance;
}

/**
 * The surface's extent: the device-available spectrum × the span of recorded history.
 *
 * Both axes prefer a backend number and fall back only when there is none, and the fallback is
 * *named* in [[BoundsProvenance]] — a bound the client invented must not be indistinguishable from
 * one the server reported.
 *
 * `horizonS` is `coverage.horizon.oldest_record_s` off any tile answer: the earliest instant either
 * tune history still holds a record for. Before it, coverage is `unknown` rather than grey, so it
 * is exactly the right floor for a view whose grey is load-bearing.
 */
export function surfaceBounds(
  nav: NavigationSlice | null | undefined,
  horizonS: number | null | undefined,
  lat: Lattice,
  nowS: number = Date.now() / 1000,
): SurfaceOrigin {
  const ext: Range | null = spectrumExtent(nav?.frequency ?? null);
  const freqSource = ext
    ? "device-available spectrum (GET /api/navigation frequency.ranges_hz)"
    : "no front end reported: the view lattice's own frequency axis";
  const f0Hz = ext ? ext.lo : 0;
  const f1Hz = ext ? ext.hi : latticeSpanHz(lat);

  const latest = nav?.time?.latest_s;
  const t1S = finite(latest) ? latest : nowS;
  const timeTop = finite(latest)
    ? "newest recorded capture time (GET /api/navigation time.latest_s)"
    : "no spectrum history reported: this client's clock";
  // One tile of the coarsest time level is the widest window the lattice can address in one step,
  // and it is the honest fallback floor: it claims no retention the server did not state.
  const fallbackSpanNs = lat.t0Ns * 2 ** (lat.levelsT - 1) * lat.cells;
  const horizonOk = finite(horizonS) && horizonS < t1S;
  const t0S = horizonOk ? horizonS : t1S - fallbackSpanNs / S_TO_NS;
  const timeFloor = horizonOk
    ? "record horizon (coverage.horizon.oldest_record_s)"
    : "no surviving tune record: one tile of the coarsest time level";

  return {
    bounds: { f0Hz, f1Hz, t0Ns: t0S * S_TO_NS, t1Ns: t1S * S_TO_NS },
    edgeNs: t1S * S_TO_NS,
    provenance: { freq: freqSource, time: `${timeTop}; back to the ${timeFloor}` },
  };
}

/** The request this client builds for the orientation coverage map. Asserted in ui/test, because a
 * contract test proves the *server* serves a route, never that the client asks the right thing. */
export function coverageUrl(box: Box, cells: number, rows: number): string {
  const q = new URLSearchParams({
    f_lo: String(Math.max(0, box.f0Hz)),
    f_hi: String(box.f1Hz),
    cells: String(cells),
    rows: String(rows),
    t0: String(box.t0Ns / S_TO_NS),
    t1: String(box.t1Ns / S_TO_NS),
  });
  return `/api/coverage?${q.toString()}`;
}

/** What the coverage map said about the whole surface. Counts, not a verdict. */
export interface CoverageCensus {
  readonly observed: number;
  readonly unobserved: number;
  readonly unknown: number;
  readonly total: number;
  /** The bounding box of the `observed` cells, or `null` when nothing here was ever sampled. */
  readonly box: Box | null;
  /**
   * **The same census with the time axis collapsed** (T-964): one count per *frequency* cell, where
   * a cell counts as sampled if any row in the window sampled it.
   *
   * A grid census counts (time × frequency) cells, and that is the wrong denominator for *"where has
   * this radio ever looked"*: a front end sees one window at a time, so a **finished** 1 MHz–6 GHz
   * survey pass occupies only a thin diagonal of the grid. T-964 measured a completed full-range
   * pass reported as *"4.3 % of this surface was ever sampled (176 of 4096 coverage cells)"* — true
   * of the 128 × 32 grid, and read (correctly, given the sentence) as *the survey lit nothing*.
   *
   * Taken from the route's own `any.bands` when it serves one, else folded here from the same cells.
   */
  readonly bands: {
    readonly observed: number;
    readonly unobserved: number;
    readonly unknown: number;
    readonly total: number;
  };
}

/**
 * The extent of what was actually sampled, from the `any` (union) plane.
 *
 * **The three states are counted separately and never summed for you** — `unknown` is *"the record
 * that would say whether we looked is gone"*, which is not *"nothing looked"*, and a preview that
 * folded them together would be making the very claim `/api/coverage` refuses to make.
 *
 * `box` covers only `observed` cells: `unknown` is not evidence that anything was measured there,
 * so opening the view on it would be opening on a guess.
 */
export function observedExtent(cov: CoverageSlice | null | undefined): CoverageCensus {
  const g = cov?.grid;
  const cells = cov?.any?.cells;
  const nf = finite(g?.cells) ? g!.cells! : 0;
  const nt = finite(g?.rows) ? g!.rows! : 0;
  const noBands = { observed: 0, unobserved: 0, unknown: 0, total: 0 } as const;
  const empty: CoverageCensus = { observed: 0, unobserved: 0, unknown: 0, total: 0, box: null, bands: noBands };
  if (!Array.isArray(cells) || nf <= 0 || nt <= 0 || cells.length !== nf * nt) return empty;
  const fLo = finite(g?.f_lo_hz) ? g!.f_lo_hz! : 0;
  const fCell = finite(g?.f_cell_hz) ? g!.f_cell_hz! : 0;
  const t0 = finite(g?.t0_s) ? g!.t0_s! : 0;
  const tCell = finite(g?.t_cell_s) ? g!.t_cell_s! : 0;
  let observed = 0, unobserved = 0, unknown = 0;
  let f0 = Infinity, f1 = -Infinity, t0i = Infinity, t1i = -Infinity;
  // T-964: the time-collapsed state per frequency cell, ranked so the strongest claim survives the
  // fold — sampled beats forgotten beats grey, the route's own `bands` ordering.
  const rank = new Uint8Array(nf);
  for (let i = 0; i < cells.length; i++) {
    const s = cells[i]?.state;
    const f = i % nf, t = Math.floor(i / nf);
    if (s === "unobserved") { unobserved++; continue; }
    if (s === "unknown") { unknown++; rank[f] = Math.max(rank[f], 1); continue; }
    // T-595: `"excluded"` is sampled spectrum (the DC notch), so it counts towards the observed
    // extent the view opens on — the radio was demonstrably there.
    if (s !== "observed" && s !== "excluded") continue;
    observed++;
    rank[f] = 2;
    if (f < f0) f0 = f;
    if (f > f1) f1 = f;
    if (t < t0i) t0i = t;
    if (t > t1i) t1i = t;
  }
  // The route's own count when it serves one (T-964), else the fold above of the same cells: one
  // sentence, one rule, whichever end computed it.
  const served = cov?.any?.bands;
  const n = (v: unknown) => (finite(v) ? v : 0);
  const bands = finite(served?.cells) && served!.cells! > 0
    ? {
      observed: n(served!.observed_cells) + n(served!.excluded_cells),
      unobserved: n(served!.unobserved_cells),
      unknown: n(served!.unknown_cells),
      total: served!.cells!,
    }
    : {
      observed: rank.reduce((a, r) => a + (r === 2 ? 1 : 0), 0),
      unobserved: rank.reduce((a, r) => a + (r === 0 ? 1 : 0), 0),
      unknown: rank.reduce((a, r) => a + (r === 1 ? 1 : 0), 0),
      total: nf,
    };
  const box: Box | null = observed > 0 && fCell > 0 && tCell > 0
    ? {
      f0Hz: fLo + f0 * fCell,
      f1Hz: fLo + (f1 + 1) * fCell,
      t0Ns: (t0 + t0i * tCell) * S_TO_NS,
      t1Ns: (t0 + (t1i + 1) * tCell) * S_TO_NS,
    }
    : null;
  return { observed, unobserved, unknown, total: cells.length, box, bands };
}

/**
 * **The colour scale's anchor** (T-470): the display range the backend measured over a region,
 * adopted as-is.
 *
 * This is the whole of the fix for *"colours animate and shift when I zoom"*. The renderer needs a
 * `(lo, hi)` to turn a dB into a colour, and it used to compute one every frame from the tiles that
 * happened to be on screen — so navigating re-coloured measurements that had not changed. The
 * replacement has to be a range that is **not a function of the viewport**, and `GET /api/coverage`
 * already answers exactly that for a region, in the tiles' own unit, in an answer this client
 * **already fetches at open**: `shade.range_db`, whose `shade.normalisation` is the shader's
 * arithmetic written out. So the anchor costs no request, invents no number, and is measured once
 * over the region rather than continuously over the window.
 *
 * **Only the top is adopted; the span is stated** — see [[ANCHOR_SPAN_DB]] for why, and for the
 * measurement that forced it. In one line: the fold is max-hold, so `range_db.hi` is exactly the
 * region's maximum at any resolution, while `range_db.lo` is a *minimum of maxima* that folding can
 * only raise, so it is an upper bound on the floor and anchoring there clips real measurement to
 * black. `lo` is still read, because a reported range is how this client knows the answer carried a
 * measurement at all.
 *
 * `null` when no range was reported — which is very nearly the same condition as *nothing here was
 * ever observed*, since the range is the observed minimum and maximum. The caller states its own
 * fallback rather than being handed one that looks like a measurement.
 */
export function shadeRange(
  cov: CoverageSlice | null | undefined,
  where: string,
): { readonly lo: number; readonly hi: number; readonly source: string } | null {
  const r = cov?.shade?.range_db;
  const lo = r?.lo, hi = r?.hi;
  if (!finite(lo) || !finite(hi) || !(hi > lo)) return null;
  return {
    lo: hi - ANCHOR_SPAN_DB,
    hi,
    source: `${ANCHOR_SPAN_DB} dB below the peak measured once over ${where} `
      + "(GET /api/coverage shade.range_db.hi, which max-hold makes exact at any resolution)",
  };
}

/** A pane's opening window: a frequency centre/span and an absolute time centre/span. */
export interface OpeningWindow {
  readonly freq: FreqWindow;
  readonly centerNs: number;
  readonly spanNs: number;
  /** `true` when it came from observed coverage rather than from the whole surface. */
  readonly onCoverage: boolean;
}

/**
 * Where to open the first pane: on the observed box with a margin, or on the whole surface when
 * nothing was observed.
 *
 * The margin is deliberate and small: it puts the *edge* of coverage on screen, so the boundary
 * between what was sampled and what was not is visible from the first frame rather than being
 * something the user has to pan to discover.
 */
export function openingWindow(bounds: Box, observed: Box | null, margin = 0.2): OpeningWindow {
  const full: OpeningWindow = {
    freq: { centerHz: (bounds.f0Hz + bounds.f1Hz) / 2, spanHz: bounds.f1Hz - bounds.f0Hz },
    centerNs: (bounds.t0Ns + bounds.t1Ns) / 2,
    spanNs: bounds.t1Ns - bounds.t0Ns,
    onCoverage: false,
  };
  if (!observed || !(observed.f1Hz > observed.f0Hz) || !(observed.t1Ns > observed.t0Ns)) return full;
  const fPad = (observed.f1Hz - observed.f0Hz) * margin;
  const tPad = (observed.t1Ns - observed.t0Ns) * margin;
  return {
    freq: {
      centerHz: (observed.f0Hz + observed.f1Hz) / 2,
      spanHz: Math.min(observed.f1Hz - observed.f0Hz + 2 * fPad, bounds.f1Hz - bounds.f0Hz),
    },
    centerNs: (observed.t0Ns + observed.t1Ns) / 2,
    spanNs: Math.min(observed.t1Ns - observed.t0Ns + 2 * tPad, bounds.t1Ns - bounds.t0Ns),
    onCoverage: true,
  };
}

const pct = (n: number, d: number) => (d > 0 ? (100 * n) / d : 0);

/** A share, written so a very small one is still a number rather than "0 %" (T-437's 0.6 %). */
export function fmtShare(n: number, d: number): string {
  const p = pct(n, d);
  if (p === 0) return "0 %";
  if (p < 0.1) return `${p.toFixed(3)} %`;
  if (p < 10) return `${p.toFixed(1)} %`;
  return `${p.toFixed(0)} %`;
}

/**
 * The sentence a nearly-empty first screen needs, **before** the user decides it is broken.
 *
 * It states the measured share, says which of the three coverage states the emptiness is, and says
 * where the view was opened — because T-437's 99.4 % is a true statement about a young pyramid and
 * the only defect available here is failing to say so.
 *
 * ## The share must answer the question the sentence asks (T-964)
 *
 * *"Ever sampled"* is a question about the **frequency axis**: did the radio ever look here in this
 * window. The sentence used to quote the share of (time × frequency) **cells**, which answers a
 * different question — *was it being looked at then* — and the two differ by the whole shape of a
 * survey. A front end sees one window at a time, so a sweep is a thin diagonal across the grid: a
 * **completed** 1 MHz–6 GHz pass came back as *"4.3 % of this surface was ever sampled (176 of 4096
 * coverage cells)"*, which is true of the grid and false of the survey, and read as the fog-of-war
 * map having nothing to show after the one feature that fills it.
 *
 * So both numbers are stated, each against its own question: the frequency share is what the survey
 * achieved, the cell share is what the surface draws. Neither is dropped — a user who reads 100 %
 * and then sees a mostly-grey canvas needs the second number to know why, which is the same honesty
 * rule the sentence exists for.
 */
export function orientationNote(c: CoverageCensus, opened: OpeningWindow): string {
  if (c.total === 0) {
    return "No coverage map was returned for this surface, so nothing here says whether the radio "
      + "ever looked. The view opens on the whole surface; anything drawn came from the pyramid.";
  }
  const b = c.bands;
  const sampled = `${fmtShare(b.observed, b.total)} of this surface was ever sampled`;
  // What the survey achieved, then what the canvas draws — a sweep occupies one window at a time,
  // so the second share is always the smaller one and saying why is the point.
  const counts = `${b.observed} of ${b.total} frequency cells ever sampled in this window; `
    + `${fmtShare(c.observed, c.total)} of its ${c.total} time × frequency cells, which is what the `
    + `surface draws — a radio sees one window at a time, so even a finished pass is a thin diagonal`;
  const forgotten = b.unknown > 0
    ? ` A further ${fmtShare(b.unknown, b.total)} of the frequency axis is past the record horizon — hatched, not grey: we no longer know whether we looked.`
    : "";
  if (!opened.onCoverage) {
    return `${sampled} (${counts}), so the view opens on the whole `
      + `surface and is mostly grey. Grey is the claim that nothing ever looked there — it is not a `
      + `loading state and not a failure.${forgotten}`;
  }
  return `${sampled} (${counts}). The view opens on the observed `
    + `region with a margin, so the edge of coverage is on screen: inside it is measurement, outside `
    + `it is grey, and grey means nothing ever looked there.${forgotten}`;
}
