// The unified surface's addressing: the view lattice, and the tiles a viewport box covers (T-440,
// docs/16 §8.2/§8.3, `GET /api/tiles`).
//
// **`level_f` and `level_t` are two numbers and nothing here derives one from the other.** T-434
// de-welded them in the store and T-438 de-welded them on the wire; re-welding them in the client
// would put the defect back one layer up. So there are two cell ladders, two level-choosing
// functions, and a tile address that carries both. "Constant pixel density" is the *slippy-map*
// invariant — uniform in-level density **per pane** — not one scale shared between a 6 GHz axis and
// a retention window, which is the thing that cannot be made to work.
//
// Everything in this file is arithmetic over numbers the backend served (`axes`, `extent`). No RF
// constant, no level policy, no fold: those are the route's (docs/api.md, "the budget is a fold
// target, never a level selector").

/** How a tile's grid is addressed. Every part of it is in the key the route is keyed by. */
export interface TileAddr {
  /** Whose coverage decides this tile's grey (`any` = the union). Part of the key: coverage is device-local. */
  readonly device: string;
  /** The lattice the address is expressed in (`view`, or a store scheme id as a string). */
  readonly scheme: string;
  readonly levelF: number;
  readonly levelT: number;
  readonly fIndex: number;
  readonly tIndex: number;
  /** Cells per tile edge (8…256). It scales the tile's *extent*, not its resolution, so it is part of the key. */
  readonly cells: number;
}

/**
 * The view lattice: one cell size per axis at level 0, each axis doubling **independently**.
 *
 * Node (0, 0) is the open pyramid's own level-0 cell (T-438's fix for T-437 finding F1 — docs/16
 * §6.2's 100 kHz × 128 s put a 120 s retention window inside a single time cell, so `level_t`
 * pinned at 0 and the de-welding bought nothing on the axis it exists for). The client never
 * chooses that floor; it reads it back off a tile response.
 */
export interface Lattice {
  readonly scheme: string;
  readonly cells: number;
  /** Level-0 frequency cell, Hz. */
  readonly f0Hz: number;
  /** Level-0 time cell, ns. */
  readonly t0Ns: number;
  /** Number of frequency levels (`axes.frequency.levels`). */
  readonly levelsF: number;
  /** Number of time levels (`axes.time.levels`). */
  readonly levelsT: number;
}

export const fCellHz = (lat: Lattice, level: number): number => lat.f0Hz * 2 ** level;
export const tCellNs = (lat: Lattice, level: number): number => lat.t0Ns * 2 ** level;
/** The frequency extent of one whole tile at `level`, Hz — `cells` cells wide. */
export const fTileHz = (lat: Lattice, level: number): number => fCellHz(lat, level) * lat.cells;
/** The time extent of one whole tile at `level`, ns. */
export const tTileNs = (lat: Lattice, level: number): number => tCellNs(lat, level) * lat.cells;

/** A viewport box in the surface's own absolute coordinates: Hz, and absolute capture time in ns. */
export interface Box {
  readonly f0Hz: number;
  readonly f1Hz: number;
  readonly t0Ns: number;
  readonly t1Ns: number;
}

/**
 * The finest frequency level whose cell is still at least one pixel wide — so a pane never
 * magnifies a cell it did not measure, and never asks for sixteen times the tiles it can show.
 *
 * Clamped into the lattice's own axis: a demand off the end is answered by the coarsest (or
 * finest) level that exists, because the route 404s a node that does not, and a client that
 * requested one would be asking the surface to lie about which ladder it is on.
 */
export function levelForHzPerPx(lat: Lattice, hzPerPx: number): number {
  return clampLevel(Math.ceil(Math.log2(Math.max(hzPerPx, Number.MIN_VALUE) / lat.f0Hz)), lat.levelsF);
}

/** The same on the time axis, against its own ladder. Deliberately a separate function (§8.2). */
export function levelForNsPerPx(lat: Lattice, nsPerPx: number): number {
  return clampLevel(Math.ceil(Math.log2(Math.max(nsPerPx, Number.MIN_VALUE) / lat.t0Ns)), lat.levelsT);
}

function clampLevel(l: number, levels: number): number {
  if (!Number.isFinite(l)) return 0;
  return Math.max(0, Math.min(Math.max(0, levels - 1), Math.ceil(l)));
}

/** Per-axis levels for a pane of `wPx × hPx` showing `box`. The two axes are resolved separately. */
export function levelsFor(lat: Lattice, box: Box, wPx: number, hPx: number): { levelF: number; levelT: number } {
  return {
    levelF: levelForHzPerPx(lat, (box.f1Hz - box.f0Hz) / Math.max(1, wPx)),
    levelT: levelForNsPerPx(lat, (box.t1Ns - box.t0Ns) / Math.max(1, hPx)),
  };
}

/** The absolute extent of one tile. Its origin is 0 Hz and the Unix epoch, exactly as the route's. */
export function extentOf(lat: Lattice, a: TileAddr): Box {
  const fw = fTileHz(lat, a.levelF), tw = tTileNs(lat, a.levelT);
  return { f0Hz: a.fIndex * fw, f1Hz: (a.fIndex + 1) * fw, t0Ns: a.tIndex * tw, t1Ns: (a.tIndex + 1) * tw };
}

/**
 * Every tile of `(levelF, levelT)` that intersects `box`, ordered **least-wanted first**: oldest
 * time row first, and within a row the columns furthest from the centre first.
 *
 * The order is for the fetch queue, which is **LIFO** (docs/16 §5.5 cap (1)), so the tile enqueued
 * *last* is fetched *first*. Emitting the newest row's centre column last therefore fetches the
 * middle of the live edge first — where the eye is — and leaves the corners for the tail. Getting
 * this backwards is not a nuance at 11.4 ms a tile: it is the difference between a screen that
 * fills from the middle out and one that fills from a corner nobody is looking at.
 *
 * Indices below zero are dropped rather than clamped — the route refuses a negative index, and
 * clamping would silently address a different tile.
 */
export function tilesFor(lat: Lattice, box: Box, levelF: number, levelT: number, device = "any"): TileAddr[] {
  const fw = fTileHz(lat, levelF), tw = tTileNs(lat, levelT);
  if (!(fw > 0) || !(tw > 0) || !(box.f1Hz > box.f0Hz) || !(box.t1Ns > box.t0Ns)) return [];
  const f0 = Math.max(0, Math.floor(box.f0Hz / fw)), f1 = Math.floor((box.f1Hz - 1e-9) / fw);
  const t0 = Math.max(0, Math.floor(box.t0Ns / tw)), t1 = Math.floor((box.t1Ns - 1) / tw);
  const out: TileAddr[] = [];
  const fMid = (f0 + f1) / 2;
  const cols: number[] = [];
  for (let f = f0; f <= f1; f++) cols.push(f);
  cols.sort((a, b) => Math.abs(b - fMid) - Math.abs(a - fMid)); // furthest first: LIFO reverses it
  for (let t = t0; t <= t1; t++) {
    for (const f of cols) out.push({ device, scheme: lat.scheme, levelF, levelT, fIndex: f, tIndex: t, cells: lat.cells });
  }
  return out;
}

/**
 * The tile `df`/`dt` levels coarser that contains `a`. A coarser cell covers twice the span per
 * level on that axis, so the index halves — **per axis, independently**.
 *
 * This is what makes "not loaded" drawable without grey (docs/16 §5.5's most important sentence):
 * while a finer tile is missing the view draws a resident ancestor upscaled and says so.
 */
export function ancestor(a: TileAddr, df: number, dt: number): TileAddr {
  return {
    ...a,
    levelF: a.levelF + df,
    levelT: a.levelT + dt,
    fIndex: Math.floor(a.fIndex / 2 ** df),
    tIndex: Math.floor(a.tIndex / 2 ** dt),
  };
}

/**
 * Ancestors of `a`, nearest first, within the lattice: (1,0), (0,1), (1,1), (2,1)… — ordered by
 * total level distance so the closest-resolution fallback is preferred, and capped at `maxSteps`
 * per axis.
 */
export function ancestorsOf(lat: Lattice, a: TileAddr, maxSteps = 4): TileAddr[] {
  const out: { addr: TileAddr; d: number }[] = [];
  for (let df = 0; df <= maxSteps; df++) {
    for (let dt = 0; dt <= maxSteps; dt++) {
      if (df === 0 && dt === 0) continue;
      if (a.levelF + df >= lat.levelsF || a.levelT + dt >= lat.levelsT) continue;
      out.push({ addr: ancestor(a, df, dt), d: df + dt });
    }
  }
  out.sort((x, y) => x.d - y.d);
  return out.map((x) => x.addr);
}

/** The cache key. Every part of the route's key is in it — including `device`, `scheme` and `cells`. */
export const keyOf = (a: TileAddr): string =>
  `${a.device}|${a.scheme}|${a.levelF}|${a.levelT}|${a.fIndex}|${a.tIndex}|${a.cells}`;

/** The request this client builds for `a` (ui/test asserts the request, not only the response). */
export function tileUrl(a: TileAddr, path = "/api/tiles"): string {
  const q = new URLSearchParams({
    level_f: String(a.levelF),
    level_t: String(a.levelT),
    f_index: String(a.fIndex),
    t_index: String(a.tIndex),
  });
  if (a.scheme !== "view") q.set("scheme", a.scheme);
  if (a.device !== "any") q.set("device", a.device);
  if (a.cells !== 256) q.set("cells", String(a.cells));
  return `${path}?${q.toString()}`;
}

/** Does `a`'s extent intersect `box`? Used to cancel queued tiles for a viewport the user has left. */
export function intersects(lat: Lattice, a: TileAddr, box: Box): boolean {
  const e = extentOf(lat, a);
  return e.f1Hz > box.f0Hz && e.f0Hz < box.f1Hz && e.t1Ns > box.t0Ns && e.t0Ns < box.t1Ns;
}

/**
 * The lattice a tile response describes. The response states the cell size **at the level it was
 * asked for**, so level 0 is that cell halved `level` times — the client never picks the floor.
 */
export function latticeFrom(resp: {
  key: { scheme: string; level_f: number; level_t: number; cells: number };
  axes: { frequency: { levels: number; cell_hz: number }; time: { levels: number; cell_s: number } };
}): Lattice {
  return {
    scheme: String(resp.key.scheme),
    cells: resp.key.cells,
    f0Hz: resp.axes.frequency.cell_hz / 2 ** resp.key.level_f,
    t0Ns: (resp.axes.time.cell_s * 1e9) / 2 ** resp.key.level_t,
    levelsF: resp.axes.frequency.levels,
    levelsT: resp.axes.time.levels,
  };
}
