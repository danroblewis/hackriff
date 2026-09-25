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
// The layer draws ONLY at genuinely coarse zoom — [[isCoarseZoom]]. It is NOT built on measuring
// any per-pixel cell size against a fixed px threshold (`isGeneralized`, `GENERALIZE_BELOW_CSS_PX`):
// the tile pyramid is "constant pixel density" by construction (`./lattice.ts`'s own header), so
// `densityAddrs`' own tile — chosen to match screen resolution the way the base tile pass's tile is
// — is ALWAYS close to one pixel on a side, at every zoom, and even the LATTICE's fixed, un-adapted
// level-0 cell reduces to an algebraically IDENTICAL px-threshold test once clamped (both are the
// same `f0Hz / hzPerPx` ratio compared against the same px bound) — a review caught exactly this
// (T-810, fix 1: gating on the tile's own cell "is always 1-2 px, so isGeneralized is always true").
//
// What genuinely varies with zoom, and cannot degenerate to a constant, is a **ratio with no px
// bound in it at all**: how many level-0 lattice cells — the finest grid this system addresses —
// are being compressed into ONE screen pixel, on either axis ([[DENSITY_MIN_CELLS_PER_PX]]). A wide
// viewport compresses thousands of level-0 cells per pixel; a viewport narrow enough to show real
// boxes compresses only a handful. This is a presentation/aggregation constant, like
// `VIEWPORT_TILE_BUDGET` — never a claim about how wide any real signal is (no signal logic here).
// Past the threshold this layer draws nothing at all, leaving the picture to the `detections` boxes
// — "drilling in resolves to boxes". It is never a numbered bubble: the mark is a filled cell, its
// darkness a function of the count.
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
 * How many level-0 lattice cells — the finest grid this system addresses at all — must compress
 * into ONE screen pixel, on AT LEAST ONE axis, before a pane reads as "coarse zoom".
 *
 * Deliberately **not** a pixel-SIZE bound (`isGeneralized`-style): a bound of that shape is, once
 * algebra is done on it, the exact tautology a review caught (T-810, fix 1) — `f0Hz / hzPerPx`
 * compared against a px threshold is what the removed per-tile check reduced to as well, since
 * `densityAddrs`' own tile is chosen to match screen resolution and lands in the same few px
 * regardless of zoom. A **ratio of cells to a pixel** carries no px bound to collapse to: it grows
 * without limit as the pane widens and shrinks toward zero as it narrows, so [[isCoarseZoom]]
 * genuinely crosses this threshold exactly once, at a real span, and stays crossed on one side.
 */
export const DENSITY_MIN_CELLS_PER_PX = 50;

/**
 * **The coarse-zoom test** (T-810, fix 2 after review caught the pixel-size version as
 * tautological): does viewing `box` at `wPx x hPx` compress at least [[DENSITY_MIN_CELLS_PER_PX]]
 * level-0 lattice cells into one screen pixel, on either axis? A pure presentation/aggregation
 * constant, like `VIEWPORT_TILE_BUDGET` — it says nothing about how wide any real signal is, only
 * how much the lattice's own finest addressable grid is being compressed on screen. TRUE over a
 * viewport spanning a wide slice of the addressable range; FALSE once the pane is narrow enough
 * that a real feature would draw as a legible box rather than vanish into the aggregate.
 */
export function isCoarseZoom(lat: Lattice, box: Box, wPx: number, hPx: number, minCellsPerPx = DENSITY_MIN_CELLS_PER_PX): boolean {
  const fSpan = box.f1Hz - box.f0Hz, tSpan = box.t1Ns - box.t0Ns;
  if (!(fSpan > 0) || !(tSpan > 0) || !(wPx > 0) || !(hPx > 0) || !(lat.f0Hz > 0) || !(lat.t0Ns > 0)) return false;
  const cellsPerPxF = fSpan / (wPx * lat.f0Hz);
  const cellsPerPxT = tSpan / (hPx * lat.t0Ns);
  return cellsPerPxF >= minCellsPerPx || cellsPerPxT >= minCellsPerPx;
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
  /** [[DENSITY_MIN_CELLS_PER_PX]]'s override, for tests. */
  readonly minCellsPerPx?: number;
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
 * the count — but ONLY when [[isCoarseZoom]] says this pane is genuinely coarse-zoomed. Past that
 * this draws nothing at all: this layer never competes with the `detections` boxes it exists to
 * stand in for.
 */
export function densityQuads(
  tiles: readonly DensityTile[], paneBox: Box, rect: PaneRect, lat: Lattice, style: DensityStyle = {},
): OverlayQuad[] {
  if (!isCoarseZoom(lat, paneBox, rect.w, rect.h, style.minCellsPerPx)) return [];
  const k = style.dpr && style.dpr > 0 ? style.dpr : 1;
  const rgb = style.rgba ?? DENSITY_MARK;
  const capCount = Math.max(1, style.capCount ?? DENSITY_CAP_COUNT);
  const maxAlpha = style.maxAlpha ?? DENSITY_MAX_ALPHA;
  const hatchPeriodPx = style.hatchPeriodPx ?? DENSITY_HATCH_PERIOD_CSS_PX;
  const hatchOnPx = style.hatchOnPx ?? DENSITY_HATCH_ON_CSS_PX;
  const out: OverlayQuad[] = [];
  for (const tile of tiles) {
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

// ## When a density tile is asked for again (T-810, review fixes 2 and 3)
//
// `/api/tiles/events` is "not a tile channel: counts change with every append to the observation
// ledger" (docs/api.md), so fetching each address once froze a live-edge tile's counts until the
// pane crossed a tile boundary, and left a failed fetch empty for as long. Nor may a tile be SEALED
// once the edge passes its end, the way the tile cache seals spectrum tiles: an event is counted in
// the cell of its START, most observations are written when a track CLOSES (after the tracker's idle
// timeout, 60 s by default and adaptive up to an hour), and a start can move earlier — so counts
// keep arriving in tiles the edge left long ago. The rule is therefore:
//
//  - a copy records **the live edge at the instant it was asked for** (`edgeAtFetchNs`);
//  - while its pane is **following**, it is stale when the edge has moved since (`edgeAtFetchNs <
//    edge`, i.e. rows arrived) AND at least [[DENSITY_LIVE_REFRESH_MS]] has passed — every tile on
//    screen, with no cap at the tile's end. Bounded by [[DENSITY_MAX_TILES_PER_PANE]] per pane per
//    period, one batch in flight per pane, and [[sharedDensityReader]] sharing one request between
//    panes asking for the same address;
//  - a **frozen** pane is a view over the past: it asks once and keeps its own copy. Copies are held
//    **per pane**, so a frozen pane never picks up the counts a following pane refreshed;
//  - a failed fetch keeps whatever copy was in hand and is retried with exponential backoff
//    ([[DENSITY_RETRY_BASE_MS]] doubling to [[DENSITY_RETRY_MAX_MS]]) — never recorded as fetched.

/** The least wall time between two asks for one density address by one following pane. */
export const DENSITY_LIVE_REFRESH_MS = 5000;
/** First retry delay after a failed density fetch; doubles per consecutive failure. */
export const DENSITY_RETRY_BASE_MS = 1000;
/** The longest a failed density address waits before its next try. */
export const DENSITY_RETRY_MAX_MS = 30_000;

interface DensityEntry {
  tile: DensityTile | null;
  /** The live edge when the copy in hand was asked for; `-Infinity` before any copy. */
  edgeAtFetchNs: number;
  /** Wall ms of the last ask (success or failure). */
  askedAtMs: number;
  failures: number;
}

const entryKey = (pane: string, a: TileAddr): string => `${pane}|${addrSpelling(a)}`;

/** Per-pane, per-address density copies and the one rule for when each is asked for again. Pure:
 * no network, no clock — the host passes `nowMs` and the edge, and reports each answer back. */
export class DensityFetches {
  private readonly map = new Map<string, DensityEntry>();

  /** The addresses of `addrs` that pane `pane` needs a request for now. */
  due(pane: string, addrs: readonly TileAddr[], edgeNs: number, following: boolean, nowMs: number): TileAddr[] {
    return addrs.filter((a) => {
      const e = this.map.get(entryKey(pane, a));
      if (!e) return true;
      if (e.failures > 0) return nowMs - e.askedAtMs >= Math.min(DENSITY_RETRY_MAX_MS, DENSITY_RETRY_BASE_MS * 2 ** (e.failures - 1));
      if (!following) return false;
      return e.edgeAtFetchNs < edgeNs && nowMs - e.askedAtMs >= DENSITY_LIVE_REFRESH_MS;
    });
  }

  /** Record a successful answer for pane `pane`'s `a`, asked when the edge stood at `edgeAtFetchNs`.
   *
   * A non-finite edge — the host has no edge to name yet, before the first row — is stored as
   * `-Infinity`, never as `NaN` (T-927, follow-up 4): `NaN < edge` is FALSE, so a `NaN` fetch edge
   * makes [[due]]'s "rows arrived since" test read "nothing to ask about" for ever, and this copy is
   * never refreshed again — an accidental seal, the very thing review fix 3 removed. `-Infinity` is
   * the truth of that case: asked before any edge was known, so stale as soon as one is. */
  succeeded(pane: string, a: TileAddr, tile: DensityTile, edgeAtFetchNs: number, nowMs: number): void {
    const edge = Number.isFinite(edgeAtFetchNs) ? edgeAtFetchNs : Number.NEGATIVE_INFINITY;
    this.map.set(entryKey(pane, a), { tile, edgeAtFetchNs: edge, askedAtMs: nowMs, failures: 0 });
  }

  /** Record a failed ask: the copy in hand (if any) is kept, and a retry is scheduled. */
  failed(pane: string, a: TileAddr, nowMs: number): void {
    const k = entryKey(pane, a);
    const e = this.map.get(k);
    this.map.set(k, { tile: e?.tile ?? null, edgeAtFetchNs: e?.edgeAtFetchNs ?? Number.NEGATIVE_INFINITY, askedAtMs: nowMs, failures: (e?.failures ?? 0) + 1 });
  }

  /** Pane `pane`'s copies for `addrs`, in their order. */
  tiles(pane: string, addrs: readonly TileAddr[]): DensityTile[] {
    const out: DensityTile[] = [];
    for (const a of addrs) { const t = this.map.get(entryKey(pane, a))?.tile; if (t) out.push(t); }
    return out;
  }

  /** Drop every copy but those `keep` names — each a `[pane, addr]` a pane currently shows. */
  retain(keep: Iterable<readonly [string, TileAddr]>): void {
    const k = new Set<string>();
    for (const [p, a] of keep) k.add(entryKey(p, a));
    for (const key of [...this.map.keys()]) if (!k.has(key)) this.map.delete(key);
  }
}

/** One shared read's answer: the tile (`null` when it failed or did not parse) **and the live edge
 * the request that fetched it was ISSUED at** — which is not the edge of the tick that joined it. */
export interface DensityAnswer {
  readonly tile: DensityTile | null;
  readonly edgeAtFetchNs: number;
}

/**
 * The density reader the host polls with: one request per address in flight, SHARED by every pane
 * that asks for it meanwhile, so two following panes due for the same address in one tick send one
 * `GET`, not two. `get` is the host's read (a URL in, a body out); this file never reaches the
 * network itself. A failed or unparseable answer resolves a `null` tile (the caller retries it).
 *
 * The answer carries `edgeAtFetchNs` — the `edgeNs` of the ask that **issued** the request — because
 * a pane joining a request in flight must record the copy at the edge it was actually taken at, not
 * at its own later tick (T-927, follow-up 2). Recording the joining tick's edge claims a snapshot
 * seconds fresher than it is, and `DensityFetches.due`'s "rows arrived since" test then skips the
 * refresh that would have caught the counts that arrived in between.
 */
export function sharedDensityReader(get: (url: string) => Promise<unknown>): (a: TileAddr, edgeNs: number) => Promise<DensityAnswer> {
  const inflight = new Map<string, Promise<DensityAnswer>>();
  return (a, edgeNs) => {
    const k = addrSpelling(a);
    let q = inflight.get(k);
    if (!q) {
      q = get(densityUrl(a))
        .then((body) => ({ tile: parseDensityTile(a, body), edgeAtFetchNs: edgeNs }), () => ({ tile: null, edgeAtFetchNs: edgeNs }))
        .finally(() => { inflight.delete(k); });
      inflight.set(k, q);
    }
    return q;
  };
}
