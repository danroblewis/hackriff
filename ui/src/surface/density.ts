// T-810 (MAP-10, docs/23 §10.6 rule 6 last bullet, re-scoped 2026-09-24 from pin clustering):
// "Density, not clustering, at coarse zoom. Where many features would generalize, a density layer
// (features per cell, from `/api/tiles/events`) replaces numbered cluster bubbles; drilling in
// resolves to boxes."
//
// ## What this draws, and what it must never look like
//
// `GET /api/tiles/events` answers a count of events per cell of ONE tile address — never a
// viewport bbox — so this layer asks for the tile(s) the pane's box covers at the pane's own
// level (the same `levelsFor`/`tilesFor` the base tile pass resolves a pane's address with,
// `./lattice.ts`), and reads back a count grid over exactly that tile's `nt × nf` cells.
//
// A cell is drawn ONLY where [[isGeneralized]] says a feature there would already collapse to a
// symbol (under `GENERALIZE_BELOW_CSS_PX` in BOTH axes, the same predicate `marks.ts`/`pins.ts`
// decide generalization by) — so zooming in past that threshold makes this layer draw nothing and
// leaves the picture to the `detections` boxes, exactly the "drilling in resolves to boxes" rule.
// It is never a numbered bubble: the mark is a filled cell, its darkness a function of the count.
//
// It is a **stroke, never a wash**: like every overlay, it is submitted to `overlay.ts`'s pattern
// shader with a hatch, so off-pattern pixels are discarded and the measurement under it is
// untouched — the same structural guarantee the byte-identical-with-overlays-off test already
// covers, unchanged by this file.
//
// Thin client: no signal logic here. The count per cell is the backend's; this file only asks for
// the tile a pane is looking at, converts its grid into overlay geometry through the same `toClip`
// the tiles are drawn with, and never reaches a device route.

import { addrSpelling, levelsFor, tilesFor, type Box, type Lattice, type TileAddr } from "./lattice";
import { GENERALIZE_BELOW_CSS_PX, isGeneralized } from "./marks";
import type { OverlayQuad } from "./minimap";
import { toClip, type PaneRect } from "./surface";

const NS_PER_S = 1e9;

/** Most tiles fetched per pane per refresh — a coarse zoom's whole viewport is usually 1-4; this
 * only bounds a pathological case (a very wide pane at a level whose tiles are still numerous). */
export const DENSITY_MAX_TILES_PER_PANE = 16;

/** One `GET /api/tiles/events` answer, kept as served. */
export interface DensityTile {
  readonly addr: TileAddr;
  readonly fLoHz: number;
  readonly fCellHz: number;
  readonly t0Ns: number;
  readonly tCellNs: number;
  readonly nt: number;
  readonly nf: number;
  /** Row-major, `nt` rows (earliest first) x `nf` columns (lowest frequency first) — exactly the
   * route's own spelling, so this file never re-derives the grid it was handed. */
  readonly counts: readonly number[];
}

/**
 * The tile addresses covering `box` at the level a pane of `wPx x hPx` would draw its base tiles
 * at (`levelsFor`) — so density is asked about the same resolution the boxes it stands in for
 * would be drawn at, never a coarser or finer one this pane is not showing. Bounded to
 * [[DENSITY_MAX_TILES_PER_PANE]], keeping `tilesFor`'s own least-wanted-first order so a bounded
 * pane still drops the tiles furthest from its centre first.
 */
export function densityAddrs(lat: Lattice, box: Box, wPx: number, hPx: number, device = "any"): TileAddr[] {
  const { levelF, levelT } = levelsFor(lat, box, wPx, hPx);
  const all = tilesFor(lat, box, levelF, levelT, device);
  return all.length <= DENSITY_MAX_TILES_PER_PANE ? all : all.slice(all.length - DENSITY_MAX_TILES_PER_PANE);
}

/**
 * The request for one tile address (`docs/api.md` "GET /api/tiles/events"). Deliberately its own
 * builder, not `./lattice.ts`'s `tileUrl`: that route's allowed parameters are `device`, `scheme`,
 * `level_f`, `level_t`, `f_index`, `t_index`, `cells`, `state`, `token` — no `planes` and, unlike
 * `/api/tiles`, no `client` share either — so a builder that carried either would be refused the
 * moment a page has named a tile client id.
 */
export function densityUrl(a: TileAddr, path = "/api/tiles/events"): string {
  const q = new URLSearchParams({
    level_f: String(a.levelF), level_t: String(a.levelT), f_index: String(a.fIndex), t_index: String(a.tIndex),
  });
  if (a.scheme !== "view") q.set("scheme", a.scheme);
  if (a.device !== "any") q.set("device", a.device);
  if (a.cells !== 256) q.set("cells", String(a.cells));
  return `${path}?${q.toString()}`;
}

const num = (v: unknown): number | null => (typeof v === "number" && Number.isFinite(v) ? v : null);

/** The wire answer -> [[DensityTile]], or `null` when it is not one (malformed extent, no counts
 * grid): dropped rather than drawn, the same rule every other layer here follows. */
export function parseDensityTile(addr: TileAddr, body: unknown): DensityTile | null {
  const b = body as { extent?: Record<string, unknown>; counts?: unknown } | null;
  const ext = b?.extent;
  if (!ext || !Array.isArray(b?.counts)) return null;
  const nt = ext.nt, nf = ext.nf;
  if (typeof nt !== "number" || typeof nf !== "number" || !(nt > 0) || !(nf > 0)) return null;
  const fLoHz = num(ext.f_lo_hz), fCellHz = num(ext.f_cell_hz), t0S = num(ext.t0_s), tCellS = num(ext.t_cell_s);
  if (fLoHz === null || fCellHz === null || t0S === null || tCellS === null || !(fCellHz > 0) || !(tCellS > 0)) return null;
  const counts = (b.counts as unknown[]).map((c) => (typeof c === "number" && Number.isFinite(c) && c > 0 ? c : 0));
  return { addr, fLoHz, fCellHz, t0Ns: t0S * NS_PER_S, tCellNs: tCellS * NS_PER_S, nt, nf, counts };
}

export interface DensityStyle {
  readonly generalizeBelowPx?: number;
  readonly dpr?: number;
  readonly rgba?: readonly [number, number, number, number];
  /** The count at/above which a cell reaches [[maxAlpha]]; below it, alpha scales linearly. */
  readonly capCount?: number;
  readonly maxAlpha?: number;
  readonly hatchPeriodPx?: number;
  readonly hatchOnPx?: number;
}

/** A warm ink, off every measurement-ramp stop and every class outline in `marks.ts` — a density
 * cell must never be mistaken for a Confirmed box's cyan fill or a Candidate's violet outline. */
export const DENSITY_MARK: readonly [number, number, number, number] = [0.93, 0.56, 0.16, 1];
export const DENSITY_CAP_COUNT = 8;
export const DENSITY_MAX_ALPHA = 0.55;
export const DENSITY_HATCH_PERIOD_CSS_PX = 6;
export const DENSITY_HATCH_ON_CSS_PX = 2;

/**
 * `tiles` drawn into one pane: one hatch-filled quad per non-empty cell, its darkness a function of
 * the count, and ONLY for a tile whose cell is under `generalizeBelowPx` in BOTH axes on screen —
 * [[isGeneralized]], the same predicate a box collapses to a symbol by. A tile whose cells are
 * already big enough to be boxes draws nothing here at all: this layer never competes with the
 * `detections` boxes it exists to stand in for.
 */
export function densityQuads(tiles: readonly DensityTile[], paneBox: Box, rect: PaneRect, style: DensityStyle = {}): OverlayQuad[] {
  const belowPx = style.generalizeBelowPx ?? GENERALIZE_BELOW_CSS_PX;
  const k = style.dpr && style.dpr > 0 ? style.dpr : 1;
  const W = Math.max(1, rect.w), H = Math.max(1, rect.h);
  const rgb = style.rgba ?? DENSITY_MARK;
  const capCount = Math.max(1, style.capCount ?? DENSITY_CAP_COUNT);
  const maxAlpha = style.maxAlpha ?? DENSITY_MAX_ALPHA;
  const hatchPeriodPx = style.hatchPeriodPx ?? DENSITY_HATCH_PERIOD_CSS_PX;
  const hatchOnPx = style.hatchOnPx ?? DENSITY_HATCH_ON_CSS_PX;
  const out: OverlayQuad[] = [];
  for (const tile of tiles) {
    // The cell size is uniform across one tile: one on-screen measurement decides the whole tile.
    const [ccx0, ccy0, ccx1, ccy1] = toClip(
      { f0Hz: tile.fLoHz, f1Hz: tile.fLoHz + tile.fCellHz, t0Ns: tile.t0Ns, t1Ns: tile.t0Ns + tile.tCellNs }, paneBox,
    );
    const cellWpx = (Math.abs(ccx1 - ccx0) / 2) * W / k;
    const cellHpx = (Math.abs(ccy1 - ccy0) / 2) * H / k;
    if (!isGeneralized(cellWpx, cellHpx, belowPx)) continue;
    for (let r = 0; r < tile.nt; r++) {
      for (let c = 0; c < tile.nf; c++) {
        const count = tile.counts[r * tile.nf + c] ?? 0;
        if (!(count > 0)) continue;
        const f0 = tile.fLoHz + c * tile.fCellHz, f1 = f0 + tile.fCellHz;
        const t0 = tile.t0Ns + r * tile.tCellNs, t1 = t0 + tile.tCellNs;
        const [x0, y0, x1, y1] = toClip({ f0Hz: f0, f1Hz: f1, t0Ns: t0, t1Ns: t1 }, paneBox);
        const vx0 = Math.max(x0, -1), vx1 = Math.min(x1, 1), vy0 = Math.max(y0, -1), vy1 = Math.min(y1, 1);
        if (!(vx1 > vx0) || !(vy1 > vy0)) continue;
        const alpha = maxAlpha * Math.min(1, count / capCount);
        out.push({
          clip: [vx0, vy0, vx1, vy1],
          rgba: [rgb[0], rgb[1], rgb[2], alpha],
          kind: "density-cell",
          id: `${addrSpelling(tile.addr)}.${r}.${c}`,
          part: "fill",
          pattern: { mode: "hatch", periodPx: hatchPeriodPx * k, onPx: hatchOnPx * k, origin: [x0, y0] },
        });
      }
    }
  }
  return out;
}
