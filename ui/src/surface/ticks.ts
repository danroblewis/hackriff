// Axis ticks for the surface's chrome readout (T-459, docs/16 §8.5a).
//
// T-445 FINDING 3: the retired surfaces drew labelled axis ticks; the cutover replaced them with the
// chrome's one-line-per-viewport readout, which states the two EDGES of a pane's window and nothing
// between them. That is a READOUT, not a RULER — it says where the window starts and stops but gives
// no intermediate marks to read a feature's frequency or age off the screen.
//
// **Why this is not "just add ticks".** The axes are independently levelled (T-434/T-438/T-440) and
// each pane has its own window, so ticks are per-pane and per-axis. The interval has to come from
// **the level actually drawn** — the same `(cellHz, cellS)` the chrome already reports off the
// `PaneReport` (`panes.ts`'s `paneStatuses()`), never a second calculation over the raw window. A
// tick interval derived any other way would be a second opinion about the view, which is the drift
// family this milestone spent itself closing (§8.5a's "same ramp, same scale, stated level").
//
// **Honesty applies to the interval, not just the edges.** A tick finer than the cell size the pane
// drew would imply the hardware resolved something it did not — the same rule that keeps a wide zoom
// from faking detail (CLAUDE.md's discretized-navigation invariant). So the chosen step is always
// `>= cellHz` / `>= cellS`, never narrower, even when that means fewer ticks than the target count.
//
// This module is pure arithmetic and string formatting — no DOM, no canvas — so it is unit-tested
// directly (ADR-0013 §6: canvas/WebGL/audio are never tested headless; this has none of those).
// `chrome.ts` turns its output into a sentence on the row; nothing here touches a pixel.

import { fmtSpan } from "./panes";

export interface AxisTick {
  /** Hz for a frequency tick, absolute capture-time ns for a time tick. */
  readonly value: number;
  readonly label: string;
}

/** Roughly this many ticks inside a window, before honesty flooring may thin them out. */
const TARGET_TICKS = 5;

const NICE = [1, 2, 5];

/**
 * The smallest "nice" (1/2/5 × 10ⁿ) step that is `>= min`.
 *
 * Nice steps are what a ruler prints — round MHz, round seconds — never a step that happens to
 * divide the span evenly, which would print unreadable fractions.
 */
export function niceStep(min: number): number {
  if (!(min > 0)) return 1;
  const exp = Math.floor(Math.log10(min));
  for (const e of [exp, exp + 1]) {
    for (const n of NICE) {
      const step = n * 10 ** e;
      if (step >= min * (1 - 1e-9)) return step;
    }
  }
  return 10 ** (exp + 1);
}

/** How many MHz decimals a step this fine needs so consecutive ticks print as distinct numbers. */
function mhzDecimals(stepHz: number): number {
  return Math.max(0, Math.min(6, Math.ceil(-Math.log10(stepHz / 1e6) - 1e-9)));
}

function fmtFreqTick(hz: number, stepHz: number): string {
  return `${(hz / 1e6).toFixed(mhzDecimals(stepHz))} MHz`;
}

/**
 * Intermediate frequency marks inside `[loHz, hiHz)`, at a step floored to `cellHz` — the cell the
 * pane's `PaneReport` says it actually drew at. Empty when the window is degenerate or so narrow
 * (relative to its own cell) that no interior nice step fits, which is honest rather than a bug: a
 * pane one cell wide has no finer mark to offer.
 */
export function freqTicksOf(loHz: number, hiHz: number, cellHz: number, targetTicks = TARGET_TICKS): AxisTick[] {
  const span = hiHz - loHz;
  if (!(span > 0) || !(cellHz > 0)) return [];
  const step = niceStep(Math.max(cellHz, span / (targetTicks + 1)));
  // Strictly interior: a mark exactly AT an edge would only restate what `headline` already says,
  // never add one. `eps` is scaled to the step (not to the absolute Hz magnitude), so it excludes
  // only a coincidental exact-edge alignment, never a legitimate near-edge tick.
  const eps = step * 1e-6;
  const out: AxisTick[] = [];
  for (let v = Math.ceil((loHz + eps) / step) * step; v < hiHz - eps; v += step) {
    out.push({ value: v, label: fmtFreqTick(v, step) });
  }
  return out;
}

/**
 * Intermediate time marks inside `[t0Ns, t1Ns)`, at a step floored to `cellS` (seconds — the pane's
 * stated time-cell size). Labelled the same way the chrome's own `timeLabel` is: offset behind the
 * live edge, because that is this surface's one time vocabulary (§8.5a states `LIVE` or `−<span>`,
 * never a wall-clock string), and `"live edge"` for the tick that lands on it.
 */
export function timeTicksOf(t0Ns: number, t1Ns: number, cellS: number, edgeNs: number, targetTicks = TARGET_TICKS): AxisTick[] {
  const span = t1Ns - t0Ns;
  const cellNs = cellS * 1e9;
  if (!(span > 0) || !(cellNs > 0)) return [];
  const step = niceStep(Math.max(cellNs, span / (targetTicks + 1)));
  // Strictly interior, for the same reason as [[freqTicksOf]]: an edge-coincident mark restates
  // `timeLabel`, it does not add one.
  const eps = step * 1e-6;
  const out: AxisTick[] = [];
  for (let v = Math.ceil((t0Ns + eps) / step) * step; v < t1Ns - eps; v += step) {
    out.push({ value: v, label: Math.abs(edgeNs - v) < eps ? "live edge" : `−${fmtSpan((edgeNs - v) / 1e9)}` });
  }
  return out;
}

/** Join a tick list into the sentence the chrome row shows, or `null` when there is nothing between
 * the two edges to mark. */
export function joinTicks(ticks: readonly AxisTick[]): string | null {
  return ticks.length ? ticks.map((t) => t.label).join(", ") : null;
}

/** The whole ruler line for one pane: frequency marks, then time marks, each `null` when empty. */
export function rulerLabel(
  loHz: number, hiHz: number, t0Ns: number, t1Ns: number, cellHz: number, cellS: number, edgeNs: number,
): string | null {
  const f = joinTicks(freqTicksOf(loHz, hiHz, cellHz));
  const t = joinTicks(timeTicksOf(t0Ns, t1Ns, cellS, edgeNs));
  if (!f && !t) return null;
  const parts = [f ? `freq ${f}` : null, t ? `time ${t}` : null].filter((x): x is string => x !== null);
  return parts.join(" · ");
}
