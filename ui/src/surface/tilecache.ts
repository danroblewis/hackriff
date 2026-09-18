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

import { intersects, keyOf, type Box, type Lattice, type TileAddr } from "./lattice";
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

/** What the cache can answer. **`pending` is not a cell state** — see ui/src/surface/cellrule.ts. */
export type Residency<T> = { readonly kind: "resident"; readonly entry: TileEntry<T> } | { readonly kind: "pending" };

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
  /** Outstanding requests. Defaults to the route's own `TILE_MAX_IN_FLIGHT`; a `503` naming a
   * smaller cap lowers it, because the cap is ingest backpressure and the server owns it. */
  inFlight?: number;
  /** Queue depth. A fast pan can enqueue thousands; the oldest (bottom of the LIFO) are the ones
   * the user has already left, so they are the ones dropped. */
  maxQueue?: number;
  now?: () => number;
  /** Delay after a `503` before the queue is pumped again, ms. */
  busyBackoffMs?: number;
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
  /** Frames on which the pins alone exceeded the budget: the pane is asking for more than the
   * budget can hold, and the honest response is to keep drawing, not to grey anything. */
  overBudgetFrames: number;
  distinctKeys: number;
}

const MB = 1024 * 1024;

export class TileCache<T> {
  readonly budgetBytes: number;
  private readonly maxQueue: number;
  private readonly busyBackoffMs: number;
  private readonly now: () => number;
  private map = new Map<string, TileEntry<T>>();
  private queue: TileAddr[] = [];
  private queued = new Set<string>();
  private inflight = new Map<string, AbortController | null>();
  private evicted = new Set<string>();
  private everRequested = new Set<string>();
  private clock = 0;
  private frame = 0;
  private bytes = 0;
  private busyUntil = 0;
  private limit: number;
  readonly stats: TileCacheStats = {
    uploads: 0, hits: 0, misses: 0, evictions: 0, refetchAfterEvict: 0, requests: 0,
    failures: 0, busyRefusals: 0, cancelled: 0, overBudgetFrames: 0, distinctKeys: 0,
  };

  constructor(
    private readonly tex: TileTextures<T>,
    private readonly source: (addr: TileAddr, signal?: AbortSignal) => Promise<TileData>,
    opts: TileCacheOptions = {},
  ) {
    this.budgetBytes = opts.budgetBytes ?? 96 * MB;
    this.limit = opts.inFlight ?? 4;
    this.maxQueue = opts.maxQueue ?? 4096;
    this.busyBackoffMs = opts.busyBackoffMs ?? 200;
    this.now = opts.now ?? (() => Date.now());
  }

  get residentTiles(): number { return this.map.size; }
  get residentBytes(): number { return this.bytes; }
  get inFlightLimit(): number { return this.limit; }
  get inFlightCount(): number { return this.inflight.size; }
  get queueDepth(): number { return this.queue.length; }

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
    this.schedule(addr);
    return { kind: "pending" };
  }

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
    const wanted = (a: TileAddr) => viewports.some((v) =>
      a.levelF >= v.levelF && a.levelF <= v.levelF + 1 &&
      a.levelT >= v.levelT && a.levelT <= v.levelT + 1 &&
      intersects(lat, a, v.box));
    const keep: TileAddr[] = [];
    for (const a of this.queue) {
      if (wanted(a)) keep.push(a);
      else { this.queued.delete(keyOf(a)); this.stats.cancelled++; }
    }
    this.queue = keep;
    for (const [key, ctrl] of this.inflight) {
      const a = parseKey(key);
      if (a && !wanted(a) && ctrl) { ctrl.abort(); this.stats.cancelled++; }
    }
  }

  /** Drop one tile so the growing edge can rewrite it (T-439's live tiles are not immutable). */
  invalidate(addr: TileAddr): boolean {
    const key = keyOf(addr);
    const e = this.map.get(key);
    if (!e) return false;
    this.tex.destroy(e.tex);
    this.map.delete(key);
    this.bytes -= e.data.bytes;
    return true;
  }

  dispose(): void {
    for (const e of this.map.values()) this.tex.destroy(e.tex);
    this.map.clear();
    this.bytes = 0;
    for (const [, c] of this.inflight) c?.abort();
    this.inflight.clear();
    this.queue = [];
    this.queued.clear();
  }

  private pump(): void {
    if (this.now() < this.busyUntil) return;
    while (this.inflight.size < this.limit && this.queue.length) {
      const addr = this.queue.pop()!; // LIFO: the most recently wanted tile first
      const key = keyOf(addr);
      this.queued.delete(key);
      if (this.map.has(key) || this.inflight.has(key)) continue;
      const ctrl = typeof AbortController === "function" ? new AbortController() : null;
      this.inflight.set(key, ctrl);
      if (this.evicted.has(key)) this.stats.refetchAfterEvict++;
      // The request is off the in-flight list BEFORE anything is re-queued, or `schedule` would see
      // its own request still outstanding and silently drop the retry.
      const done = (requeue: boolean) => {
        this.inflight.delete(key);
        if (requeue) this.schedule(addr);
        this.pump();
      };
      void this.source(addr, ctrl?.signal).then(
        (data) => {
          try { this.insert(addr, data); } catch { this.stats.failures++; }
          done(false);
        },
        (err) => done(this.failed(addr, err, ctrl?.signal.aborted ?? false)),
      );
    }
  }

  /** Whether the tile is still wanted after `err`. */
  private failed(addr: TileAddr, err: unknown, aborted: boolean): boolean {
    // An abort is this cache's own doing — the viewport moved — so it is neither a failure nor a
    // reason to ask again.
    if (aborted) return false;
    if (err instanceof TileBusyError) {
      // Not an error: the route is telling the client it is asking for too many at once, and
      // naming the number. Adopt it, back off, and keep wanting the tile.
      this.stats.busyRefusals++;
      if (err.limit && err.limit > 0) this.limit = Math.min(this.limit, err.limit);
      this.busyUntil = this.now() + this.busyBackoffMs;
      return true;
    }
    this.stats.failures++;
    return false;
  }

  private insert(addr: TileAddr, data: TileData): void {
    const key = keyOf(addr);
    if (this.map.has(key)) return; // never upload the same tile twice
    if (data.serverInFlightLimit && data.serverInFlightLimit > 0) this.limit = Math.min(this.limit, data.serverInFlightLimit);
    const tex = this.tex.upload(data);
    this.stats.uploads++;
    // A tile that has just arrived is pinned for the frame it arrived on: it cost the server 11.4 ms
    // and evicting it before it has been drawn once would spend that twice.
    this.map.set(key, { key, addr, data, tex, lastUsed: ++this.clock, pinnedFrame: this.frame });
    this.bytes += data.bytes;
    this.evict();
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
      this.bytes -= victim.data.bytes;
      this.evicted.add(victim.key);
      this.stats.evictions++;
    }
  }
}

const levelDistance = <T>(e: TileEntry<T>) => e.addr.levelF + e.addr.levelT;

/** The inverse of [[keyOf]], for cancelling in-flight requests by viewport. */
export function parseKey(key: string): TileAddr | null {
  const p = key.split("|");
  if (p.length !== 7) return null;
  const n = p.slice(2).map(Number);
  if (n.some((v) => !Number.isFinite(v))) return null;
  return { device: p[0], scheme: p[1], levelF: n[0], levelT: n[1], fIndex: n[2], tIndex: n[3], cells: n[4] };
}
