// T-981: **the `frontend` layer** — where the radio's own front end, not the air, put the energy.
//
// A strong burst keying up with the amp on, or the start-up transient, drives the 8-bit front end
// past full scale; clipping spreads energy across the whole tuned window, and the canvas draws that
// one-row stripe exactly like a signal. The backend measures each spectrum row's own clipped
// samples and judges a clipped row whose energy stepped up across the whole span a **front-end
// event** (`GET /api/frontend/events`, docs/api.md). This layer marks those events so the stripe is
// never read as signal.
//
// ## Nothing here judges anything
//
// Every event is a backend answer; this file only converts `[t0, t1) × [f_lo, f_hi]` into quads
// inside one pane, through the very `toClip` the tiles are placed with, from inside
// `SurfaceView.frame()` — so the mark and the tile row at its capture time are placed by one
// mapping on one frame and cannot drift apart (T-388). The poll only refreshes the records.
//
// ## A distinct mark, never a signal's
//
// Its own ink (a hot red, off every measurement mark, class outline and ramp stop) drawn as a
// **hatch** between two solid edges — a stroke, never a wash, so the energy under it stays
// readable. Never a box: a detection's box is the one symbol that means "signal here". An event is
// often a single row, so the band is held to a minimum height on screen, centred on its time.

import type { Box } from "./lattice";
import type { OverlayQuad } from "./minimap";
import { toClip, type PaneRect } from "./surface";

const S_TO_NS = 1e9;

/** The front-end ink: a hot red, off the confirmed teal, candidate violet, selection orange, the
 * paths blue, every retune ink and the density orange. */
export const FRONTEND_MARK: readonly [number, number, number, number] = [1.0, 0.18, 0.28, 0.9];
/** Edge thickness, device px. */
export const FRONTEND_EDGE_PX = 1;
/** Smallest on-screen height of an event's band, device px (a one-row event may be sub-pixel). */
export const FRONTEND_MIN_PX = 4;
export const FRONTEND_HATCH_PERIOD_PX = 6;
export const FRONTEND_HATCH_ON_PX = 2;

/** One front-end event as the client holds it: a time–frequency region at absolute capture time. */
export interface FrontEndEvent {
  readonly id: string;
  readonly device: string;
  readonly t0Ns: number;
  readonly t1Ns: number;
  readonly f0Hz: number;
  readonly f1Hz: number;
  readonly rows: number;
}

/**
 * The request for the panes whose `frontend` layer is on: one `GET /api/frontend/events` over the
 * union of their time spans, or `null` when there is nothing to ask about. Events are selected by
 * time only — each spans its whole tuned window — and a pane clips what it draws.
 */
export function frontEndRequest(boxes: readonly Box[]): string | null {
  const ok = boxes.filter((b) => b.f1Hz > b.f0Hz && b.t1Ns > b.t0Ns && b.t0Ns > 0);
  if (ok.length === 0) return null;
  const t0 = Math.min(...ok.map((b) => b.t0Ns)) / S_TO_NS;
  const t1 = Math.max(...ok.map((b) => b.t1Ns)) / S_TO_NS;
  return `/api/frontend/events?t0=${t0}&t1=${t1}`;
}

/** The wire answer → [[FrontEndEvent]]s. Anything malformed is dropped rather than drawn. */
export function parseFrontEndEvents(body: unknown): FrontEndEvent[] {
  const list = (body as { events?: unknown } | null)?.events;
  if (!Array.isArray(list)) return [];
  const out: FrontEndEvent[] = [];
  const num = (v: unknown): v is number => typeof v === "number" && Number.isFinite(v);
  for (const e of list as Record<string, unknown>[]) {
    if (!e || !num(e.t0) || !num(e.t1) || !num(e.f_lo_hz) || !num(e.f_hi_hz)) continue;
    if (!(e.t1 > e.t0) || !(e.f_hi_hz > e.f_lo_hz)) continue;
    const device = typeof e.device_id === "string" ? e.device_id : "unknown";
    out.push({
      id: `frontend:${device}:${e.t0_ns ?? e.t0}`,
      device,
      t0Ns: e.t0 * S_TO_NS,
      t1Ns: e.t1 * S_TO_NS,
      f0Hz: e.f_lo_hz,
      f1Hz: e.f_hi_hz,
      rows: num(e.rows) ? e.rows : 1,
    });
  }
  return out;
}

export interface FrontEndStyle { dpr?: number; rgba?: readonly [number, number, number, number] }

/**
 * Mark every event inside one pane: a hatched band over the event's window and time, between two
 * solid edges, held to [[FRONTEND_MIN_PX]] tall. An event wholly off the pane draws nothing.
 */
export function frontEndQuads(
  events: readonly FrontEndEvent[], paneBox: Box, rect: PaneRect, style: FrontEndStyle = {},
): OverlayQuad[] {
  const k = style.dpr && style.dpr > 0 ? style.dpr : 1;
  const rgba = style.rgba ?? FRONTEND_MARK;
  const h = Math.max(1, rect.h);
  const minY = (FRONTEND_MIN_PX * k * 2) / h; // clip units: 2 clip = h px
  const edgeY = (FRONTEND_EDGE_PX * k * 2) / h;
  const out: OverlayQuad[] = [];
  for (const e of events) {
    const [x0, ya, x1, yb] = toClip({ f0Hz: e.f0Hz, f1Hz: e.f1Hz, t0Ns: e.t0Ns, t1Ns: e.t1Ns }, paneBox);
    let y0 = Math.min(ya, yb), y1 = Math.max(ya, yb);
    if (y1 - y0 < minY) {
      const c = (y0 + y1) / 2;
      y0 = c - minY / 2;
      y1 = c + minY / 2;
    }
    const vx0 = Math.max(x0, -1), vx1 = Math.min(x1, 1), vy0 = Math.max(y0, -1), vy1 = Math.min(y1, 1);
    if (!(vx1 > vx0) || !(vy1 > vy0)) continue;
    out.push({
      clip: [vx0, vy0, vx1, vy1], rgba, kind: "frontend-event", id: e.id, part: "fill",
      pattern: { mode: "hatch", periodPx: FRONTEND_HATCH_PERIOD_PX * k, onPx: FRONTEND_HATCH_ON_PX * k, origin: [x0, y0] },
    });
    for (const y of [y0, y1]) {
      const a = Math.max(-1, y - edgeY / 2), b = Math.min(1, y + edgeY / 2);
      if (b > a) out.push({ clip: [vx0, a, vx1, b], rgba, kind: "frontend-event", id: e.id, part: "edge" });
    }
  }
  return out;
}

/** The layer's key for the layers menu. */
export function frontEndKeyEntries(events: readonly FrontEndEvent[]): {
  key: string; label: string; note: string; rgb: readonly [number, number, number];
}[] {
  const n = events.length;
  return [{
    key: "frontend",
    label: "Front-end overload",
    note: n === 0
      ? "No front-end event in view."
      : `${n} event${n === 1 ? "" : "s"} in view: the ADC clipped and the whole window lifted — the radio's energy, not a signal.`,
    rgb: [FRONTEND_MARK[0], FRONTEND_MARK[1], FRONTEND_MARK[2]],
  }];
}
