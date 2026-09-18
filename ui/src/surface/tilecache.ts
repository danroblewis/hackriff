// The **shared** tile-texture LRU (T-440, docs/16 §5.5 and §8.3).
//
// This is why the surface is *one* WebGL2 context rather than one per pane: textures are not
// shareable between contexts, so a per-pane context means a per-pane cache, and a tile visible in
// eight panes would be uploaded eight times. T-437 measured both sides of that: 95 distinct keys →
// **95 uploads**, up to 8 panes on one tile, **18.68 MB** resident against **149.44 MB** for one
// cache per pane. The architecture follows from the requirement, not from taste.
//
// Three things this file is careful about, in order of how much they matter:
//
//  1. **A budget can never manufacture grey** (T-437 F3). [[acquire]] answers with a *residency* —
//     `resident` or `pending` — and `pending` is drawn as its own mark. Nothing here produces a
//     cell state, so eviction cannot be spelled as "the radio never looked".
//  2. **The budget is bytes, not tiles.** `cells` is a request parameter (8…256), so tiles are not
//     uniform and a count would be off by up to 1024× between a 256² and an 8² tile. §5.5 chose a
//     count only because §6 fixed `nt` as well as `nf`; the route reopened that by making `cells`
//     addressable, so the honest budget is the one it was a proxy for.
//  3. **Production is the cost, by three orders of magnitude.** Rendering is p95 2.2 ms for 48
//     panes; a 256² tile costs the server **11.4 ms** (T-438, measured), so a 208-tile screen is
//     ~2.4 s. Everything expensive here is therefore about *order* and *not asking twice*: a LIFO
//     queue so a fast pan serves where the user now is, cancellation for viewports they have left,
//     an in-flight cap matching the server's own, and one request per key however many panes want it.
//
// ## T-454: why the cap did not hold, and what replaced it
//
// The user saw the route's own `503` text. The cap and the cancellation in this file were both
// running — T-450 measured 26 tiles resident against **17 268 cancelled in 75 s** — and that number
// *is* the defect rather than evidence against it. Three things were wrong, all of them the same
// mistake: **treating a local count as if it described the server.**
//
//  1. **An abort does not give the server its slot back.** `hk-api` serves on blocking
//     `std::net` threads (`crates/hk-api/src/lib.rs`, "Threads, not tokio"): a closed socket is
//     noticed when the response is *written*, so `tiles_json` keeps its [`TileSlot`] and keeps the
//     history lock until the read finishes — 11.4 ms for a fine tile, **5.2 s** for the map's
//     level-10 one. Aborting freed this client's slot instantly and pumped a replacement, so a drag
//     issued and abandoned requests at frame rate while the server's four slots filled with work
//     nobody was waiting for. The cap here counted 4; the route was holding dozens. So an abandoned
//     request is now **charged to the budget until it is expected to have finished** ([[abandon]]),
//     with the expectation *measured* from completions rather than assumed. That turns the issue
//     rate from "as fast as we can draw" into "as fast as the server is observed to serve".
//  2. **Adopting the named cap was a no-op.** `TILE_MAX_IN_FLIGHT` is a constant 4 and this cache
//     started at 4, so `min(4, 4)` changed nothing — and the number is a **server-wide** budget
//     across every pane, every tab and the bootstrap probe, not an allowance for one client. A
//     refusal is now AIMD: **halve on refusal, recover one slot per [[RECOVER_AFTER]] successes**,
//     with the server's number kept as the *ceiling* it actually is.
//  3. **The coarse viewport could starve the fine ones.** One 5.2 s map tile holding a slot is a
//     quarter of the budget for five seconds. Issue is now shared between viewports ([[nextAddr]]).
//
// ## T-460: the live edge was frozen HERE, and what it costs to unfreeze it
//
// The user's report was *"the live view still does not add waterfall rows for recent samples"*, and
// the backend was measured innocent: the live-edge tile grows (33 024 → 43 520 observed cells over
// 40 s, newest row 128 → 169) and builds from partial data in ~145 ms without waiting. The fault was
// one line of this file. [[acquire]] answers a resident tile **unconditionally**, and the only path
// that could ever drop one — [[invalidateEdge]] — had a single call site in `ui/src`, the retune. No
// timer, no age rule: **a live-edge tile was fetched once and frozen until the pane scrolled into a
// new address, which at `level_t = 0` is once every 256 seconds.** The cells beyond it were drawn
// correctly as T-441's `AWAITING`, so the honesty machinery was working perfectly — the request
// simply never happened.
//
// [[refreshEdge]] is the fix, and the interesting part is not that it re-asks but what stops it
// becoming a poll. A live tile's body is ~19 MB, so a naive 1 Hz refresh would spend the whole
// in-flight budget to deliver a percent of new payload. Three constraints make it cheap **by
// construction** rather than by a tuned constant:
//
//  1. **Only the live-edge address, only for a viewport that is FOLLOWING, only at the level it is
//     actually drawn at.** A frozen pane is a view over data that cannot change, and a parent-pin
//     tile is not on screen. The caller passes the following viewports; nothing else is eligible.
//  2. **Never more often than the data can change.** A tile's newest row is one cell tall, so a
//     re-ask inside `tCellNs(level_t)` cannot return a row the copy in hand does not already have.
//     That makes the cadence a *function of the zoom*: 1 s at level 0, 32 s five levels out, with no
//     policy number to tune.
//  3. **Only into a slot the ordinary queue did not take, and never above a fixed share of measured
//     capacity.** A refresh is revalidation of something already on screen, so it must never be the
//     reason a tile the user is waiting for is not being fetched: it is issued at the *end* of
//     [[pump]], after the ordinary queue has had first refusal on every slot in the budget, and at
//     most once per [[REFRESH_DUTY]] × **the cost of the previous revalidation, measured on the
//     request itself** — so the lane can never take more than 1/[[REFRESH_DUTY]] of what a live-edge
//     tile has just been observed to cost. If tiles get slower the refresh gets rarer on its own; if
//     they get cheaper (T-467 measured 18.4 MB → 1.12 MB, `build_ms` 707 → 14) it speeds up, with no
//     constant to retune. One revalidation is in flight at a time, so this bounds the whole lane.
//
//     **It was "only when the cache is completely idle", and that was measured wrong in a browser.**
//     With T-479's defect present the minimap held three places that could never arrive — a 400
//     re-asked at frame rate — so the queue was *never* empty and the refresh lane never ran once in
//     forty seconds of capture. One stuck place must not be able to freeze the live edge, and a rule
//     whose precondition can be held false forever by something unrelated is not a rule.
//
// **It is one source of rows, not two.** The refresh re-fetches the same address through the same
// `source`, and the answer replaces the resident tile in place ([[insert]]); nothing here invents a
// row, and the stale copy stays on screen and drawn until the new one lands, so the refresh cannot
// flash grey. T-468's row-push route is the durable fix; this restores the guarantee now.

import { extentOf, intersects, keyOf, tCellNs, type Box, type Lattice, type TileAddr } from "./lattice";
import { TileBusyError, type TileData } from "./tile";

/** The GPU side, kept behind an interface so the cache is testable without a GL context. */
export interface TileTextures<T> {
  upload(data: TileData): T;
  destroy(tex: T): void;
}

export interface TileEntry<T> {
  readonly key: string;
  readonly addr: TileAddr;
  readonly data: TileData;
  readonly tex: T;
  lastUsed: number;
  /** Frame number this tile was last pinned on. Pinned tiles are never evicted (§5.5's two pins). */
  pinnedFrame: number;
}

/**
 * One viewport as the cache understands it: the box being looked at **and the levels it is drawing
 * at**. The levels are not decoration — see [[TileCache.setViewports]].
 */
export interface Viewport {
  readonly box: Box;
  readonly levelF: number;
  readonly levelT: number;
}

/**
 * What the cache can answer. **`pending` is not a cell state** — see ui/src/surface/cellrule.ts.
 *
 * `failed` marks a place the route answered *no* to permanently (T-479). It is carried on `pending`
 * rather than as a third kind deliberately: "not loaded" is already structurally distinct from
 * "never observed" — the renderer clears a place to `PENDING` and only a `coverage` state byte can
 * produce grey — so a failed place is already visibly not an unobserved one, which is the claim that
 * matters. The flag says *which* not-loaded it is, for a caller that wants to mark it further.
 */
export type Residency<T> =
  | { readonly kind: "resident"; readonly entry: TileEntry<T> }
  | { readonly kind: "pending"; readonly failed?: boolean };

export interface TileCacheOptions {
  /**
   * Resident decoded bytes. Default **96 MB** = 512 tiles of 256² at 192 KB each.
   *
   * Measured, not guessed: the worst case is every pane at exactly one cell per pixel, which is
   * the densest a pane can ask for. On the spike's rig (3520 × 2000 device px) this addressing
   * produces, counting the parent-level pin:
   *
   * |  panes |  tiles |     MB | + parent pin |     MB |
   * |-------:|-------:|-------:|-------------:|-------:|
   * |      1 |    135 |   25.3 |          175 |   32.8 |
   * |      8 |    159 |   29.8 |          216 |   40.5 |
   * |     16 |    213 |   39.9 |          301 |   56.4 |
   * |     48 |    290 |   54.4 |          427 |   80.1 |
   *
   * which agrees with T-437's measured 208 unique keys / 220 resident at 48 panes. **80 MB is the
   * worst case including both pins, so 96 MB keeps the pins under the budget even at 48 panes** —
   * and that is the property that matters, because it is the case where pins and budget would
   * otherwise fight and something on screen would be missing. Upload is 0.026 ms/tile, so the
   * budget is a memory decision and nothing else.
   */
  budgetBytes?: number;
  /**
   * Outstanding requests **this client starts with**, and the ceiling it may recover to. Defaults
   * to the route's own `TILE_MAX_IN_FLIGHT`.
   *
   * It is a starting point, not the operating value: the route's number is a budget shared by every
   * viewport, every tab and the bootstrap probe, so the share belonging to this cache can only be
   * discovered by being refused. See [[failed]].
   */
  inFlight?: number;
  /** Queue depth. A fast pan can enqueue thousands; the oldest (bottom of the LIFO) are the ones
   * the user has already left, so they are the ones dropped. */
  maxQueue?: number;
  now?: () => number;
  /** Delay after a `503` before the queue is pumped again, ms. Doubles per consecutive refusal. */
  busyBackoffMs?: number;
  /**
   * First guess at how long the server takes to produce one tile, ms — replaced by measurement as
   * soon as anything completes. It only decides how hard the first drag pushes before the cache has
   * seen a single answer, so the default is deliberately nearer the map's 5.2 s than a fine tile's
   * 11.4 ms: guessing *fast* and being wrong is the failure this exists to stop.
   */
  serverMsGuess?: number;
}

export interface TileCacheStats {
  uploads: number;
  hits: number;
  misses: number;
  evictions: number;
  refetchAfterEvict: number;
  requests: number;
  failures: number;
  busyRefusals: number;
  cancelled: number;
  /** In-flight requests this client walked away from. Each one is still costing the route a slot,
   * which is why it is counted separately from the queued drops in `cancelled`. */
  abandoned: number;
  /** Frames on which the pins alone exceeded the budget: the pane is asking for more than the
   * budget can hold, and the honest response is to keep drawing, not to grey anything. */
  overBudgetFrames: number;
  distinctKeys: number;
  /** Live-edge revalidations issued (T-460). Counted separately from `requests` because they are
   * the one class of fetch this cache starts for a tile it already holds. */
  edgeRefreshes: number;
  /** Refreshes whose answer actually replaced a resident tile — the number that says rows reached
   * the texture, as against merely having been asked for. */
  edgeRefreshApplied: number;
  /** Places the route answered *no* to permanently, and that are never asked for again (T-479). */
  terminalFailures: number;
}

const MB = 1024 * 1024;

/** Consecutive completed requests before the cap recovers one slot. The additive half of AIMD. */
export const RECOVER_AFTER = 8;
/**
 * Floor on what an abandoned request is charged, ms: the route's **measured** cost for one 256²
 * tile (T-438). A request cannot have cost the server nothing, and charging zero is what let the
 * frame loop become a request pump.
 */
const MIN_RESIDUAL_MS = 12;
/** Bounds on the measured service-time estimate, ms: a fine tile's 11.4 ms and the map's 5.2 s. */
const MIN_SERVER_MS = 11, MAX_SERVER_MS = 6000;
/**
 * The live-edge refresh may issue at most one request per this many **measured** service times
 * ([[TileCache.serverEstimateMs]]), so revalidation can never take more than a quarter of what the
 * route has been observed to be able to produce (T-460).
 *
 * It is a share of measured capacity rather than a frequency on purpose: a 19 MB live tile that
 * takes 600 ms to answer makes the refresh 2.4 s apart without anything being retuned, which is the
 * property that stops this from becoming the poll the ticket forbids.
 */
export const REFRESH_DUTY = 4;
/** How often [[TileCache.refreshEdge]] may walk the resident set, ms. The walk is cheap; doing it
 * at frame rate would still be 60× more often than the finest tile can change. */
const EDGE_SCAN_MS = 250;

/** One request the client is waiting on, and what it is being counted against. */
interface InFlight {
  readonly ctrl: AbortController | null;
  readonly startedAt: number;
  /** Index into the viewports of the last [[TileCache.setViewports]], or -1 for none. */
  readonly owner: number;
}

export class TileCache<T> {
  readonly budgetBytes: number;
  private readonly maxQueue: number;
  private readonly busyBackoffMs: number;
  private readonly now: () => number;
  private map = new Map<string, TileEntry<T>>();
  private queue: TileAddr[] = [];
  private queued = new Set<string>();
  private inflight = new Map<string, InFlight>();
  /** When each abandoned request is expected to stop costing the route a slot. See [[abandon]]. */
  private abandonedUntil: number[] = [];
  /** The viewports issue is shared between, and the lattice they are expressed in. */
  private viewports: readonly Viewport[] = [];
  private lat: Lattice | null = null;
  private evicted = new Set<string>();
  private everRequested = new Set<string>();
  /** Keys whose in-flight fetch was overtaken by a retune: the data that arrives describes the old
   * tuning, so it is dropped on arrival and asked for again ([[invalidateEdge]]). */
  private stale = new Set<string>();
  /**
   * Places the route refused permanently, and why (T-479).
   *
   * The user watched the console flood because this did not exist: the canvas asked for an
   * out-of-range node (`level_f=10`, `t_index=218471` — T-480 owns the address arithmetic), the route
   * correctly answered **400**, and the client asked again on the very next frame. `acquire` had no
   * memory of the refusal, so `schedule` re-queued it forever at whatever rate the budget allowed.
   *
   * The fix is not another status beside 503. **Retryable is the enumerated case and terminal is the
   * default** ([[retryable]]), because the defect was that everything unenumerated fell through to
   * *ask again* — the same shape as `BiasTee::Unknown` not being `Off`, in the retry direction.
   */
  private terminal = new Map<string, string>();
  /** Keys being re-fetched **while their copy stays resident and drawn** (T-460). The set is what
   * lets [[insert]] tell a revalidation, which replaces, from a duplicate, which never uploads. */
  private refreshing = new Set<string>();
  /** The live-edge revalidation lane: its own queue, so it can never take a slot from [[queue]]. */
  private refreshQueue: TileAddr[] = [];
  private refreshQueued = new Set<string>();
  /** When each resident tile's data was last taken in, ms. The refresh interval is measured from
   * this, so a tile is never asked for again inside the period its newest cell spans. */
  private refreshedAt = new Map<string, number>();
  /** The next instant a refresh may be issued, and the next one the resident set may be walked. */
  private refreshNextIssue = 0;
  private nextEdgeScan = 0;
  private clock = 0;
  private frame = 0;
  private bytes = 0;
  private busyUntil = 0;
  private limit: number;
  /** The most this client may recover to: the server's own number, never raised by anything here. */
  private ceiling: number;
  /** Consecutive refusals, for the backoff; and completions since the last one, for the recovery. */
  private refusals = 0;
  private goodRuns = 0;
  /** Measured mean production time, ms. See [[observe]]. */
  private serverMs: number;
  readonly stats: TileCacheStats = {
    uploads: 0, hits: 0, misses: 0, evictions: 0, refetchAfterEvict: 0, requests: 0,
    failures: 0, busyRefusals: 0, cancelled: 0, abandoned: 0, overBudgetFrames: 0, distinctKeys: 0,
    edgeRefreshes: 0, edgeRefreshApplied: 0, terminalFailures: 0,
  };

  constructor(
    private readonly tex: TileTextures<T>,
    private readonly source: (addr: TileAddr, signal?: AbortSignal) => Promise<TileData>,
    opts: TileCacheOptions = {},
  ) {
    this.budgetBytes = opts.budgetBytes ?? 96 * MB;
    this.ceiling = this.limit = Math.max(1, opts.inFlight ?? 4);
    this.maxQueue = opts.maxQueue ?? 4096;
    this.busyBackoffMs = opts.busyBackoffMs ?? 200;
    this.serverMs = clamp(opts.serverMsGuess ?? 400, MIN_SERVER_MS, MAX_SERVER_MS);
    this.now = opts.now ?? (() => Date.now());
  }

  get residentTiles(): number { return this.map.size; }
  get residentBytes(): number { return this.bytes; }
  get inFlightLimit(): number { return this.limit; }
  get inFlightCount(): number { return this.inflight.size; }
  get queueDepth(): number { return this.queue.length; }
  /** Live-edge revalidations queued but not yet issued (T-460). Its own lane, never [[queue]]. */
  get refreshDepth(): number { return this.refreshQueue.length; }
  /** What the measurements say one tile costs the route, ms. */
  get serverEstimateMs(): number { return this.serverMs; }

  /**
   * Requests this client aborted that the route is presumed to still be producing.
   *
   * These occupy the budget exactly as outstanding ones do, because on the server they *are*
   * outstanding: `hk-api` notices a closed socket when it writes the response, not when the client
   * stops listening. Pruning is done here rather than on a timer so the count needs no clock of its
   * own and is exact at the only moment it is read.
   */
  get abandonedSlots(): number {
    if (this.abandonedUntil.length) {
      const t = this.now();
      this.abandonedUntil = this.abandonedUntil.filter((until) => until > t);
    }
    return this.abandonedUntil.length;
  }

  /** A new render frame: pins from the previous one lapse. */
  beginFrame(): void { this.frame++; }

  /**
   * Look a tile up for drawing, pinning it for this frame and scheduling it if it is missing.
   *
   * Never blocks and never returns a substitute: a caller that gets `pending` decides what to draw,
   * and the one thing it may not decide is grey.
   */
  acquire(addr: TileAddr, pin = true): Residency<T> {
    const key = keyOf(addr);
    this.stats.requests++;
    this.everRequested.add(key);
    this.stats.distinctKeys = this.everRequested.size;
    const e = this.map.get(key);
    if (e) {
      e.lastUsed = ++this.clock;
      if (pin) e.pinnedFrame = this.frame;
      this.stats.hits++;
      return { kind: "resident", entry: e };
    }
    this.stats.misses++;
    const why = this.terminal.get(key);
    if (why !== undefined) return { kind: "pending", failed: true };
    this.schedule(addr);
    return { kind: "pending" };
  }

  /** Why this place will never be drawn, or `null`. `"…"` is the route's own words (T-479). */
  refusalFor(addr: TileAddr): string | null { return this.terminal.get(keyOf(addr)) ?? null; }
  /** How many places the route has refused permanently. */
  get terminalPlaces(): number { return this.terminal.size; }

  /** Resident lookup with no scheduling and no pin: how a fallback search asks about ancestors
   * without queueing a fetch for every level it tries. */
  peek(addr: TileAddr, pin = false): TileEntry<T> | null {
    const e = this.map.get(keyOf(addr));
    if (!e) return null;
    e.lastUsed = ++this.clock;
    if (pin) e.pinnedFrame = this.frame;
    return e;
  }

  /** Want this tile soon, but do not draw it: the parent-level pin, and pan prefetch. */
  prefetch(addr: TileAddr): void {
    if (this.map.has(keyOf(addr))) { this.peek(addr, true); return; }
    this.schedule(addr);
  }

  /**
   * Enqueue, newest-wanted last. The queue is **LIFO**: a fast pan enqueues hundreds of tiles for
   * viewports the user has already left, and under FIFO the ones that finally arrive are for the
   * wrong place. That is what makes map clients feel laggy (§5.5 cap (1)).
   */
  private schedule(addr: TileAddr): void {
    const key = keyOf(addr);
    if (this.map.has(key) || this.inflight.has(key) || this.queued.has(key)) return;
    // The route has already said this place is not askable. A renderer calls `acquire` for it on
    // every frame, so without this the refusal is re-issued at frame rate (T-479).
    if (this.terminal.has(key)) return;
    this.queue.push(addr);
    this.queued.add(key);
    if (this.queue.length > this.maxQueue) {
      const dropped = this.queue.splice(0, this.queue.length - this.maxQueue);
      for (const d of dropped) this.queued.delete(keyOf(d));
      this.stats.cancelled += dropped.length;
    }
  }

  /**
   * End of a render frame: evict down to the budget, then pump the queue.
   *
   * Eviction runs *after* the frame's pins are known, so what is on screen is never the victim of
   * what is being fetched for it.
   */
  endFrame(): void {
    this.evict();
    this.pump();
  }

  /**
   * The viewports that still matter. Queued tiles outside every one are dropped and in-flight ones
   * are aborted — §5.5's "viewport-change cancellation", which is what makes the in-flight cap a
   * latency control rather than a queue the user waits out.
   *
   * **A viewport is a box AND its levels, and both halves are load-bearing** (T-443). The predicate
   * was extent-only, which was sound while every viewport was a pane at a comparable zoom; the
   * minimap broke it the moment it arrived, because it is *another viewport* spanning nearly the
   * whole surface, so every fine-level tile for a viewport the user had left still intersected it
   * and **cancellation silently stopped cancelling anything**. Matching the level too restores it:
   * a tile is wanted when some viewport is drawing at its level — or one step coarser, which is the
   * parent pin the renderer prefetches and must not immediately cancel.
   */
  setViewports(lat: Lattice, viewports: readonly Viewport[]): void {
    this.lat = lat;
    this.viewports = viewports;
    const wanted = (a: TileAddr) => viewports.some((v) => this.wants(lat, v, a));
    const keep: TileAddr[] = [];
    for (const a of this.queue) {
      if (wanted(a)) keep.push(a);
      else { this.queued.delete(keyOf(a)); this.stats.cancelled++; }
    }
    this.queue = keep;
    for (const [key, f] of this.inflight) {
      const a = parseKey(key);
      if (a && !wanted(a) && f.ctrl) { f.ctrl.abort(); this.stats.cancelled++; this.abandon(f); }
    }
  }

  /** Is this tile one that viewport is drawing — at its level, or one step coarser (the pin)? */
  private wants(lat: Lattice, v: Viewport, a: TileAddr): boolean {
    return a.levelF >= v.levelF && a.levelF <= v.levelF + 1 &&
      a.levelT >= v.levelT && a.levelT <= v.levelT + 1 &&
      intersects(lat, a, v.box);
  }

  /**
   * Charge an aborted request to the budget until the route is expected to be done with it.
   *
   * The abort reaches the browser, not `hk-api`: its handler discovers the closed socket when it
   * writes, so the [`TileSlot`] and the history lock stay held for the rest of the read. Releasing
   * this cache's slot at the moment of the abort is therefore a *claim about the server that is
   * false*, and it is the claim that turned a 60 Hz drag into a 60 Hz request pump. The charge is
   * the measured mean minus however long this one has already run, never below
   * [[MIN_RESIDUAL_MS]] — zero is the one answer that cannot be right.
   */
  private abandon(f: InFlight): void {
    const left = Math.max(MIN_RESIDUAL_MS, this.serverMs - (this.now() - f.startedAt));
    this.abandonedUntil.push(this.now() + left);
    this.stats.abandoned++;
  }

  /**
   * Drop every resident tile that **reaches the growing edge**, and discard any already in flight
   * for it. Returns how many resident tiles were dropped.
   *
   * Called after a retune (T-444; T-437 §5.2's one client-side ask, which the spike measured at 32
   * tiles). The edge tiles are computed *on request* from whatever the front end was tuned to at
   * that moment, so after the tuning changes a cached one is an observation claim about a tuning
   * that no longer exists. That makes this a **grey-honesty** measure, not a freshness nicety: the
   * surface's load-bearing claim is that grey means the radio never looked there, and its mirror
   * claim is that a coloured cell means it did — of *this* band, at *this* time.
   *
   * Two details that are not obvious:
   *
   *  - **Coarser levels go too, not only the finest.** A level-3 tile covering the edge is rewritten
   *    by the same new frames; keeping it because it is not "the finest" would leave the fallback
   *    path (§5.5's upscaled ancestor) drawing the old tuning underneath the new one.
   *  - **In-flight requests are marked stale, not merely aborted.** A fetch issued before the retune
   *    lands after it, and `insert` would happily accept it into an empty slot. So the key is marked
   *    and the arriving data is dropped and re-requested instead.
   *
   * Tiles addressed on another lattice are left alone: their scheme is part of the key, so they
   * cannot be confused with this one's, and this call knows nothing about their geometry.
   */
  invalidateEdge(lat: Lattice, edgeNs: number, box?: Box): number {
    if (!Number.isFinite(edgeNs)) return 0;
    let n = 0;
    for (const e of [...this.map.values()]) {
      if (this.atEdge(lat, e.addr, edgeNs, box) && this.invalidate(e.addr)) n++;
    }
    for (const key of this.inflight.keys()) {
      const a = parseKey(key);
      if (a && this.atEdge(lat, a, edgeNs, box)) this.stale.add(key);
    }
    return n;
  }

  /**
   * **Re-ask for the tiles that hold the growing edge, so live advances** (T-460).
   *
   * The counterpart to [[invalidateEdge]], and deliberately *not* it: invalidating drops the tile,
   * which would blank the newest seconds of a following pane every time the edge moved. This queues
   * a re-fetch of the same address while the copy in hand stays resident and drawn, and [[insert]]
   * swaps the texture when the answer lands. Nothing on screen goes backwards, and no second path
   * produces rows — it is the same address through the same `source`.
   *
   * `following` is the viewports that are **pinned to the growing edge**, with the levels they were
   * actually drawn at this frame. Everything eligible has to be on that list, because:
   *
   *  - a **frozen** pane is a view over data that cannot change, so refreshing for it is pure cost;
   *  - a tile one level coarser is the parent *pin* — prefetched, not drawn — and refreshing what is
   *    not on screen is the same cost with less excuse.
   *
   * The two rate limits are in [[pumpRefresh]] and in the `due` test below, and neither is a tuned
   * number: one is the period the tile's own newest cell spans, the other a fixed share of the
   * *measured* service time. Returns how many were queued, for the tests and the stats.
   */
  refreshEdge(lat: Lattice, edgeNs: number, following: readonly Viewport[]): number {
    if (!Number.isFinite(edgeNs) || !following.length) return 0;
    const t = this.now();
    if (t < this.nextEdgeScan) return 0;
    this.nextEdgeScan = t + EDGE_SCAN_MS;
    let n = 0;
    for (const e of this.map.values()) {
      const key = e.key;
      if (this.refreshing.has(key) || this.refreshQueued.has(key) || this.inflight.has(key)) continue;
      if (!this.atEdge(lat, e.addr, edgeNs)) continue;
      if (!following.some((v) => this.drawnBy(lat, v, e.addr))) continue;
      // The newest cell of a tile is one cell tall, so a re-ask inside that period cannot come back
      // with a row the copy in hand does not already have. This is the cadence, and it scales itself
      // with the zoom: 1 s at level 0, 32 s five levels out.
      if (t < (this.refreshedAt.get(key) ?? 0) + tCellNs(lat, e.addr.levelT) / 1e6) continue;
      this.refreshQueue.push(e.addr);
      this.refreshQueued.add(key);
      n++;
    }
    if (n) this.pump();
    return n;
  }

  /** Is this tile one that viewport is **drawing**, at exactly its level? Unlike [[wants]] this
   * excludes the parent pin: a refresh is for what is on screen. */
  private drawnBy(lat: Lattice, v: Viewport, a: TileAddr): boolean {
    return a.scheme === lat.scheme && a.levelF === v.levelF && a.levelT === v.levelT &&
      intersects(lat, a, v.box);
  }

  /** Does this tile hold the growing edge — i.e. is its newest cell still being written? */
  private atEdge(lat: Lattice, a: TileAddr, edgeNs: number, box?: Box): boolean {
    if (a.scheme !== lat.scheme) return false;
    const ext = extentOf(lat, a);
    // One cell of slack: the cell the edge is *in* is partly written, and so is the one before it
    // when a frame straddles the boundary.
    if (ext.t1Ns < edgeNs - tCellNs(lat, a.levelT)) return false;
    return !box || (ext.f1Hz > box.f0Hz && ext.f0Hz < box.f1Hz);
  }

  /** Drop one tile so the growing edge can rewrite it (T-439's live tiles are not immutable). */
  invalidate(addr: TileAddr): boolean {
    const key = keyOf(addr);
    const e = this.map.get(key);
    if (!e) return false;
    this.tex.destroy(e.tex);
    this.map.delete(key);
    this.refreshedAt.delete(key);
    this.bytes -= e.data.bytes;
    return true;
  }

  dispose(): void {
    for (const e of this.map.values()) this.tex.destroy(e.tex);
    this.map.clear();
    this.bytes = 0;
    for (const [, f] of this.inflight) f.ctrl?.abort();
    this.inflight.clear();
    this.abandonedUntil = [];
    this.queue = [];
    this.queued.clear();
    this.stale.clear();
    this.refreshing.clear();
    this.refreshQueue = [];
    this.refreshQueued.clear();
    this.refreshedAt.clear();
    this.terminal.clear();
  }

  private pump(): void {
    if (this.now() < this.busyUntil) return;
    // The budget is what the ROUTE has out on this client's behalf — the requests being waited on
    // *and* the ones walked away from, which it is still producing. See [[abandonedSlots]].
    while (this.inflight.size + this.abandonedSlots < this.limit && this.queue.length) {
      const next = this.nextAddr();
      if (!next) break; // every viewport is at its share; the rest of the queue waits
      const { addr, owner } = next;
      const key = keyOf(addr);
      this.queued.delete(key);
      if (this.map.has(key) || this.inflight.has(key)) continue;
      this.issue(addr, owner);
    }
    this.pumpRefresh();
  }

  /**
   * Issue at most one live-edge revalidation (T-460), and only when it can cost nothing anyone is
   * waiting for.
   *
   * **Last refusal, not first.** This runs at the end of [[pump]], so the ordinary queue has already
   * been offered every slot in the budget; a refresh can only use one the visible work could not.
   * The route's cap is shared and an abandoned read holds a slot until it finishes ([[abandon]]), so
   * `abandonedSlots` is charged here exactly as it is there — the free slot has to be free *at the
   * route*, not merely in this map.
   *
   * It is deliberately **not** "only when nothing at all is outstanding". That was the first rule,
   * and a browser run measured it wrong: T-479's permanently-refused minimap places kept the queue
   * non-empty for the whole session, and the live edge never refreshed once. A precondition that
   * something unrelated can hold false forever is not a safety property, it is a deadlock.
   *
   * **And the duty limit is what stops it being a poll.** Once per [[REFRESH_DUTY]] × the measured
   * service time, so the lane can never occupy more than a quarter of observed capacity — and gets
   * rarer on its own if tiles get more expensive, which is exactly the 19 MB case the ticket forbids
   * polling into.
   */
  private pumpRefresh(): void {
    if (!this.refreshQueue.length) return;
    // **At most one revalidation in flight, ever.** It is what makes the duty limit below a bound on
    // the lane rather than on each of its members, and it is why the cadence can be set from the one
    // completion rather than from a running estimate.
    if (this.refreshing.size) return;
    const t = this.now();
    if (t < this.busyUntil || t < this.refreshNextIssue) return;
    if (this.inflight.size + this.abandonedSlots >= this.limit) return;
    const addr = this.refreshQueue.shift()!;
    const key = keyOf(addr);
    this.refreshQueued.delete(key);
    // Evicted, retuned away, or otherwise no longer in hand while it waited: there is nothing to
    // revalidate, and the ordinary miss path owns the address now.
    if (!this.map.has(key)) return;
    this.refreshing.add(key);
    this.refreshedAt.set(key, t);
    this.stats.edgeRefreshes++;
    this.issue(addr, -1);
  }

  /** Start one request for `addr`, on the budget. The single place a fetch begins — an ordinary
   * miss and a live-edge revalidation differ only in what [[insert]] does with the answer. */
  private issue(addr: TileAddr, owner: number): void {
    const key = keyOf(addr);
    const ctrl = typeof AbortController === "function" ? new AbortController() : null;
    const started = this.now();
    this.inflight.set(key, { ctrl, startedAt: started, owner });
    if (this.evicted.has(key)) this.stats.refetchAfterEvict++;
    // The request is off the in-flight list BEFORE anything is re-queued, or `schedule` would see
    // its own request still outstanding and silently drop the retry.
    const done = (requeue: boolean) => {
      this.inflight.delete(key);
      if (this.refreshing.delete(key)) {
        // **The lane's cadence is a share of the LANE'S OWN cost, measured on this very request.**
        //
        // It was `REFRESH_DUTY x serverMs`, and a browser run showed why that is the wrong number:
        // `serverMs` folds in every completion, including the minimap's coarse read, which T-450
        // measured at **5.2 s** and which `MAX_SERVER_MS` clamps at 6 s — so one unrelated map tile
        // set the live edge's refresh interval to 24 s and the newest quarter-minute of a 17 s
        // window went unasked-for. A fine live-edge tile is not that tile and must not be charged
        // for it. Waiting (REFRESH_DUTY - 1) further service times after it lands makes the lane's
        // duty cycle exactly 1/REFRESH_DUTY of what it was just observed to cost, whatever that is.
        const spent = Math.max(MIN_RESIDUAL_MS, this.now() - started);
        this.refreshNextIssue = this.now() + (REFRESH_DUTY - 1) * spent;
      }
      if (requeue) this.schedule(addr);
      this.pump();
    };
    void this.source(addr, ctrl?.signal).then(
      (data) => {
        this.observe(started);
        this.succeeded();
        // A tile the retune overtook is requeued, not kept: `insert` says which happened, and the
        // requeue has to run in `done`, after the key leaves the in-flight map, or `schedule`
        // would see this very request still outstanding and silently drop the retry.
        let requeue = false;
        try { requeue = !this.insert(addr, data); } catch { this.stats.failures++; }
        done(requeue);
      },
      (err) => done(this.failed(addr, err, started, ctrl?.signal.aborted ?? false)),
    );
  }

  /**
   * The next tile to ask for: **LIFO, but shared between viewports**.
   *
   * LIFO alone answers "where is the user now"; it does not answer "which viewport". T-450 measured
   * why that matters — the map's level-10 tile takes **5.2 s and 9.5 MB**, so one coarse read holds
   * a quarter of the budget for five seconds while the panes show pending. Each viewport therefore
   * gets `limit / viewports` slots (at least one), and the scan skips a tile whose viewports are
   * all at their share rather than dropping it: it is still wanted, just not next.
   *
   * With no viewports yet — before the first frame reports any — this is exactly the old `pop()`.
   */
  private nextAddr(): { addr: TileAddr; owner: number } | null {
    const lat = this.lat, vs = this.viewports;
    if (!lat || !vs.length) {
      const addr = this.queue.pop();
      return addr ? { addr, owner: -1 } : null;
    }
    const share = Math.max(1, Math.floor(this.limit / vs.length));
    const held = new Array<number>(vs.length).fill(0);
    for (const f of this.inflight.values()) if (f.owner >= 0 && f.owner < held.length) held[f.owner]++;
    for (let i = this.queue.length - 1; i >= 0; i--) {
      const addr = this.queue[i];
      let orphan = true;
      for (let v = 0; v < vs.length; v++) {
        if (!this.wants(lat, vs[v], addr)) continue;
        orphan = false;
        if (held[v] < share) { this.queue.splice(i, 1); return { addr, owner: v }; }
      }
      // Wanted by no viewport at all: `setViewports` has not seen it yet, so it belongs to no
      // share and is issued on the global budget alone.
      if (orphan) { this.queue.splice(i, 1); return { addr, owner: -1 }; }
    }
    return null;
  }

  /** Fold one completed request into the measured service time. Refusals are excluded: a `503`
   * returns at once and would teach the estimate that tiles are free. */
  private observe(startedAt: number): void {
    const d = this.now() - startedAt;
    if (!(d >= 0)) return;
    this.serverMs = clamp(this.serverMs + 0.25 * (d - this.serverMs), MIN_SERVER_MS, MAX_SERVER_MS);
  }

  /** A completion: the refusal streak is over, and every [[RECOVER_AFTER]] of them buys a slot
   * back, up to the ceiling the server named. The additive-increase half of AIMD. */
  private succeeded(): void {
    this.refusals = 0;
    if (this.limit >= this.ceiling) { this.goodRuns = 0; return; }
    if (++this.goodRuns >= RECOVER_AFTER) { this.limit++; this.goodRuns = 0; }
  }

  /** Whether the tile is still wanted after `err`. */
  private failed(addr: TileAddr, err: unknown, startedAt: number, aborted: boolean): boolean {
    // An abort is this cache's own doing — the viewport moved — so it is neither a failure nor a
    // reason to ask again. It is not free either: the route is still producing the tile, and
    // `setViewports` has already charged it.
    if (aborted) return false;
    if (err instanceof TileBusyError) {
      // Not an error: the route is telling the client it is asking for too many at once.
      //
      // **The number it names is the whole server's budget, not this client's allowance** — every
      // pane, every other tab and the bootstrap probe draw on the same four slots, and an abandoned
      // read holds one until it finishes. So the number is kept as a *ceiling* and the operating
      // cap is halved: the share that belongs to this cache is not something either side knows, it
      // is something backing off finds. Recovery is [[succeeded]]; together they are AIMD.
      this.stats.busyRefusals++;
      if (err.limit && err.limit > 0) this.ceiling = Math.min(this.ceiling, err.limit);
      this.limit = Math.max(1, Math.min(Math.floor(this.limit / 2), this.ceiling));
      this.goodRuns = 0;
      this.refusals = Math.min(this.refusals + 1, 4);
      // The refusal itself cost the server nothing, but whatever is holding the slots has not
      // finished — so wait longer each time rather than re-asking on the same cadence.
      this.busyUntil = this.now() + this.busyBackoffMs * 2 ** (this.refusals - 1);
      return true;
    }
    this.observe(startedAt);
    this.stats.failures++;
    // **Terminal unless asking again could change the answer** (T-479). Nothing is re-queued here
    // either way — the difference is whether the *next frame* may ask. A renderer calls `acquire` for
    // every place it draws on every frame, so a place that is not remembered as refused is asked for
    // again at frame rate, which is exactly what flooded the user's console with 400s.
    //
    // It is not grey: `acquire` still answers `pending`, and only a `coverage` state byte can produce
    // grey (ui/src/surface/cellrule.ts), so a refused place stays structurally distinguishable from
    // one the radio never looked at.
    if (!retryable(err)) {
      this.terminal.set(keyOf(addr), describeFailure(err));
      this.stats.terminalFailures++;
    }
    return false;
  }

  /** Take a fetched tile. **False means "ask again"**: the tuning changed while it was in flight. */
  private insert(addr: TileAddr, data: TileData): boolean {
    const key = keyOf(addr);
    if (this.stale.delete(key)) return false;
    const prev = this.map.get(key);
    // A **live-edge revalidation replaces** the copy in hand (T-460); anything else that arrives for
    // a resident key is a duplicate, and the same tile is never uploaded twice.
    if (prev && !this.refreshing.has(key)) return true;
    // `cost.in_flight_limit` is the same server-wide number the refusal names, so it sets the
    // ceiling — it is not permission to run at it.
    if (data.serverInFlightLimit && data.serverInFlightLimit > 0) {
      this.ceiling = Math.min(this.ceiling, data.serverInFlightLimit);
      this.limit = Math.min(this.limit, this.ceiling);
    }
    const tex = this.tex.upload(data);
    this.stats.uploads++;
    // The replaced texture is destroyed and its bytes returned: a refresh that leaked one would turn
    // the growing edge into a memory leak proportional to how long the view is left running.
    if (prev) {
      this.tex.destroy(prev.tex);
      this.bytes -= prev.data.bytes;
      this.stats.edgeRefreshApplied++;
    }
    // A tile that has just arrived is pinned for the frame it arrived on: it cost the server 11.4 ms
    // and evicting it before it has been drawn once would spend that twice.
    this.map.set(key, { key, addr, data, tex, lastUsed: ++this.clock, pinnedFrame: this.frame });
    this.bytes += data.bytes;
    this.refreshedAt.set(key, this.now());
    this.evict();
    return true;
  }

  /**
   * LRU over resident bytes, **never evicting a pinned tile**, with §5.5's level-distance tiebreak:
   * among equally-stale tiles the one furthest from what is being looked at goes first, since a
   * user three levels in is not returning to level 0 within a frame budget.
   */
  private evict(): void {
    while (this.bytes > this.budgetBytes) {
      let victim: TileEntry<T> | null = null;
      for (const e of this.map.values()) {
        if (e.pinnedFrame === this.frame) continue;
        if (!victim || e.lastUsed < victim.lastUsed || (e.lastUsed === victim.lastUsed && levelDistance(e) > levelDistance(victim))) victim = e;
      }
      if (!victim) { this.stats.overBudgetFrames++; return; }
      this.tex.destroy(victim.tex);
      this.map.delete(victim.key);
      this.refreshedAt.delete(victim.key);
      this.bytes -= victim.data.bytes;
      this.evicted.add(victim.key);
      this.stats.evictions++;
    }
  }
}

/**
 * **Is asking again capable of changing the answer?** (T-479.)
 *
 * Two cases, and they are the whole enumeration — everything else is terminal by default, which is
 * the inversion this function exists for. The old rule special-cased `503` and let *every other*
 * outcome fall through to "ask again", so each unenumerated status was wrong by default; this is the
 * same principle as `BiasTee::Unknown` not being `Off`, applied in the retry direction.
 *
 *  1. **`TileBusyError`** — the route's `503`. T-454's backpressure is a statement about *now*: the
 *     history lock is held, and the only correct response is to wait and ask again. It is handled by
 *     the AIMD branch above with its own backoff and never reaches here.
 *  2. **A failure that carries no HTTP status** — the request never got an answer at all (socket
 *     closed, server restarting, DNS). The server said nothing, so nothing it said is permanent, and
 *     blanking the surface for the rest of the session over a momentary disconnect would be the
 *     opposite defect. This does not re-queue either: the next frame's `acquire` asks again, so the
 *     retry is paced by the render loop rather than by a tight failure cycle.
 *
 * Anything the server *said* — 400, 404, 413, 500 — is terminal. The status is read structurally
 * rather than by instanceof so this does not couple to which error class the fetch layer happens to
 * build (`tile.ts` is not this file's to know the internals of).
 */
function retryable(err: unknown): boolean {
  if (err instanceof TileBusyError) return true;
  const status = (err as { status?: unknown } | null)?.status;
  return typeof status !== "number";
}

/** The route's own words for a refusal, for the readout and for a test's failure message. */
function describeFailure(err: unknown): string {
  const status = (err as { status?: unknown } | null)?.status;
  const msg = err instanceof Error ? err.message : String(err);
  return typeof status === "number" ? `HTTP ${status}: ${msg}` : msg;
}

const levelDistance = <T>(e: TileEntry<T>) => e.addr.levelF + e.addr.levelT;

const clamp = (v: number, lo: number, hi: number) => (Number.isFinite(v) ? Math.min(hi, Math.max(lo, v)) : lo);

/** The inverse of [[keyOf]], for cancelling in-flight requests by viewport. */
export function parseKey(key: string): TileAddr | null {
  const p = key.split("|");
  if (p.length !== 7) return null;
  const n = p.slice(2).map(Number);
  if (n.some((v) => !Number.isFinite(v))) return null;
  return { device: p[0], scheme: p[1], levelF: n[0], levelT: n[1], fIndex: n[2], tIndex: n[3], cells: n[4] };
}
