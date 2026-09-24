// **The on-canvas measurement tool** (T-822 / MAP-22, docs/25 §4, RESEARCH-003): drag/cursors to
// read Δf, Δt, bandwidth and duration between two points on the surface, inspectrum-style.
//
// **Pure presentation arithmetic over already-known view state** — CLAUDE.md's thin-client rule in
// this ticket's own words. Everything here is `Math` over two `{fHz, tNs}` points; nothing fetches,
// polls or decides what a signal is. The **stored** value has one authority: `POST
// /api/measurements` sends only the two cursors and the server computes value/unit/place
// (`crates/hk-model/src/repo/measurements.rs`'s `compute_measurement`, docs/25 §10.4). What is
// computed here is the **live readout** — the numbers a drag shows before (and instead of) a save —
// and it is deliberately the same arithmetic the server does for `delta_f`/`delta_t`/`bandwidth`/
// `duration`, so the number on screen while dragging is never surprised by the number the server
// answers with.
import type { MarkRegion } from "./marks";

const S_TO_NS = 1e9;

/** What a two-cursor drag reads, before any save. `bandwidth`/`duration` are the same numbers as
 * `deltaFHz`/`deltaTS` under docs/25 §4's naming — a marked-out region's width and time extent —
 * kept as separate fields only so a caller can label them by what the user is doing (marking a
 * span) rather than by which axis moved. */
export interface MeasureReadout {
  readonly fLoHz: number;
  readonly fHiHz: number;
  readonly t0Ns: number;
  readonly t1Ns: number;
  readonly deltaFHz: number;
  readonly deltaTS: number;
  readonly bandwidthHz: number;
  readonly durationS: number;
}

/** The readout for a stroke's two corners, already ordered low-to-high by `normalizeRegion`. */
export function measureReadout(region: MarkRegion): MeasureReadout {
  const deltaFHz = region.f1Hz - region.f0Hz;
  const deltaTS = (region.t1Ns - region.t0Ns) / S_TO_NS;
  return {
    fLoHz: region.f0Hz, fHiHz: region.f1Hz, t0Ns: region.t0Ns, t1Ns: region.t1Ns,
    deltaFHz, deltaTS, bandwidthHz: deltaFHz, durationS: deltaTS,
  };
}

/** `Hz` formatted at a resolution useful for a ruler readout — three decimal MHz down to whole Hz,
 * the same idea `centre/surface.ts`'s own `fmtHz` uses for the hover line, kept separate because
 * this module must stay DOM-free and importable from a unit test with no app around it. */
function fmtHz(hz: number): string {
  const a = Math.abs(hz);
  if (a >= 1e6) return `${(hz / 1e6).toFixed(3)} MHz`;
  if (a >= 1e3) return `${(hz / 1e3).toFixed(3)} kHz`;
  return `${hz.toFixed(1)} Hz`;
}

/** The one-line readout text: `"Δf 6.600 MHz · Δt 3.000 s"`. A stroke with no extent on an axis
 * still names it (`0.0 Hz`/`0.000 s`) rather than omitting it — the axis was genuinely dragged
 * across, even if by nothing, and a reader should not have to infer which quantity is missing. */
export function fmtMeasureReadout(r: MeasureReadout): string {
  return `Δf ${fmtHz(r.deltaFHz)} · Δt ${r.deltaTS.toFixed(3)} s`;
}
