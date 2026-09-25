// **The `tune` layer** (T-898, docs/23 §10.6 rule 2): *where the radio has been* — the device's own
// route through frequency, traced on the map like a directions line.
//
// The user's second map-UI principle is that anything with (time × frequency) coordinates is drawn
// on the surface. The front end's own movement has them: at every instant it was tuned somewhere,
// and each retune moved it. With the canvas's axes (X = frequency, Y = time) that draws as a
// **vertical run per dwell** and a **near-horizontal step per retune** — a staircase up the screen
// showing, at a glance, where the survey has actually been.
//
// ## Nothing here derives the tune history
//
// Every vertex is a backend answer (`GET /api/tune-history`, derived by `hk_store::tunepath` from
// the tune records the coverage map is already built on). This file converts vertices into strokes
// inside one pane, through the very `toClip` the tiles are placed with, from inside
// `SurfaceView.frame()` — so a vertex and the tile row at its capture time are placed by one
// mapping on one frame and cannot drift apart (T-388's structural fix). The poll only refreshes the
// records; it never positions anything.
//
// ## One line per front end, labelled by `device_id`
//
// The several-SDRs rule: each radio's route is its own path with its own ink, and the key in the
// layers menu names the device beside its colour ([[tuneKeyEntries]]). `unknown` — a record that
// named no radio — is its own route and never folded into a named one, exactly as `/api/coverage`
// keeps them apart.
//
// ## It is the device's route, not a measurement
//
// The stroke is drawn in its own palette, distinct from every measurement mark, and it is only ever
// a stroke: like the `paths` layer it submits axis-aligned `path-stroke` rectangles through
// [[pathQuads]], so the data pass stays byte-identical with the layer on and off. A gap the backend
// broke the route at is simply two paths, and nothing is drawn between them — the client never
// joins them, because not knowing where the radio was is not something to draw over.

import type { Box } from "./lattice";
import type { OverlayQuad } from "./minimap";
import { pathQuads, type PathStyle } from "./paths";
import type { PaneRect } from "./surface";

const S_TO_NS = 1e9;

/** Stroke thickness of a device route, device px. */
export const TUNE_STROKE_PX = 2;

/**
 * The inks a device route may be drawn in, in assignment order: an Okabe–Ito-derived set, chosen
 * colour-blind safe (T-813's rule) and away from every measurement mark — the confirmed teal, the
 * candidate purple, the selection orange and the `paths` layer's blue — so the radio's own route
 * can never be mistaken for a signal.
 */
export const TUNE_INKS: readonly (readonly [number, number, number, number])[] = [
  [0.84, 0.37, 0.0, 0.9], // vermillion
  [0.94, 0.89, 0.26, 0.9], // yellow
  [0.8, 0.47, 0.65, 0.9], // reddish purple
  [0.0, 0.45, 0.7, 0.9], // dark blue
  [0.35, 0.7, 0.9, 0.9], // sky
];

/** One device's route as the client holds it: ordered vertices at absolute capture time. */
export interface TunePath {
  readonly id: string;
  /** The front end this is the route of (`unknown` for a record that named none). */
  readonly device: string;
  readonly deviceNamed: boolean;
  readonly retunes: number;
  readonly vertices: readonly { readonly tNs: number; readonly fHz: number }[];
}

/**
 * The request for the panes whose `tune` layer is on: one `GET /api/tune-history` over the union of
 * their boxes, or `null` when there is nothing to ask about. A viewport route answers about a
 * viewport, so all four bounds are always sent.
 */
export function tuneHistoryRequest(boxes: readonly Box[]): string | null {
  const ok = boxes.filter((b) => b.f1Hz > b.f0Hz && b.t1Ns > b.t0Ns && b.t0Ns > 0);
  if (ok.length === 0) return null;
  const f0 = Math.max(0, Math.min(...ok.map((b) => b.f0Hz)));
  const f1 = Math.max(...ok.map((b) => b.f1Hz));
  const t0 = Math.min(...ok.map((b) => b.t0Ns)) / S_TO_NS;
  const t1 = Math.max(...ok.map((b) => b.t1Ns)) / S_TO_NS;
  return `/api/tune-history?f_lo=${f0}&f_hi=${f1}&t0=${t0}&t1=${t1}`;
}

/** The wire answer (`docs/api.md` "GET /api/tune-history") → [[TunePath]]s. Anything malformed is
 * dropped rather than drawn: a vertex with no time or frequency has no place on the map. */
export function parseTuneHistory(body: unknown): TunePath[] {
  const list = (body as { paths?: unknown } | null)?.paths;
  if (!Array.isArray(list)) return [];
  const out: TunePath[] = [];
  for (const p of list as Record<string, unknown>[]) {
    if (typeof p?.id !== "string" || typeof p?.device !== "string") continue;
    const vs = Array.isArray(p.vertices) ? (p.vertices as Record<string, unknown>[]) : [];
    const vertices = vs
      .filter((v) => typeof v?.t_s === "number" && typeof v?.f_hz === "number"
        && Number.isFinite(v.t_s) && Number.isFinite(v.f_hz))
      .map((v) => ({ tNs: (v.t_s as number) * S_TO_NS, fHz: v.f_hz as number }));
    if (vertices.length < 2) continue;
    out.push({
      id: p.id,
      device: p.device,
      deviceNamed: p.device_named === true,
      retunes: typeof p.retunes === "number" ? p.retunes : 0,
      vertices,
    });
  }
  return out;
}

/** The devices with a route, in the order the answer named them (stable, so the inks are stable). */
export function tuneDevices(paths: readonly TunePath[]): string[] {
  const seen: string[] = [];
  for (const p of paths) if (!seen.includes(p.device)) seen.push(p.device);
  return seen;
}

/** The ink of one device's route: its position in [[tuneDevices]], wrapped into [[TUNE_INKS]]. */
export function tuneInk(device: string, devices: readonly string[]): readonly [number, number, number, number] {
  const i = devices.indexOf(device);
  return TUNE_INKS[(i < 0 ? 0 : i) % TUNE_INKS.length];
}

/**
 * Stroke every device's route into one pane, each in its own ink, through the tiles' own mapping
 * ([[pathQuads]] does the clipping and the stepping).
 */
export function tuneQuads(
  paths: readonly TunePath[],
  paneBox: Box,
  rect: PaneRect,
  style: PathStyle = {},
): OverlayQuad[] {
  const devices = tuneDevices(paths);
  const out: OverlayQuad[] = [];
  for (const p of paths) {
    out.push(...pathQuads(
      [{ id: p.id, kind: "sweep", vertices: p.vertices }],
      paneBox,
      rect,
      { strokePx: TUNE_STROKE_PX, ...style, rgba: style.rgba ?? tuneInk(p.device, devices) },
    ));
  }
  return out;
}

/** One row of the layer's key: a front end, its ink, and what the line claims. */
export interface TuneKeyEntry {
  readonly key: string;
  readonly label: string;
  readonly note: string;
  readonly rgb: readonly [number, number, number];
}

/**
 * The key for the layers menu: **one row per front end**, so the route on screen is labelled by the
 * radio that drove it. With no route in the window the key says exactly that, rather than showing a
 * swatch for a line nobody can see.
 */
export function tuneKeyEntries(paths: readonly TunePath[]): TuneKeyEntry[] {
  const devices = tuneDevices(paths);
  if (devices.length === 0) {
    return [{
      key: "none",
      label: "No route in view",
      note: "No tune record covers this window — not a claim that the radio was nowhere.",
      rgb: [0.55, 0.58, 0.6],
    }];
  }
  return devices.map((d) => {
    const mine = paths.filter((p) => p.device === d);
    const retunes = mine.reduce((n, p) => n + p.retunes, 0);
    const ink = tuneInk(d, devices);
    const named = mine.every((p) => p.deviceNamed);
    return {
      key: d,
      label: named ? d : `${d} (no device named)`,
      note: `${retunes} retune${retunes === 1 ? "" : "s"} in view`
        + (mine.length > 1 ? `, in ${mine.length} runs — a gap no record covers is not drawn across.` : "."),
      rgb: [ink[0], ink[1], ink[2]] as const,
    };
  });
}
