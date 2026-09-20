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
  /**
   * The coarsest frequency level this server will answer, when the route states one
   * (`axes.frequency.max_level`). Absent, the ceiling is `levelsF - 1` — the declared axis.
   *
   * It is a **separate number from `levelsF`** because the two answer different questions: how many
   * levels the address lattice *names*, and how far up it can actually be *read*. On a server whose
   * store ladder is shallower than the address lattice those differ, and the client must obey the
   * smaller one or it addresses a node that is named and cannot be built (T-480).
   */
  readonly maxLevelF?: number;
  /** The same on the time axis. Deliberately separate: the axes are levelled independently. */
  readonly maxLevelT?: number;
  /** The addressable frequency extent, Hz. Defaults to [[ADDRESSABLE_HZ]]. */
  readonly maxFHz?: number;
  /** The addressable time extent, ns from the epoch. Defaults to [[ADDRESSABLE_NS]]. */
  readonly maxTNs?: number;
}

/**
 * The route's own addressable spectrum: `f_index` is refused once a tile's high edge passes this
 * (`hk-api/src/tiles.rs`, *"f_index is outside the addressable spectrum (0 .. 1 THz)"*).
 */
export const ADDRESSABLE_HZ = 1e12;
/**
 * The route's own addressable time, ns since the epoch: `t_index` is refused once a tile's end
 * passes `i64::MAX / 2` (*"t_index is outside the addressable time range"*).
 */
export const ADDRESSABLE_NS = 4_611_686_018_427_387_904;

/** The coarsest frequency level that may be addressed — the declared axis, or the stated ceiling. */
export const levelCapF = (lat: Lattice): number =>
  Math.max(0, Math.min(lat.levelsF - 1, lat.maxLevelF ?? lat.levelsF - 1));
/** The same on the time axis, against its own ladder. */
export const levelCapT = (lat: Lattice): number =>
  Math.max(0, Math.min(lat.levelsT - 1, lat.maxLevelT ?? lat.levelsT - 1));

/** The highest `f_index` whose tile still lies inside the addressable spectrum, at `level`. */
export const maxFIndex = (lat: Lattice, level: number): number =>
  Math.max(0, Math.floor((lat.maxFHz ?? ADDRESSABLE_HZ) / fTileHz(lat, level)) - 1);
/** The highest `t_index` whose tile still lies inside the addressable time range, at `level`. */
export const maxTIndex = (lat: Lattice, level: number): number =>
  Math.max(0, Math.floor((lat.maxTNs ?? ADDRESSABLE_NS) / tTileNs(lat, level)) - 1);

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
  return clampLevel(levelDemandF(lat, hzPerPx), levelCapF(lat));
}

/** The same on the time axis, against its own ladder. Deliberately a separate function (§8.2). */
export function levelForNsPerPx(lat: Lattice, nsPerPx: number): number {
  return clampLevel(levelDemandT(lat, nsPerPx), levelCapT(lat));
}

/**
 * The level a viewport **asks for**, before the lattice's ceiling is applied — the honest statement
 * of "how far out this zoom wants to go", and the thing a chrome readout would name when a view is
 * zoomed past what the surface can be read at.
 *
 * Exported because it is also how the address tests prove they are not vacuous: a clamp that is
 * never reached proves nothing, so the sweep asserts these demands genuinely exceed the cap.
 */
export const levelDemandF = (lat: Lattice, hzPerPx: number): number =>
  Math.ceil(Math.log2(Math.max(hzPerPx, Number.MIN_VALUE) / lat.f0Hz));
/** The same on the time axis. */
export const levelDemandT = (lat: Lattice, nsPerPx: number): number =>
  Math.ceil(Math.log2(Math.max(nsPerPx, Number.MIN_VALUE) / lat.t0Ns));

function clampLevel(l: number, cap: number): number {
  if (!Number.isFinite(l)) return 0;
  return Math.max(0, Math.min(Math.max(0, cap), Math.ceil(l)));
}

/** Per-axis levels for a pane of `wPx × hPx` showing `box`. The two axes are resolved separately. */
export function levelsFor(lat: Lattice, box: Box, wPx: number, hPx: number): { levelF: number; levelT: number } {
  return {
    levelF: levelForHzPerPx(lat, (box.f1Hz - box.f0Hz) / Math.max(1, wPx)),
    levelT: levelForNsPerPx(lat, (box.t1Ns - box.t0Ns) / Math.max(1, hPx)),
  };
}

/**
 * **Is this address a node of `lat`?** The whole of T-480's definition of done, as a pure predicate
 * over an address and the lattice it claims to be in — so a test can quantify over the zoom/pan
 * range and ask *"can any address outside the lattice be produced?"* rather than checking three
 * examples.
 *
 * Written from the lattice's declared axes, **not** from the clamp: if it were derived from
 * [[clampAddr]] it would only prove the clamp agrees with itself.
 */
export function inLattice(lat: Lattice, a: TileAddr): boolean {
  const int = (v: number) => Number.isSafeInteger(v);
  return (
    a.scheme === lat.scheme && a.cells === lat.cells &&
    int(a.levelF) && a.levelF >= 0 && a.levelF <= levelCapF(lat) &&
    int(a.levelT) && a.levelT >= 0 && a.levelT <= levelCapT(lat) &&
    int(a.fIndex) && a.fIndex >= 0 && a.fIndex <= maxFIndex(lat, a.levelF) &&
    int(a.tIndex) && a.tIndex >= 0 && a.tIndex <= maxTIndex(lat, a.levelT)
  );
}

/**
 * The nearest node of `lat` to `a` — **per axis, against that axis's own extent**.
 *
 * This is CLAUDE.md's discretized-navigation rule applied to addressing: *"zoom/pan and region
 * select resolve only to realizable configurations and snap to the nearest one"*. An address off
 * the lattice is that invariant broken in the client, and it is why the route ends up being asked a
 * question that has no answer (T-480; seen in the wild as `level_f=10, t_index=218471`).
 *
 * Nothing here couples the axes: the frequency level is clamped against the frequency ceiling and
 * the time level against the time ceiling, and clamping one never moves the other. T-434 de-welded
 * them in the store, T-438 on the wire and T-440 per pane; a clamp that tied them would put the
 * weld back one layer up.
 *
 * **Clamping the LEVEL is a snap to a realizable node; clamping an INDEX is not** — a different
 * index is a different place, so an index past the end of its axis is only ever *dropped* by
 * [[tilesFor]]. This function exists for the single-address paths ([[ancestor]]) where the caller
 * already knows which place it means.
 */
export function clampAddr(lat: Lattice, a: TileAddr): TileAddr {
  const levelF = clampLevel(a.levelF, levelCapF(lat));
  const levelT = clampLevel(a.levelT, levelCapT(lat));
  const fIndex = Math.max(0, Math.min(maxFIndex(lat, levelF), Math.floor(a.fIndex) || 0));
  const tIndex = Math.max(0, Math.min(maxTIndex(lat, levelT), Math.floor(a.tIndex) || 0));
  return { ...a, scheme: lat.scheme, cells: lat.cells, levelF, levelT, fIndex, tIndex };
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
 * Indices outside their axis are dropped rather than clamped — the route refuses a negative index,
 * and clamping would silently address a different tile. **The LEVELS are clamped** (T-480): a level
 * is a resolution, so the nearest realizable one is the same place seen at a resolution that
 * exists, and this is the derivation, so a caller that computes `levelF + 1` for a parent row
 * cannot leave the lattice by doing so.
 */
export function tilesFor(lat: Lattice, box: Box, levelF: number, levelT: number, device = "any"): TileAddr[] {
  levelF = clampLevel(levelF, levelCapF(lat));
  levelT = clampLevel(levelT, levelCapT(lat));
  const fw = fTileHz(lat, levelF), tw = tTileNs(lat, levelT);
  if (!(fw > 0) || !(tw > 0) || !(box.f1Hz > box.f0Hz) || !(box.t1Ns > box.t0Ns)) return [];
  const fMax = maxFIndex(lat, levelF), tMax = maxTIndex(lat, levelT);
  const f0 = Math.max(0, Math.floor(box.f0Hz / fw)), f1 = Math.min(fMax, Math.floor((box.f1Hz - 1e-9) / fw));
  const t0 = Math.max(0, Math.floor(box.t0Ns / tw)), t1 = Math.min(tMax, Math.floor((box.t1Ns - 1) / tw));
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
 *
 * `df`/`dt` are **requests**, clamped per axis against that axis's own ceiling, and the index is
 * halved by the step actually taken — so a fallback always names a node that exists and always
 * still contains `a` (T-480). Asking to coarsen past the top of one axis leaves the other alone.
 */
export function ancestor(lat: Lattice, a: TileAddr, df: number, dt: number): TileAddr {
  const base = clampAddr(lat, a);
  const ef = Math.max(0, Math.min(Math.floor(df) || 0, levelCapF(lat) - base.levelF));
  const et = Math.max(0, Math.min(Math.floor(dt) || 0, levelCapT(lat) - base.levelT));
  return {
    ...base,
    levelF: base.levelF + ef,
    levelT: base.levelT + et,
    fIndex: Math.floor(base.fIndex / 2 ** ef),
    tIndex: Math.floor(base.tIndex / 2 ** et),
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
      // The CEILING, not the declared axis length: on a server whose store ladder is shallower than
      // the address lattice those differ, and a fallback that stepped past the ceiling would be a
      // second address the route cannot answer, asked while standing in for the first (T-480).
      if (a.levelF + df > levelCapF(lat) || a.levelT + dt > levelCapT(lat)) continue;
      out.push({ addr: ancestor(lat, a, df, dt), d: df + dt });
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

// ---------------------------------------------------------------------------------------------
// T-505: the two tiers, and which one a viewport is drawn from.
// ---------------------------------------------------------------------------------------------

/**
 * Which lattice a pane drew from.
 *
 * - `detail` — the fine view lattice, whose node (0, 0) is the live chain's own cell. It answers
 *   the tuned window at the resolution the front end measured.
 * - `overview` — `scheme=overview`, the same de-welded construction anchored on the
 *   **spectrum-history** pyramid, whose cells are absolutely coarse. It answers wide-and-long
 *   viewports.
 *
 * **This is CLAUDE.md's honesty tiers made real in the tile SOURCE, not in a label.** Until T-505
 * one lattice was asked to be both, and it cannot be: `axes.*.max_level` bounds level *indices*,
 * never cell size, so a floor N doublings finer shrinks the coarsest *addressable* tile by exactly
 * 2^N. T-484 spent six doublings of cell AREA on fidelity — 6250 Hz × 1 s to the display plan's own
 * 2343.75 Hz × 40.1 ms — and the 6 GHz × 30 min minimap went from 32 addresses to **1780** behind a
 * four-slot in-flight cap; nothing arrived, and the map went dark. Patch the display to 4096 bins
 * and it is **7031**. No lattice depth recovers it (T-501 swept it), because the binding constraint
 * is the *work* of folding a tile out of the store's coarsest cell.
 */
export type ViewTier = "detail" | "overview";

/** The two lattices a surface draws from, each read back off its own tile answer. */
export interface LatticeSet {
  readonly detail: Lattice;
  readonly overview: Lattice;
}

/**
 * **The stated tile budget, per viewport, per frame.**
 *
 * `tilesFor` has no budget of its own: `Surface.render` walks every address a viewport enumerates,
 * every frame, and `TileCache` queues each miss behind the route's four-slot in-flight cap under a
 * 96 MB LRU. So the number of addresses a viewport enumerates is a **product property** — it is the
 * number that went dark — and it needs a bound that is asserted rather than hoped for.
 *
 * 100 is generous and measured: the shipped floor's deepest viewport (6 GHz over a 30-minute
 * record) needs 32, and a pane at one cell per pixel on a 3520 × 2000 rig needs ~135 *across every
 * pane on the screen* (`TileCacheOptions`), so a per-viewport 100 leaves room for the fine tier to
 * keep answering everything it can actually deliver.
 */
export const VIEWPORT_TILE_BUDGET = 100;

/** One tier evaluated against one viewport: what it would draw, and what it would cost. */
export interface TierChoice {
  readonly tier: ViewTier;
  readonly lat: Lattice;
  readonly levelF: number;
  readonly levelT: number;
  /** The addresses this viewport enumerates on `lat`, least-wanted first ([[tilesFor]]). */
  readonly addrs: readonly TileAddr[];
  /**
   * The viewport asked for a level past this lattice's ceiling, so the level actually used is
   * finer than the screen can show — which is both wasted resolution and, at the coarse end, the
   * tile explosion. Reported so a pane can say *why* it left the fine tier.
   */
  readonly clamped: boolean;
}

function evaluate(lat: Lattice, tier: ViewTier, box: Box, wPx: number, hPx: number, device: string): TierChoice {
  const hzPerPx = (box.f1Hz - box.f0Hz) / Math.max(1, wPx);
  const nsPerPx = (box.t1Ns - box.t0Ns) / Math.max(1, hPx);
  const { levelF, levelT } = levelsFor(lat, box, wPx, hPx);
  return {
    tier, lat, levelF, levelT,
    addrs: tilesFor(lat, box, levelF, levelT, device),
    clamped: levelDemandF(lat, hzPerPx) > levelCapF(lat) || levelDemandT(lat, nsPerPx) > levelCapT(lat),
  };
}

/**
 * **Which tier answers this viewport** — decided by the budget, not by a span threshold.
 *
 * The rule, and it is one sentence: *the detail tier answers unless it cannot draw the viewport
 * inside [[VIEWPORT_TILE_BUDGET]], and then the overview tier does.* That is deliberately the
 * budget itself rather than a span or a zoom level, because the budget is the property that broke
 * — a threshold would be a second number that has to be kept in step with the one that matters,
 * and the two would drift exactly as the ceiling and the floor did.
 *
 * Two consequences worth stating:
 *
 * - **On the shipped floor nothing moves.** Every viewport the surface can open is inside the
 *   budget on the detail lattice (32 at the deepest), so this function returns `detail` for all of
 *   them and the overview tier is never fetched. It becomes load-bearing exactly when the fidelity
 *   floor lands, which is the point: the prerequisite is in place before the change that needs it.
 * - **The live/tuned window stays on the detail tier even at the fine floor** (15 addresses for
 *   2.4 MHz × 20 s), so the resolution T-483 measured as missing is delivered where it is looked
 *   at. What moves to the overview tier is the wide sweep and the deep scrub, which are survey
 *   questions and are answered — and *stated* — as survey.
 *
 * Total by construction: if neither tier fits the budget the cheaper one is used, so this always
 * returns the smallest enumeration available rather than failing.
 */
export function tierFor(set: LatticeSet, box: Box, wPx: number, hPx: number, device = "any"): TierChoice {
  const d = evaluate(set.detail, "detail", box, wPx, hPx, device);
  if (d.addrs.length <= VIEWPORT_TILE_BUDGET) return d;
  const o = evaluate(set.overview, "overview", box, wPx, hPx, device);
  return o.addrs.length < d.addrs.length ? o : d;
}

/** A set in which both tiers are the same lattice: what a host has before the overview probe
 * answers, and what a server with one pyramid honestly offers. */
export const oneTier = (lat: Lattice): LatticeSet => ({ detail: lat, overview: lat });

/** Does `a`'s extent intersect `box`? Used to cancel queued tiles for a viewport the user has left. */
export function intersects(lat: Lattice, a: TileAddr, box: Box): boolean {
  const e = extentOf(lat, a);
  return e.f1Hz > box.f0Hz && e.f0Hz < box.f1Hz && e.t1Ns > box.t0Ns && e.t0Ns < box.t1Ns;
}

/**
 * The lattice a tile response describes. The response states the cell size **at the level it was
 * asked for**, so level 0 is that cell halved `level` times — the client never picks the floor.
 *
 * `max_level` is read per axis when the route states one, and left undefined when it does not, so
 * an invented ceiling is never indistinguishable from a reported one (the rule `surfaceBounds`
 * already follows for the surface's extent).
 */
export function latticeFrom(resp: {
  key: { scheme: string | number; level_f: number; level_t: number; cells: number };
  axes: {
    frequency: { levels: number; cell_hz: number; max_level?: number };
    time: { levels: number; cell_s: number; max_level?: number };
  };
}, cells = resp.key.cells): Lattice {
  return {
    scheme: String(resp.key.scheme),
    cells,
    f0Hz: resp.axes.frequency.cell_hz / 2 ** resp.key.level_f,
    t0Ns: (resp.axes.time.cell_s * 1e9) / 2 ** resp.key.level_t,
    levelsF: resp.axes.frequency.levels,
    levelsT: resp.axes.time.levels,
    ...(statedLevel(resp.axes.frequency.max_level) !== null ? { maxLevelF: statedLevel(resp.axes.frequency.max_level)! } : {}),
    ...(statedLevel(resp.axes.time.max_level) !== null ? { maxLevelT: statedLevel(resp.axes.time.max_level)! } : {}),
  };
}

const statedLevel = (v: unknown): number | null =>
  typeof v === "number" && Number.isSafeInteger(v) && v >= 0 ? v : null;
