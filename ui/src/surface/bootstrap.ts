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
  any?: { cells?: readonly { state?: string }[] | null } | null;
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
  const empty: CoverageCensus = { observed: 0, unobserved: 0, unknown: 0, total: 0, box: null };
  if (!Array.isArray(cells) || nf <= 0 || nt <= 0 || cells.length !== nf * nt) return empty;
  const fLo = finite(g?.f_lo_hz) ? g!.f_lo_hz! : 0;
  const fCell = finite(g?.f_cell_hz) ? g!.f_cell_hz! : 0;
  const t0 = finite(g?.t0_s) ? g!.t0_s! : 0;
  const tCell = finite(g?.t_cell_s) ? g!.t_cell_s! : 0;
  let observed = 0, unobserved = 0, unknown = 0;
  let f0 = Infinity, f1 = -Infinity, t0i = Infinity, t1i = -Infinity;
  for (let i = 0; i < cells.length; i++) {
    const s = cells[i]?.state;
    if (s === "unobserved") { unobserved++; continue; }
    if (s === "unknown") { unknown++; continue; }
    if (s !== "observed") continue;
    observed++;
    const f = i % nf, t = Math.floor(i / nf);
    if (f < f0) f0 = f;
    if (f > f1) f1 = f;
    if (t < t0i) t0i = t;
    if (t > t1i) t1i = t;
  }
  const box: Box | null = observed > 0 && fCell > 0 && tCell > 0
    ? {
      f0Hz: fLo + f0 * fCell,
      f1Hz: fLo + (f1 + 1) * fCell,
      t0Ns: (t0 + t0i * tCell) * S_TO_NS,
      t1Ns: (t0 + (t1i + 1) * tCell) * S_TO_NS,
    }
    : null;
  return { observed, unobserved, unknown, total: cells.length, box };
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
 */
export function orientationNote(c: CoverageCensus, opened: OpeningWindow): string {
  if (c.total === 0) {
    return "No coverage map was returned for this surface, so nothing here says whether the radio "
      + "ever looked. The view opens on the whole surface; anything drawn came from the pyramid.";
  }
  const sampled = `${fmtShare(c.observed, c.total)} of this surface was ever sampled`;
  const forgotten = c.unknown > 0
    ? ` A further ${fmtShare(c.unknown, c.total)} is past the record horizon — hatched, not grey: we no longer know whether we looked.`
    : "";
  if (!opened.onCoverage) {
    return `${sampled} (${c.observed} of ${c.total} coverage cells), so the view opens on the whole `
      + `surface and is mostly grey. Grey is the claim that nothing ever looked there — it is not a `
      + `loading state and not a failure.${forgotten}`;
  }
  return `${sampled} (${c.observed} of ${c.total} coverage cells). The view opens on the observed `
    + `region with a margin, so the edge of coverage is on screen: inside it is measurement, outside `
    + `it is grey, and grey means nothing ever looked there.${forgotten}`;
}
