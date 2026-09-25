// T-1008: the scan plan as a map overlay — the frequency region a sweep will cover and the dwell
// steps it will take, drawn over the canvas (grey, unobserved cells included) so the user sees what
// they would be scanning before they commit the radio, and sees it progress while it runs.
//
// ## What this file may decide, and what it may not
//
// **It never computes a step.** The steps are `plan.windows` exactly as `GET /api/control/scan?
// windows=1` served them — the engine's own compiled hops, in visit order (T-452/T-1008). The
// tiling depends on the rate in force, the device's ranges, RF-path boundaries and clipping, and
// none of that is the client's to know; a second tiling here would be a plan the engine does not
// execute. While a region is being dragged and has not been re-priced yet, there are no windows to
// draw, and this draws the region alone (hatched, no step lines) rather than guess at them.
//
// It decides only presentation: where a served slice lands in a pane (the same `toClip` arithmetic
// every other mark uses), which slices are covered / being dwelt on / still to come (from the
// server's own `progress.step` and `progress.dwell_step` indices), and when step lines are too
// dense to draw honestly.
//
// ## Strokes, never a wash
//
// Everything is a quad for `overlay.ts`: region edges are solid bars, step boundaries are dashed
// lines, and the fill is T-910's screen-door hatch — fragments off the pattern are DISCARDED, so
// between two hatch lines the pixel is the data (or the grey) exactly as the data pass drew it.
// That is what lets the plan sit visibly over grey cells without tinting a single measurement.
//
// ## Time
//
// A plan is a frequency programme, not a time region: it spans the pane's whole height, like the
// band-plan priors' edges (`./priors.ts`). What it has covered so far shows up beneath it as the
// coverage map fills — the plan's hatch over a covered step thins so that fill is what you see.
import type { Box } from "./lattice";
import type { OverlayQuad } from "./minimap";
import type { PaneRect } from "./surface";

/** One step as served (`plan.windows[i]`): the slice it is responsible for and its centre. */
export interface ScanWindow {
  readonly step: number;
  readonly lo_hz: number;
  readonly hi_hz: number;
  readonly center_hz: number;
}

/** What the overlay draws, in one pane-independent model. Built by the scan controller. */
export interface ScanOverlayModel {
  /** `plan`: a proposal being edited (nothing committed); `running` / `yielded`: a sweep exists. */
  readonly state: "plan" | "running" | "yielded";
  /** The region's edges, Hz — the draggable handles while `editable`. */
  readonly loHz: number;
  readonly hiHz: number;
  /** The steps, as served; `null` while the region is being (re)priced. */
  readonly windows: readonly ScanWindow[] | null;
  /** `progress.dwell_step`: the step being dwelt on now (running only). */
  readonly dwellStep: number | null;
  /** `progress.step`: the next step to tune; the steps before it in this pass are covered. */
  readonly nextStep: number | null;
  /** Whether the region edges can be dragged (an idle plan only: a running sweep is committed). */
  readonly editable: boolean;
}

/** Step status for presentation. */
export type StepStatus = "pending" | "covered" | "dwelling";

/** The status of step `i` — from the server's indices only. */
export function stepStatus(m: ScanOverlayModel, i: number): StepStatus {
  if (m.state === "plan") return "pending";
  if (m.dwellStep !== null && i === m.dwellStep) return "dwelling";
  if (m.nextStep !== null && i < m.nextStep) return "covered";
  return "pending";
}

/** The plan's ink: lime — a hue the ramp (blue → cyan → yellow → red → white) never reaches head-on,
 * and none of the other marks use (amber is the selection/pane mark, orange the density hatch,
 * violet the candidates and priors, blue the paths and measurements). */
export const SCAN_INK = [0.62, 1.0, 0.22] as const;
const rgba = (a: number) => [SCAN_INK[0], SCAN_INK[1], SCAN_INK[2], a] as const;
/** Region edges: solid, the handles. */
const EDGE_PX = 3;
/** Step boundaries are drawn only where a step is at least this wide on screen (device px);
 * narrower, the lines would merge into a wash, so only the fill and the edges are drawn and the
 * step count is stated in words by the panel instead. */
export const MIN_STEP_PX = 6;
/** The dwelling step is never drawn narrower than this, so a 1 800-step pass still shows where
 * the radio is. */
const MIN_DWELL_PX = 4;
/** Hatch per status: [periodPx, onPx, alpha]. Covered steps fade (sparser, fainter) so the coverage
 * filling in beneath them is what reads; the dwelling one is the densest and brightest. */
const HATCH: Record<StepStatus, readonly [number, number, number]> = {
  pending: [9, 2, 0.55],
  covered: [16, 1, 0.3],
  dwelling: [5, 2, 0.95],
};
/** Pixels either side of an edge that grab it. */
export const GRAB_PX = 8;

type Clip = readonly [number, number, number, number];

/**
 * The quads of one pane's plan, through the pane's own box. A slice outside the pane draws nothing;
 * a slice cut by the pane's edge draws only its visible part. Pure.
 */
export function scanPlanQuads(m: ScanOverlayModel | null, box: Box, rect: PaneRect): OverlayQuad[] {
  if (!m || !(box.f1Hz > box.f0Hz) || !(rect.w > 0) || !(rect.h > 0) || !(m.hiHz > m.loHz)) return [];
  const out: OverlayQuad[] = [];
  const span = box.f1Hz - box.f0Hz;
  const x = (f: number) => (2 * (f - box.f0Hz)) / span - 1;
  const pxX = 2 / rect.w;
  const origin = [-1, -1] as const;
  const hatch = (x0: number, x1: number, st: StepStatus, id: string) => {
    const a = Math.max(-1, x0), b = Math.min(1, x1);
    if (!(b > a)) return;
    const [period, on, alpha] = HATCH[st];
    out.push({
      clip: [a, -1, b, 1] as Clip, rgba: rgba(alpha), kind: "scan-plan", id, part: "fill",
      pattern: { mode: "hatch", periodPx: period, onPx: on, origin },
    });
  };
  const vline = (xc: number, wPx: number, alpha: number, id: string, part: "edge" | "handle", dashed = false) => {
    if (xc < -1 || xc > 1) return;
    const x0 = Math.max(-1, Math.min(1 - wPx * pxX, xc - (wPx * pxX) / 2));
    out.push({
      clip: [x0, -1, x0 + wPx * pxX, 1] as Clip, rgba: rgba(alpha), kind: "scan-plan", id, part,
      ...(dashed ? { pattern: { mode: "dash-y" as const, periodPx: 8, onPx: 4, origin } } : {}),
    });
  };

  const w = m.windows;
  if (!w || w.length === 0) {
    // Being re-priced: the region alone. No step lines — the steps are the server's to state.
    hatch(x(m.loHz), x(m.hiHz), "pending", "scan:region");
  } else {
    // The fill: consecutive slices of one status merge into one quad, so a 3 000-step pass is a
    // handful of quads, not thousands.
    let runStart = 0;
    for (let i = 1; i <= w.length; i++) {
      const st = stepStatus(m, runStart);
      if (i < w.length && stepStatus(m, i) === st) continue;
      if (st !== "dwelling") hatch(x(w[runStart].lo_hz), x(w[i - 1].hi_hz), st, `scan:${st}:${runStart}`);
      runStart = i;
    }
    // The dwelling step: its own hatch and a solid outline, widened to be visible when tiny.
    if (m.dwellStep !== null && m.dwellStep >= 0 && m.dwellStep < w.length) {
      const d = w[m.dwellStep];
      let x0 = x(d.lo_hz), x1 = x(d.hi_hz);
      const minW = MIN_DWELL_PX * pxX;
      if (x1 - x0 < minW) { const c = (x0 + x1) / 2; x0 = c - minW / 2; x1 = c + minW / 2; }
      hatch(x0, x1, "dwelling", `scan:dwelling:${m.dwellStep}`);
      vline(x0, 2, 0.95, `scan:dwelling:${m.dwellStep}`, "edge");
      vline(x1, 2, 0.95, `scan:dwelling:${m.dwellStep}`, "edge");
    }
    // Step boundaries — only where a step is wide enough on screen to be read as a step.
    const meanPx = ((x(w[w.length - 1].hi_hz) - x(w[0].lo_hz)) / w.length) / pxX;
    if (meanPx >= MIN_STEP_PX) {
      for (let i = 1; i < w.length; i++) vline(x(w[i].lo_hz), 1, 0.55, `scan:step:${i}`, "edge", true);
    }
  }
  // The region's edges: the handles, drawn last so they are on top.
  vline(x(m.loHz), m.editable ? EDGE_PX + 1 : EDGE_PX, 0.95, "scan:lo", "handle");
  vline(x(m.hiHz), m.editable ? EDGE_PX + 1 : EDGE_PX, 0.95, "scan:hi", "handle");
  return out;
}

/** Whether steps are drawn individually in a pane (the panel says so when they are not). */
export function stepsDrawn(m: ScanOverlayModel | null, box: Box, rect: PaneRect): boolean {
  const w = m?.windows;
  if (!w || w.length === 0 || !(box.f1Hz > box.f0Hz) || !(rect.w > 0)) return false;
  const px = ((w[w.length - 1].hi_hz - w[0].lo_hz) / (box.f1Hz - box.f0Hz)) * rect.w;
  return px / w.length >= MIN_STEP_PX;
}

/**
 * Which region edge, if any, is under device-pixel column `xPx` (the canvas's GL x) in a pane —
 * only for an editable plan. The nearer edge wins; a narrow region's two edges both in reach pick
 * by side.
 */
export function scanEdgeAt(
  m: ScanOverlayModel | null, box: Box, rect: PaneRect, xPx: number, grabPx = GRAB_PX,
): "lo" | "hi" | null {
  if (!m?.editable || !(box.f1Hz > box.f0Hz) || !(rect.w > 0)) return null;
  const px = (f: number) => rect.x + ((f - box.f0Hz) / (box.f1Hz - box.f0Hz)) * rect.w;
  const dLo = Math.abs(xPx - px(m.loHz)), dHi = Math.abs(xPx - px(m.hiHz));
  if (dLo > grabPx && dHi > grabPx) return null;
  return dLo < dHi ? "lo" : dHi < dLo ? "hi" : xPx < px(m.loHz) ? "lo" : "hi";
}

/** The region after dragging `edge` to `fHz`: the other edge stays, and the two never cross or
 * meet (a region needs width); `minHz` is the narrowest the drag may make it. */
export function dragRegion(
  lo: number, hi: number, edge: "lo" | "hi", fHz: number, minHz: number,
): { loHz: number; hiHz: number } {
  if (edge === "lo") return { loHz: Math.min(fHz, hi - minHz), hiHz: hi };
  return { loHz: lo, hiHz: Math.max(fHz, lo + minHz) };
}
