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
//     refusal is now AIMD: **halve on refusal, recover one slot per [[RECOVER_AFTER]] successes**
//     — or, once T-539 found that a frozen view has no successes to offer, per [[RECOVER_QUIET]]
//     service times of quiet — with the server's number kept as the *ceiling* it actually is.
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
//     most once per [[REFRESH_DUTY]] × **what that lane costs, measured on its own requests**
//     ([[TileCache.refreshCostOf]]) — so the lane can never take more than 1/[[REFRESH_DUTY]] of
//     what a live-edge tile has been observed to cost. It is the **minimum of the lane's recent
//     completions** rather than the last one, because a completion's wall time is production *plus*
//     whatever a neighbour's history-lock hold added, and one contended sample charges the live edge
//     for the minimap's tile (T-491, measured: the same address 844 ms during the minimap's initial
//     fill and 80 ms after it). If tiles get slower the refresh gets rarer on its own; if
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
//
// ## T-495: eligibility, not re-entry — the same assumption one axis over
//
// The user's report was a **band of grey in the middle of a tile that never fills**: retune, pan onto
// the newly-live tile, pan away, pan back, and the rows recorded while it was off screen are grey for
// the rest of the session. It reads like a cache that hands back a stale copy on re-entry, and it is
// not. Driven through this cache the client makes **no request at all** on re-entry — and would make
// two if the live edge happened to still be inside the tile. The measurement and the rule are on
// [[TileCache.behindTheEdge]]; the one-sentence version is that T-460 made a live tile eligible for
// revalidation *while the edge is inside it*, and that eligibility expires permanently the moment the
// edge crosses the tile's end. A tile that spent the last of its own life off screen therefore keeps
// whatever the server had when it was last looked at, and the rest of its span stays `unobserved` —
// grey, and honest, and served before the rows existed.
//
// So freshness is not residency and it is not "at the edge" either: **a copy is fresh when the edge
// it was asked at had already reached the end of what it could hold**, which is `extent.t1_s` from
// the route's own answer. That test seals a tile exactly once and forever, so the fix costs one extra
// request per tile per lifetime and cannot become a poll.

//
// ## T-538: speculation is about where the view is GOING, not about where it is
//
// T-471 landed a standing *ring* — the one-tile border around every viewport, refreshed on a 250 ms
// scan — and was reverted off `main` for two measured reasons, both of which this lane is shaped to
// make impossible rather than to guard against:
//
//  1. **A speculative fetch that gets aborted charges the server a slot it will never use.** An
//     `abort()` reaches the browser, not `hk-api` (see [[abandon]]), so the route keeps producing a
//     tile nobody will read. The ring aborted its own requests every time the viewport moved: 46-53
//     wire aborts against 6-8 for the client without it, and other tenants of the four-slot budget
//     were refused. **So this lane never aborts.** Not in [[setViewports]], not anywhere but
//     [[dispose]]. It is issued only when this client holds *nothing at all* ([[idle]]) and the AIMD
//     cap is at its ceiling, so there is at most **one** speculative read outstanding, it is never
//     competing with a visible miss, and it is allowed to finish — which is the only way a slot
//     actually comes back. "Cancellable without charging the server" is answered by not needing to
//     cancel.
//  2. **"Backing off means asking nothing" was gated on the BUDGET, not the CAP.** `inflight <
//     effectiveLimit` is still true at a halved cap, so after a `503` the ring took the one
//     remaining slot every 250 ms for ever. Here the gate is `limit >= ceiling`: while AIMD is
//     backed off at all, speculation is silent. T-539's elapsed-quiet recovery is what makes that
//     free of the self-lock the first fix attempt had — an idle client's cap climbs back on the
//     clock, with nothing issued to earn it.
//
// And the third thing, which is why the ring was *visible* in `surface-nav`'s steady-state window at
// all: **a standing ring speculates about a view that is not moving.** A frozen pane over recorded
// data is going nowhere, so there is nothing to predict, and a lane that keeps asking anyway is a
// poll with a story. [[prefetchAhead]] is therefore driven by **displacement between frames**: the
// tiles one tile-width along the direction the box actually moved, and nothing at all when it did
// not move — where "moved" means *by at least one lattice cell*, since the level is chosen so a cell
// is about a pixel and a sub-cell wobble has not moved anything on screen. A still view issues zero
// requests **because it has no direction**, not because a threshold was tuned to make it quiet.
//
// Two more rules complete it. **Each address is speculated at most once, ever** ([[speculated]]) —
// so this lane can never produce a re-ask, which is the exact shape `surface-nav` partitions the
// steady window by (T-564), and it cannot become a poll however long a pan lasts. And **it queues
// nothing**: candidates live for the one call that computed them, so there is no backlog that can
// drain into a window after the motion that wanted it has stopped, and the client's queue depth is
// unaffected by construction.
//
// The trade this replaces: T-471 measured ~5 speculative requests to save one blocking miss on a
// one-tile pan, paid whether or not anyone was panning. This pays **at most one request per frame
// of actual motion, only while otherwise idle**, for the tile the pan is heading into.

import {
  extentOf, fCellHz, fTileHz, inLattice, intersects, keyOf, tCellNs, tTileNs, tilesFor,
  type Box, type Lattice, type TileAddr,
} from "./lattice";
import { BYTES_PER_CELL, TileBusyError, TileDecodeError, weakerTier, type TileData } from "./tile";
import { CELL } from "./cellrule";
import { RowAccumulator, type ColumnAddr, type GapBlock, type RowBlock, type RowResolution, type TileRows, type WantedColumn } from "./rowfeed";

/** The GPU side, kept behind an interface so the cache is testable without a GL context. */
export interface TileTextures<T> {
  upload(data: TileData): T;
  destroy(tex: T): void;
  /**
   * Rewrite rows `[row0, row0 + rows)` of `tex` from `data` in place (T-893): how a pushed row
   * reaches the screen without re-uploading the whole tile. Optional — without it the cache
   * uploads a fresh texture and destroys the old one.
   */
  patch?(tex: T, data: TileData, row0: number, rows: number): void;
}

export interface TileEntry<T> {
  readonly key: string;
  readonly addr: TileAddr;
  readonly data: TileData;
  readonly tex: T;
  lastUsed: number;
  /** Frame number this tile was last pinned on. Pinned tiles are never evicted (§5.5's two pins). */
  pinnedFrame: number;
  /**
   * **The live edge, in capture ns, at the instant this copy was ASKED FOR** (T-495).
   *
   * How far into its own span the answer can possibly hold rows for — and therefore the whole of
   * what "fresh" means for a tile. See [[TileCache.behindTheEdge]].
   *
   * At issue rather than at arrival, and that direction is the bug: the answer was built at some
   * instant between the two, so the edge at arrival **over-states** what the copy contains, and an
   * over-statement is exactly a tile marked finished while a gap is still in it. Under-stating costs
   * at most one extra request.
   */
  edgeAtFetchNs: number;
  /**
   * **Built from pushed rows alone, not from an answer of `GET /api/tiles`** (T-893). The rows the
   * live edge has entered arrive on `/ws/tiles/rows` before any tile read could, so the cache files
   * them as a tile at once; the first real answer for the address replaces it (and the pushed rows
   * past that answer's horizon are laid back over it — [[TileCache.insert]]).
   */
  synthetic?: boolean;
}

/**
 * One viewport as the cache understands it: the box being looked at **and the levels it is drawing
 * at**. The levels are not decoration — see [[TileCache.setViewports]].
 */
export interface Viewport {
  readonly box: Box;
  readonly levelF: number;
  readonly levelT: number;
  /**
   * **The lattice this viewport's levels are indices into** (T-505).
   *
   * Since the surface draws from two tiers, one lattice per cache is not enough to say what a
   * viewport wants: a level index means a different cell on each lattice, and a tile of the other
   * scheme is not this viewport's tile at all. Omitted, the cache falls back to the lattice
   * [[TileCache.setViewports]] was handed — which is what a single-tier caller has always passed.
   */
  readonly lat?: Lattice;
  /**
   * **The coarse stand-in level pair this viewport also asks for** (T-1037).
   *
   * A viewport wants its own level and one step coarser (the parent pin), and cancellation is what
   * makes that bound real. The coarse stand-in enumeration is *several* levels up — that is what
   * makes it one batch — so without being named here it would be cancelled at the end of the very
   * frame that asked for it, and the black it exists to remove would come straight back. It is one
   * exact pair, not a widened range: a stand-in is a specific level, and widening the predicate
   * instead would stop cancellation cancelling, which is the T-443 defect.
   */
  readonly standIn?: { readonly levelF: number; readonly levelT: number };
}

/**
 * A viewport this cache can measure **motion** for between frames (T-538): a [[Viewport]] that says
 * which pane it is. The id is load-bearing — displacement is per pane, and a set whose order is an
 * accident of rendering cannot be differenced positionally.
 */
export interface MovingViewport extends Viewport {
  readonly id: string;
}

/**
 * What the cache can answer. **`pending` is not a cell state** — see ui/src/surface/cellrule.ts.
 *
 * `failed` marks a place **this client asked about and has no usable answer for**: the route refused
 * it permanently (T-479), or the route is silent and the client is waiting out its backoff (T-499).
 * It is carried on `pending` rather than as a third kind deliberately: "not loaded" is already
 * structurally distinct from "never observed" — the renderer clears a place to `PENDING` and only a
 * `coverage` state byte can produce grey — so a failed place is already visibly not an unobserved
 * one, which is the claim that matters.
 *
 * The flag says *which* not-loaded it is, and since T-499 the renderer draws the difference
 * (`REFUSED_MARK`, ui/src/surface/cellrule.ts). That is the point of keeping the two apart: `pending`
 * means *wait*, `failed` means *nothing is coming until something changes*, and a mark that says
 * "loading" over a dead server is the same defect as `AWAITING` promising arrival (T-441).
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
  /**
   * **How long a resident tile may go without a successful re-read before it is reported STALE**
   * (T-1039), ms. Default [[DEFAULT_STALE_AFTER_MS]].
   *
   * The last good tile is never dropped for going stale — that is the whole point of
   * stale-while-revalidate (an unstable network must never clear what is drawn) — this only decides
   * when a copy in hand is old enough that the honesty tier should say so ([[TileCache.isStale]]).
   */
  staleAfterMs?: number;
  /** The source of jitter for backoff delays (T-1039), `[0, 1)`. Injectable so a test can pin the
   * spread; defaults to `Math.random`. */
  random?: () => number;
}

/**
 * **State N** for stale-while-revalidate (T-1039): a resident tile in [[TileCache.isStale]]'s scope
 * whose last successful confirmation is older than this — or older than [[STALE_CADENCE_MARGIN]]
 * times its OWN refresh cadence, whichever is longer — is reported stale rather than silently trusted
 * forever. 30 s is comfortably past the live edge's own fastest cadence (~1 s at level 0, T-460) and
 * short enough that a genuinely stuck network is visible within one glance at the pane's own readout,
 * not just to the request log; the cadence multiple is what keeps a healthy coarse level (32 s at
 * level 5) from flickering "stale" between its own ordinary refreshes.
 */
export const DEFAULT_STALE_AFTER_MS = 30_000;

/**
 * How many of a tile's OWN refresh cadences (`tCellNs(levelT)`) it may run over before
 * [[TileCache.isStale]] calls it overdue, when that is longer than [[DEFAULT_STALE_AFTER_MS]] itself
 * (T-1039, a review finding on the first cut). Without this a perfectly healthy level-5 tile —
 * refreshed once every 32 s by design (docs comment on [[TileCache.behindTheEdge]]) — read "stale"
 * for the last two seconds of every ordinary cycle, on no unhealthy network at all. 2x is one missed
 * cycle's worth of slack: due, then given one more full cadence to actually land, before the label
 * says anything is wrong.
 */
export const STALE_CADENCE_MARGIN = 2;

/**
 * **Jittered backoff** (T-1039): `base` plus up to `base × spread` of randomness, so a batch of
 * requests that all failed together (a retune, a reload, a network drop) do not all retry on the
 * exact same tick and hammer the route the instant it comes back — the same herd this file already
 * avoids for the live-edge lane by construction, applied here to the retry clock itself. `rand` is
 * `[0, 1)`; the jitter is additive and never shortens the wait below `base`, so it can only ever make
 * the ladder more spread out, never less patient.
 */
function jittered(base: number, rand: () => number): number {
  const r = rand();
  const spread = Number.isFinite(r) && r >= 0 && r < 1 ? r : 0;
  return base + base * 0.5 * spread;
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
  /**
   * Of [[edgeRefreshes]], the ones issued for a tile the live edge has **already passed** (T-495):
   * the completing re-ask that closes a tile whose last rows were recorded while it was off screen.
   * Counted separately because it is the number that says the T-495 gap is being filled rather than
   * merely that the live-edge lane is running — and because it is bounded: at most one per tile.
   */
  edgeRefreshCompletions: number;
  /** Failures that carried **no answer at all** — the server said nothing (T-499). Counted apart
   * from `failures` because they are the only retryable outcome left, and so the only one whose
   * rate is a property of this client's policy rather than of the route's. */
  silentFailures: number;
  /** Speculative look-ahead reads actually issued (T-538), apart from `requests`, which counts every
   * visible miss too. This is the number that answers "what did speculation cost" on its own. */
  speculativeIssued: number;
  /** Speculative reads that a later frame actually **drew** — the first [[TileCache.acquire]] hit on
   * a tile this lane fetched. `speculativeIssued - speculativeHits` is what it wasted, measured
   * rather than argued. */
  speculativeHits: number;
  /** Next-row reads issued ahead of a following edge (T-890, [[TileCache.lookAhead]]): at most one
   * per address. */
  aheadIssued: number;
  /** Rows filed from `/ws/tiles/rows` into a tile in hand (T-893). */
  rowsPushed: number;
  /** Tiles built from pushed rows before any answer for the address arrived (T-893). */
  rowTilesSynthesized: number;
}

const MB = 1024 * 1024;

/** Consecutive completed requests before the cap recovers one slot. The additive half of AIMD. */
export const RECOVER_AFTER = 8;
/**
 * **The other additive half: quiet, measured in service times** (T-539).
 *
 * [[RECOVER_AFTER]] counts *completions*, and a completion needs a request. A frozen view whose
 * tiles are all resident issues nothing at all, so a client that took a single `503` while settling
 * kept its halved cap for the rest of the session — measured pinned at 1–2 against a ceiling of 4
 * for the remaining ~16 s of every run, with nothing able to recover it until the user moved a
 * pane. That is backpressure that never lets go: the route says "busy" once and the client throttles
 * itself indefinitely, and the next time the user *does* pan they pay for a refusal that expired
 * long ago.
 *
 * The recovery is therefore **elapsed quiet, not traffic**. Nothing is requested to earn a slot
 * back: the cap is an upper bound on concurrency, so raising it with an empty queue sends no bytes.
 * That distinction is the whole point — a speculative ring of requests was the previous answer to
 * "keep traffic flowing so the cap can recover", and it was reverted off main (T-471) for charging
 * the route a slot it then abandoned. Recovery must not cost the server anything to happen.
 *
 * The window is `RECOVER_QUIET ×` the **measured** service time ([[TileCache.serverEstimateMs]]),
 * which is the same currency the earned path is paid in and makes the timed path a *floor, never a
 * shortcut*: `RECOVER_AFTER` completions at a cap of `n` take `RECOVER_AFTER / n` service times, so
 * a client with traffic always recovers at least as fast as one sitting still, and the two coincide
 * exactly at a cap of one — the state this exists to get out of. It scales with the route, too: an
 * expensive 19 MB tile stretches the window without anything being retuned.
 *
 * The clock starts at the **end of the busy backoff**, not at the refusal: quiet means quiet after
 * the route has stopped saying it is busy, and every request issued or answered restarts it. A
 * further refusal halves the cap again, so the AIMD shape is unchanged — only the increase is no
 * longer conditional on there being something to draw.
 *
 * And it applies **only while this client is idle** ([[TileCache.recoverElapsed]]): a client with
 * work in hand has completions to be paid in, and probing upward while still asking is a sawtooth
 * against a route whose other tenants are not going anywhere. Idle is what makes a refusal's
 * evidence genuinely stale, and what makes the raise free.
 */
export const RECOVER_QUIET = RECOVER_AFTER;
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
// **T-532: it is a share of the LANE, charged per pass over the edge.** It was charged per tile,
// which made the period for any one live-edge tile `members × REFRESH_DUTY × cost` — so the same
// rule delivered a 4× slower live edge the moment T-501's finer floor cut that edge into four tiles
// instead of one. See [[TileCache.nextRefresh]] for the measurement and for why the bound this
// comment states is only now the bound the code enforces.
/**
 * The refresh lane a tile belongs to: its **cost class**, which on this route is its level.
 *
 * A tile's production cost is a function of the level and of nothing else this client can see —
 * measured against a real server, `0/0` is 37 ms over 65 536 source cells in one history-lock hold
 * and `9/0` is 246 ms over 2 097 152 cells in 37 holds. Keying the lane by level therefore separates
 * exactly the tenants whose costs differ, and puts two panes at the same zoom in one lane, which is
 * right: they cost the same and should share a turn.
 */
const laneOf = (a: TileAddr): string => `${a.levelF}/${a.levelT}`;

/** Are these two spans the same width, to within float noise? A pan adds the same delta to both
 * edges, so its width is preserved only up to an ulp; a zoom changes it by a factor. */
const sameSize = (a: number, b: number): boolean =>
  Math.abs(a - b) <= 1e-9 * Math.max(1, Math.abs(a), Math.abs(b));
/** How often [[TileCache.refreshEdge]] may walk the resident set, ms. The walk is cheap; doing it
 * at frame rate would still be 60× more often than the finest tile can change. */
const EDGE_SCAN_MS = 250;

/**
 * **How far ahead of a following edge the next row is asked for, in measured service times**
 * (T-890). The lead is `min(one tile, AHEAD_SERVICES x serverMs)`.
 *
 * Early costs nothing extra: the address is fetched once whether it is asked for before the edge
 * reaches it or after, and the copy is revalidated by the ordinary refresh lane once the edge is
 * inside it. Late costs exactly the defect — a pane whose span is shorter than a tile has nothing
 * else in hand once its old row scrolls out. So the factor errs long: on a healthy route (a mean of
 * 100-250 ms) it is one to two seconds of lead; on a busy one (a mean of seconds, which is when the
 * blank was seen) it reaches the whole tile, i.e. the next row is asked for as soon as the edge
 * enters the current one. `serverMs` is the mean over every completion, including the waits on the
 * route's history lock that a cold miss also pays, so it is the right clock for "ask, then have".
 */
const AHEAD_SERVICES = 8;
/** The [[InFlight.owner]] of a look-ahead read: on the global budget like an orphan (-1), but not
 * sent alone the way a revalidation is. */
const AHEAD_OWNER = -2;
/** How long after a push a column counts as fed, ms: a few row periods at the finest level, so a
 * feed that stalls or closes hands its column back to the polling lane within seconds (T-893). */
const FEED_FRESH_MS = 2000;
/** A tile's column key: its address without the time index. */
/**
 * The resolution claim a tile built from pushed rows makes (T-902): exactly what the rows' blocks
 * stated, merged to the weaker — and `unknown` where no block stated one. Never inferred.
 */
function claimOf(r: RowResolution | null, cells: number): Pick<TileData, "tier" | "answeredLevel" | "fold" | "measured"> {
  if (!r) return { tier: "unknown", answeredLevel: -1, fold: { frequency: "exact", time: "exact" }, measured: { nf: cells, nt: cells } };
  return {
    tier: r.tier,
    answeredLevel: r.answeredLevel,
    fold: r.fold,
    measured: { nf: r.measuredNf, nt: Math.max(1, Math.min(cells, Math.round(cells / r.timeStretch))) },
  };
}

const columnOf = (a: TileAddr): string => keyOf({ ...a, tIndex: -1 });

/**
 * How many speculated addresses [[TileCache.speculated]] remembers (T-538). It only has to be large
 * enough that a long pan cannot walk off the end of its own history and start asking twice; at
 * ~208 tiles for a full screen, 4096 is twenty screens' worth, and the set holds keys, not tiles.
 */
const SPECULATED_MEMORY = 4096;

/**
 * **The silence backoff** (T-499): after a failure that carried no answer at all, how long before
 * this client may touch the route again, and the ceiling that wait doubles up to.
 *
 * T-479 made everything the server *said* terminal, which leaves exactly one retryable outcome —
 * *the server said nothing* — and that outcome was paced by the render loop: `acquire` runs for
 * every place on every frame, so a socket that refuses every connection is asked again at frame
 * rate. Measured in a browser with `hk serve` SIGKILLed under a live page (`ui/e2e/canvas-journey`
 * test 4): **56 failed requests in the first 5 s and 55 in the second** — flat, no decay, which is
 * the definition of a retry loop rather than a client winding down. (The ticket carries 182/181
 * from the rig it was filed on; the shape is the number that matters, and the shape is *flat*.)
 *
 * The backoff is on the **transport, not the place**, and that is the whole point: "connection
 * refused" is not a fact about a tile address, it is a fact about the server, so a per-key retry
 * schedule would still have let 30 viewport tiles each run their own ladder and multiply the wire
 * traffic by 30. One gate, doubling per consecutive silence to [[OFFLINE_MAX_BACKOFF_MS]], and
 * **one probe at a time while it is armed** ([[TileCache.effectiveLimit]]) — half-open, the standard
 * shape — so the cost of a dead server is one request per interval instead of four.
 *
 * It is not terminal, and must not become terminal: a server that restarts *is* a change of answer,
 * which is exactly [[retryable]]'s enumeration. A client that gave up for the session would need a
 * page reload to come back, and that is the defect one step further on.
 *
 * **T-523 widened what arrives here**, and the widening is safe *because* of the shape above: a
 * `5xx` the route cannot emit ([[fromTheRoute]]) is a proxy saying the origin did not answer, so it
 * is the same outcome as a refused socket and takes the same ladder. Being wrong about one costs a
 * backed-off probe; being wrong the other way — the pre-T-523 reading — costs a region of the
 * canvas that never draws again this session.
 */
const OFFLINE_BACKOFF_MS = 500, OFFLINE_MAX_BACKOFF_MS = 30_000;

/** One request the client is waiting on, and what it is being counted against. */
interface InFlight {
  readonly ctrl: AbortController | null;
  readonly startedAt: number;
  /** Index into the viewports of the last [[TileCache.setViewports]], -1 for none, or [[AHEAD_OWNER]]. */
  readonly owner: number;
}

/**
 * What [[TileCache]] tells its source about one request, beyond the address.
 *
 * `solo`: this request must not wait on any other — it is the live edge's revalidation, which the
 * product never gates on generating other tiles (T-460, T-573). A source that batches sends it alone.
 *
 * `lane`: which set this address belongs to, when the caller needs the set answered **together**
 * (T-1037). A batching source keeps one lane's addresses out of another's request, so the coarse
 * stand-in enumeration arrives as one picture instead of being cut up among a viewport's own tiles.
 * Absent is the ordinary lane; it is a grouping key and nothing else reads it.
 */
export interface TileSourceHint {
  readonly solo?: boolean;
  readonly lane?: string;
}

export class TileCache<T> {
  readonly budgetBytes: number;
  /** The lane each queued/in-flight address belongs to, when its caller named one (T-1037). Only
   * ever read to build the hint the source gets; dropped the moment the request retires. */
  private lanes = new Map<string, string>();
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
  /**
   * The live-edge revalidation lanes: their own queues, so they can never take a slot from
   * [[queue]] — **one lane per cost class**, keyed by the level a tile is drawn at (T-490).
   *
   * It was ONE queue and one cadence for every following viewport, and that is the same defect
   * T-460 fixed one level down. Its `done` handler already explains why `REFRESH_DUTY x serverMs`
   * was wrong — a mean that folds in the minimap's coarse read charges the live edge for a tile it
   * is not — and the answer there was to measure the lane's own cost on its own request. But the
   * *lane* was still a mixture: a minimap is a following viewport too, so its tiles queue here
   * beside the pane's, and one 725 ms revalidation set a 2.2 s gate on a 183 ms one.
   *
   * A tile's cost is a function of its level and nothing else here — measured against a real
   * server, `level 0/0` is 37 ms over 65 536 source cells in one history-lock hold, `level 9/0` is
   * **246 ms over 2 097 152 cells in 37 holds** — so the level *is* the cost class, and charging
   * each class its own observed cost is the same medicine at the right granularity. Panes at the
   * same zoom share a lane, which is correct: they cost the same.
   *
   * # Why nobody saw this, and what to check before trusting a green live-edge test
   *
   * `ui/e2e/live-edge.e2e.mjs` passed on `main` **for a reason unrelated to what it asserts.** The
   * view lattice named a 12 x 15 grid of levels over a 4 x 4 store, so the minimap's address was a
   * permanent `400`; T-479's terminal rule asked it once and never again, and this lane plus all
   * four in-flight slots belonged to the live pane. T-482 declares the readable ceiling, the minimap
   * becomes servable, and a second tenant appears here for the first time. Measured in a browser,
   * same gestures and same box, with only the server binary differing:
   *
   * ```
   * main   0/0 x68 @69ms    1/1 x3 @513ms   11/0 x2 @153ms   (73 requests, 5/5 samples drawn)
   * before 9/0 x16 @725ms   0/0 x14 @183ms  9/1 x8 @398ms    (41 requests, 2/5 drawn — FLAT)
   * after  0/0 x45 @128ms   9/0 x18 @650ms  9/1 x8 @660ms    (74 requests, 5/5 drawn)
   * ```
   *
   * `11/0 x2` is the whole story. **So if the live edge goes quiet, look at what the minimap's
   * address is answering before concluding anything about this lane** — a viewport that is failing
   * is also a viewport that is costing nothing, and the two are indistinguishable from here.
   *
   * **Fairness is a sixth property, not a replacement for T-460's five.** Only the live-edge
   * address, only for a FOLLOWING viewport, only at the level it was drawn at, never inside one
   * `tCellNs(level_t)`, and one revalidation in flight into a slot the ordinary queue could not use
   * — all five still hold and are still asserted in `ui/test/surface-cache.test.ts`. This adds: each
   * cost class gets its own turn and its own clock.
   */
  private refreshLanes = new Map<string, TileAddr[]>();
  private refreshQueued = new Set<string>();
  /** Round-robin cursor over [[refreshLanes]], so a cheap lane never waits behind an expensive one
   * for a slot it is entitled to. */
  private refreshCursor = 0;
  /** The lane whose revalidation is in flight, so [[issue]]'s completion charges the right one. */
  private refreshingLane: string | null = null;
  /**
   * **How many of the current PASS over this lane are still to be issued** (T-532).
   *
   * The unit the duty gate is charged to. See [[nextRefresh]] for why it cannot be the tile.
   */
  private refreshPassLeft = new Map<string, number>();
  /**
   * **When each resident tile's data was last taken in, ms** — but only as a cadence clock. It is
   * stamped by [[pumpRefresh]] at the moment a revalidation is ISSUED, not when it lands, so the
   * refresh interval is measured from *asking* and a tile is never asked for again inside the period
   * its newest cell spans. **Not** what [[isStale]] reads (see [[lastGoodAt]]): an issued-but-not-
   * yet-answered, or issued-and-failed, request must not read as a confirmation.
   */
  private refreshedAt = new Map<string, number>();
  /**
   * **When each resident tile was last CONFIRMED, ms** (T-1039): stamped only by [[insert]], on a
   * successful fetch or revalidation. This is [[isStale]]'s clock. A failed or slow revalidation
   * must never advance it — the whole of stale-while-revalidate is that an unconfirmed copy stays on
   * screen and stays honestly labelled unconfirmed, and [[refreshedAt]] alone cannot say that: it is
   * stamped at issue, so a request that never comes back would otherwise look freshly confirmed for
   * as long as the network stays down.
   */
  private lastGoodAt = new Map<string, number>();
  /**
   * **Which resident tiles [[isStale]] may even consider, and the threshold each is judged
   * against** — recomputed from scratch on every [[refreshEdge]] scan ([[recomputeStaleScope]]),
   * fixing a review finding on the first cut.
   *
   * A key is present here only when BOTH of the facts that decide whether a tile is still IN the
   * revalidation loop at all hold: **drawn at its own level by a FOLLOWING viewport this frame**
   * ([[drawnBy]] — a frozen pane's tiles are never in scope, because [[refreshEdge]] itself never
   * revalidates them) **and not yet [[behindTheEdge]]'s "sealed"** — a copy whose own fetch already
   * reached the tile's end can never be revalidated again and is correctly resident forever, not
   * stale. The value is `max(staleAfterMs, STALE_CADENCE_MARGIN × this tile's OWN refresh cadence)`,
   * computed here (where `lat` is in scope) rather than re-derived in [[isStale]] from `this.lat` —
   * which [[setViewports]], a wholly different call path, is the only thing that ever sets, so a
   * caller that only ever drives `refreshEdge` (every test in this file, and the production preview
   * driver on its own dedicated path) must not silently read a stale `null`.
   *
   * Without any of this a flat wall-clock age flagged every sealed tile, every frozen pane's tiles
   * and every coarse level slower than the flat default (32 s at level 5) as "stale" the moment the
   * age passed, on a perfectly healthy network — the "UI claims something it didn't measure" defect
   * this whole feature exists to avoid, aimed at itself.
   */
  private staleScope = new Map<string, number>();
  /** The next instant each lane may issue, and the next one the resident set may be walked. Per
   * lane, because a share of measured cost is only a fair share if it is that lane's own cost. */
  private refreshNextIssue = new Map<string, number>();
  /**
   * The last few completions of each lane, newest last — what [[refreshCostOf]] takes its minimum
   * over (T-491). Kept per lane for the same reason [[refreshNextIssue]] is, and kept as a *window*
   * rather than as one sample for the reason spelled out there.
   */
  private refreshCosts = new Map<string, number[]>();
  /**
   * Where each pane's box was on the previous frame, by pane id — the whole state of the T-538
   * look-ahead lane, and the reason it has no queue. See [[prefetchAhead]].
   */
  private lastBox = new Map<string, { box: Box; levelF: number; levelT: number; scheme: string }>();
  /** Keys with a speculative read in flight. At most one, and **never aborted** — [[setViewports]]
   * skips them, which is what keeps this lane from charging the route a slot it will not use. */
  private speculating = new Set<string>();
  /** Every address this lane has ever asked for. Speculation is once per address per session, so it
   * can never become a poll and can never show up as a re-ask. Bounded by [[SPECULATED_MEMORY]]. */
  private speculated = new Set<string>();
  /** Speculative tiles that arrived and have not yet been drawn — drained by [[acquire]] into
   * `stats.speculativeHits`, so the lane's benefit is counted rather than asserted. */
  private speculativeResident = new Set<string>();
  /**
   * **The row each following viewport's edge is about to enter, keyed** (T-890). Recomputed on every
   * edge scan from the tiles in hand ([[lookAhead]]); [[setViewports]] keeps these queued and in
   * flight although no viewport box reaches them yet, because reaching them is the whole point.
   */
  private ahead = new Set<string>();
  /** Every look-ahead address ever ISSUED, so the lane asks each ONCE (bounded like [[speculated]]).
   * A copy evicted before the edge arrives becomes an ordinary miss, never a poll. */
  private aheadAsked = new Set<string>();
  private nextEdgeScan = 0;
  /**
   * **Rows pushed by `/ws/tiles/rows`, filed by tile address** (T-893). Kept beside the tiles rather
   * than only written into them, because a tile answer that lands later is older at its top than
   * the rows already pushed — the route's horizon trails the feed — and without these the newest
   * rows would vanish from the screen until the next push.
   */
  private pushed = new RowAccumulator();
  /** When each column last had rows pushed, ms — the polling lane stands down while it is fresh. */
  private pushedAt = new Map<string, number>();
  /**
   * Frame on which each resident tile was last drawn as a STAND-IN for a missing one (T-893). A
   * stand-in drawn for a following pane is on screen at the live edge, so it is kept fresh like a
   * tile drawn at its own level ([[refreshEdge]]); before, only exact-level tiles were, and a parent
   * pin standing in for a new row stayed at whatever horizon it was first fetched at and drew nothing.
   */
  private standInFrame = new Map<string, number>();
  /** Refreshes queued for a tile only because it was standing in ([[pumpRefresh]] re-checks it). */
  private standInQueued = new Set<string>();
  /** Every key revalidated because it was drawn as a stand-in over a following pane (T-893) — for
   * the tests that assert a refresh is only ever for something on screen. */
  readonly refreshedAsStandIn = new Set<string>();
  /**
   * The newest capture instant the surface has been told about, as of the last [[refreshEdge]] —
   * stamped onto every tile this cache asks for, so a resident copy knows how much of its own span
   * it can possibly hold (T-495).
   *
   * `-Infinity` until an edge is reported, which makes every tile fetched before then *behind* and
   * therefore re-askable once. That is the conservative direction and the only safe one: "we do not
   * know whether this copy is complete" is not "it is complete", the same rule as
   * `BiasTee::Unknown` is not `Off`. It costs at most one extra request per tile, and only for
   * tiles a following viewport is drawing.
   */
  private edgeNs = Number.NEGATIVE_INFINITY;
  private clock = 0;
  private frame = 0;
  private bytes = 0;
  private busyUntil = 0;
  private limit: number;
  /** The most this client may recover to: the server's own number, never raised by anything here. */
  private ceiling: number;
  /** See [[inFlightShareStatedAt]]. */
  private shareStatedAt: number | null = null;
  /** The route's last statement of what this client holds, and when (T-959). Null until it states
   * one, in which case the abandoned-read charge is this client's own estimate and nothing more. */
  private heldStated: number | null = null;
  private heldStatedAt: number | null = null;
  /** Consecutive refusals, for the backoff; and completions since the last one, for the recovery. */
  private refusals = 0;
  private goodRuns = 0;
  /**
   * The last instant this client touched the route, or was told to wait — the clock the
   * elapsed-quiet recovery measures from (T-539). See [[RECOVER_QUIET]] and [[recoverElapsed]].
   */
  private lastWireAt = 0;
  /** Consecutive failures that carried **no answer at all**, and the instant the gate they arm
   * opens (T-499). Zero means the route is answering, and every path here reads `silences > 0`
   * rather than a second flag, so "are we backing off" has one source. */
  private silences = 0;
  private silentUntil = 0;
  /**
   * **Places a silent probe was spent on, owed the BACK of the queue when next wanted** (T-903).
   *
   * While the gate is armed the client asks one place per opening, and the queue is LIFO. A probe
   * that fails is not re-queued here — the next frame's `acquire`/`prefetch` re-schedules it — and
   * `schedule` pushes it on TOP, above every place that was already waiting, so the next opening
   * asked for the very same place again, and the one after, for the whole outage. Every other
   * wanted place was starved of a probe. Measured in `ui/e2e/live-edge` (T-523's case): a probe that
   * went out mid-zoom landed on an intermediate level that stayed wanted as the final level's pin
   * (`level_f + 1`), and in 75 s of outage the pane's own level was never asked once ("levels
   * refused: 2/1 1/1"). So a place a silence answered re-enters at the bottom and the probes take
   * turns across everything wanted. Cleared by an answer, when order stops mattering.
   */
  private silenced = new Set<string>();
  /** Measured mean production time, ms. See [[observe]]. */
  private serverMs: number;
  /** See [[TileCacheOptions.staleAfterMs]]. */
  private readonly staleAfterMs: number;
  /** See [[TileCacheOptions.random]]. */
  private readonly random: () => number;
  readonly stats: TileCacheStats = {
    uploads: 0, hits: 0, misses: 0, evictions: 0, refetchAfterEvict: 0, requests: 0,
    failures: 0, busyRefusals: 0, cancelled: 0, abandoned: 0, overBudgetFrames: 0, distinctKeys: 0,
    edgeRefreshes: 0, edgeRefreshApplied: 0, terminalFailures: 0, edgeRefreshCompletions: 0,
    silentFailures: 0, speculativeIssued: 0, speculativeHits: 0, aheadIssued: 0,
    rowsPushed: 0, rowTilesSynthesized: 0,
  };

  constructor(
    private readonly tex: TileTextures<T>,
    private readonly source: (addr: TileAddr, signal?: AbortSignal, hint?: TileSourceHint) => Promise<TileData>,
    opts: TileCacheOptions = {},
  ) {
    this.budgetBytes = opts.budgetBytes ?? 96 * MB;
    this.ceiling = this.limit = Math.max(1, opts.inFlight ?? 4);
    this.maxQueue = opts.maxQueue ?? 4096;
    this.busyBackoffMs = opts.busyBackoffMs ?? 200;
    this.serverMs = clamp(opts.serverMsGuess ?? 400, MIN_SERVER_MS, MAX_SERVER_MS);
    this.now = opts.now ?? (() => Date.now());
    this.staleAfterMs = Math.max(0, opts.staleAfterMs ?? DEFAULT_STALE_AFTER_MS);
    this.random = opts.random ?? Math.random;
  }

  get residentTiles(): number { return this.map.size; }
  get residentBytes(): number { return this.bytes; }
  /** The AIMD cap as of **now** — the elapsed-quiet recovery is folded in on read (T-539), so a
   * readout and a test see the same number the next pump would use. */
  get inFlightLimit(): number { this.recoverElapsed(); return this.limit; }
  /**
   * The cap this client may recover to: the server's own number, and since T-630 **its share** of
   * it when the route states one. Shown beside the operating cap because "why is this tab only
   * running two at a time" has two different answers — backing off, or another client is here —
   * and they are not the same finding.
   */
  get inFlightCeiling(): number { return this.ceiling; }
  /**
   * When the route last STATED this client's share (an answer's `cost.in_flight_share`, or a
   * refusal naming it), on this cache's clock — or null if it never has, in which case
   * [[inFlightCeiling]] is this client's own starting assumption, not the route's word.
   *
   * The share is learned from answers and from nothing else, and the route decides it when it
   * ADMITS a read. So it is a fact as of the last answer, never a live one: a tab that has stopped
   * asking keeps the number it was last told while other tabs come and go. A readout that states
   * the share must say how old it is (T-630's contention finding).
   */
  get inFlightShareStatedAt(): number | null { return this.shareStatedAt; }
  /** How long ago, in ms on this cache's clock, [[inFlightShareStatedAt]] was — or null. */
  get inFlightShareAgeMs(): number | null {
    return this.shareStatedAt === null ? null : Math.max(0, this.now() - this.shareStatedAt);
  }
  /**
   * What the route last **stated** this client holds (`cost.in_flight_held`), or null if it never
   * has — in which case the abandoned-read charge is an estimate and says so. Reported beside the
   * charge because "why is this client not issuing" has two answers — its own leftover reads, or
   * another client — and only the route can tell them apart.
   */
  get serverHeldStated(): number | null { return this.heldStated; }
  /** How long ago, in ms on this cache's clock, [[serverHeldStated]] was stated — or null. */
  get serverHeldStatedAgeMs(): number | null {
    return this.heldStatedAt === null ? null : Math.max(0, this.now() - this.heldStatedAt);
  }
  get inFlightCount(): number { return this.inflight.size; }
  get queueDepth(): number { return this.queue.length; }
  /** Live-edge revalidations queued but not yet issued (T-460). Its own lane, never [[queue]]. */
  get refreshDepth(): number {
    let n = 0;
    for (const q of this.refreshLanes.values()) n += q.length;
    return n;
  }
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
      // The first draw of a tile the look-ahead lane guessed at: its benefit, counted where it
      // happens rather than argued from how a pan ought to behave (T-538).
      if (this.speculativeResident.delete(key)) this.stats.speculativeHits++;
      return { kind: "resident", entry: e };
    }
    this.stats.misses++;
    const why = this.terminal.get(key);
    if (why !== undefined) return { kind: "pending", failed: true };
    this.schedule(addr);
    // **Asked, and no usable answer came back** — the other half of [[Residency.failed]] (T-499).
    // The place is still queued (recovery needs it to be), but while the route is silent it is not
    // *arriving*, and a renderer that draws it as "loading" is promising what it cannot deliver.
    return this.silences > 0 ? { kind: "pending", failed: true } : { kind: "pending" };
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

  /** Is a copy of this place in hand? No scheduling, no pin, no statistics: how the renderer asks
   * before deciding whether the coverage survey may answer the place instead (T-580). */
  isResident(addr: TileAddr): boolean { return this.map.has(keyOf(addr)); }

  /**
   * **State N of stale-while-revalidate** (T-1039): is the resident copy of this place overdue for a
   * confirmation it is actually still owed?
   *
   * `false` for a place not resident at all — staleness is a fact about a copy in hand, not about a
   * miss, which is already drawn as `pending`/`refused` and needs no second mark.
   *
   * **`false` for anything outside [[staleScope]], and that gate is not optional — it is what this
   * predicate is FOR.** The first cut read only [[lastGoodAt]]'s wall-clock age, and a review of it
   * found the exact three false-positive classes that follow from that: a **sealed** tile ([[behindTheEdge]]
   * false — its own fetch already reached the tile's end, so no further answer is coming and none is
   * owed), a **frozen pane's** tiles (never revalidated at all, because [[refreshEdge]] is only ever
   * given `following` viewports — see its own doc comment), and a **coarse level** slower than
   * `staleAfterMs` by design (32 s at level 5) flickering "stale" between its own healthy refreshes.
   * All three are read from `staleScope` ([[recomputeStaleScope]]) rather than re-derived here, so
   * this predicate can never disagree with the loop that actually decides what gets revalidated.
   *
   * **A tile the row-push lane is actively feeding is confirmed continuously**, by rows rather than
   * by a tile answer ([[fedRecently]]) — it is not stale merely because no `/api/tiles` read has
   * landed for it yet.
   *
   * Past those gates: [[lastGoodAt]] is stamped by [[insert]] on every successful answer, including a
   * revalidation that only confirmed the same bytes, so a tile the network keeps failing to refresh
   * ages here exactly as long as it has genuinely gone unconfirmed — never reset by a failed attempt
   * (a failed or slow batch must not clear what is drawn, and must not silently un-stale it either).
   * The threshold is `staleAfterMs`, or [[STALE_CADENCE_MARGIN]] cadences when that is longer, so a
   * tile whose own design refreshes it slower than the flat default is judged against ITS OWN clock.
   */
  isStale(addr: TileAddr): boolean {
    const key = keyOf(addr);
    const e = this.map.get(key);
    if (!e) return false;
    const threshold = this.staleScope.get(key);
    if (threshold === undefined) return false; // out of scope: sealed, frozen, or never offered at all.
    if (this.fedRecently(e.addr, this.now())) return false;
    const at = this.lastGoodAt.get(key);
    if (at === undefined) return true; // in scope, still owed a confirmation, and never got one.
    return this.now() - at >= threshold;
  }

  /**
   * **Rebuild [[staleScope]] from scratch** (T-1039): exactly which resident tiles [[isStale]] may
   * even consider, as of this scan, and the threshold each is judged against.
   *
   * The membership test is two conditions, both already load-bearing elsewhere in this file so this
   * is never a second derivation of either: **drawn at its own level by a viewport in `following`**
   * ([[drawnBy]] — the identical test [[refreshEdge]]'s own loop uses to decide what it revalidates,
   * so scope can never disagree with the revalidation loop about what is "on screen and live") **and
   * not yet [[behindTheEdge]]'s "sealed"** (its own fetch already reached the tile's end — nothing
   * further is coming, ever, so nothing further is owed). Cleared entirely when nothing follows at
   * all (every pane frozen): [[refreshEdge]] itself does no work in that case, and neither should
   * this. The threshold is computed HERE, from the `lat` this scan was actually given, rather than in
   * [[isStale]] from `this.lat` — see [[staleScope]]'s own comment for why that indirection is wrong.
   */
  private recomputeStaleScope(lat: Lattice, edgeNs: number, following: readonly Viewport[]): void {
    if (!following.length) { this.staleScope.clear(); return; }
    for (const e of this.map.values()) {
      const inScope = this.behindTheEdge(lat, e, edgeNs) && following.some((v) => this.drawnBy(lat, v, e.addr));
      if (!inScope) { this.staleScope.delete(e.key); continue; }
      const cadenceMs = tCellNs(lat, e.addr.levelT) / 1e6;
      this.staleScope.set(e.key, Math.max(this.staleAfterMs, STALE_CADENCE_MARGIN * cadenceMs));
    }
  }

  /**
   * **Places the coverage survey settles as never sampled — no lane may start a request for one**
   * (T-905). T-580 gated the renderer's own misses on the survey, but a request can start from
   * other lanes: T-538's pan look-ahead ([[prefetchAhead]]) issues straight to the route, and it
   * fires exactly when this cache is idle — which a pane over never-sampled spectrum always is,
   * because the survey answered every place it draws. The fog-of-war e2e caught that as a tile
   * requested over never-swept band C about one run in nine. So the rule lives HERE, where every
   * miss begins ([[pump]] and [[prefetchAhead]]), not in each caller. Consulted only for a place
   * not in hand: a resident copy's revalidation is not a skip decision.
   *
   * `null` (the default) settles nothing — a cache with no survey fetches as before.
   */
  setSettled(fn: ((addr: TileAddr) => boolean) | null): void { this.settled = fn; }
  private settled: ((addr: TileAddr) => boolean) | null = null;

  /**
   * Want this tile soon, but do not draw it: the parent-level pin, the coarse stand-in set, and pan
   * prefetch.
   *
   * `lane` names a set that must be answered **together** (T-1037): a batching source keeps one
   * lane's addresses out of another's request, so the coarse stand-in enumeration is one answer and
   * not a few addresses riding in whichever chunk of a viewport's own tiles they landed in.
   */
  prefetch(addr: TileAddr, lane?: string): void {
    if (this.map.has(keyOf(addr))) { this.peek(addr, true); return; }
    if (lane !== undefined) this.lanes.set(keyOf(addr), lane);
    this.schedule(addr);
  }

  /**
   * The `(levelF, levelT)` pairs this cache holds a tile of, on `scheme` — `"<levelF>|<levelT>"`.
   *
   * What makes an **unbounded** ancestor search cheap (T-1037). "PENDING only when no ancestor
   * exists at any level" read as a search over every level pair would be `O(levels²)` map lookups
   * per missing tile per frame; the resident set is at most a few hundred tiles across a handful of
   * level pairs, so the search runs over the pairs that could possibly answer and over nothing else.
   * Recomputed per call by the caller's frame, not cached: residency changes under it.
   */
  residentLevels(scheme?: string): Set<string> {
    const out = new Set<string>();
    for (const e of this.map.values()) {
      if (scheme !== undefined && e.addr.scheme !== scheme) continue;
      out.add(`${e.addr.levelF}|${e.addr.levelT}`);
    }
    return out;
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
    // A place whose silent probe just failed waits behind the others (T-903, [[silenced]]).
    if (this.silenced.delete(key)) this.queue.unshift(addr);
    else this.queue.push(addr);
    this.queued.add(key);
    if (this.queue.length > this.maxQueue) {
      const dropped = this.queue.splice(0, this.queue.length - this.maxQueue);
      for (const d of dropped) { this.queued.delete(keyOf(d)); this.lanes.delete(keyOf(d)); }
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
    // The next row of a following edge is wanted although no box reaches it yet (T-890, [[lookAhead]]).
    const wanted = (a: TileAddr) => this.ahead.has(keyOf(a)) || viewports.some((v) => this.wants(lat, v, a));
    const keep: TileAddr[] = [];
    for (const a of this.queue) {
      if (wanted(a)) keep.push(a);
      else { this.queued.delete(keyOf(a)); this.lanes.delete(keyOf(a)); this.stats.cancelled++; }
    }
    this.queue = keep;
    for (const [key, f] of this.inflight) {
      // **A speculative read is never aborted** (T-538). It is outside every viewport box by
      // construction — that is what makes it a guess — so this predicate would abort it on the very
      // next call whether or not the view moved. More importantly, aborting it is the defect that
      // reverted T-471: the abort reaches the browser and not `hk-api`, so the route goes on
      // producing a tile nobody will read and holds the slot while it does ([[abandon]]). There is
      // at most one of these, it was started only while this client held nothing at all, and letting
      // it finish is the only thing that actually gives the slot back.
      if (this.speculating.has(key)) continue;
      const a = parseKey(key);
      if (a && !wanted(a) && f.ctrl) { f.ctrl.abort(); this.stats.cancelled++; this.abandon(f); }
    }
  }

  /** Is this tile one that viewport is drawing — at its level, or one step coarser (the pin)?
   *
   * **Scheme first** (T-505): a level index is a statement about one lattice, so a tile of the
   * other tier is never this viewport's, whatever its indices say. Without this a detail-tier tile
   * and an overview-tier tile with the same `(level_f, level_t)` would each keep the other alive
   * and cancellation would silently stop cancelling — the T-443 defect the levels were added to
   * fix, one lattice up. */
  private wants(lat: Lattice, v: Viewport, a: TileAddr): boolean {
    const l = v.lat ?? lat;
    if (a.scheme !== l.scheme || !intersects(l, a, v.box)) return false;
    // The coarse stand-in set (T-1037): the one level pair above the pin this viewport also asked
    // for. Matched exactly, so it keeps that set alive and nothing else.
    const si = v.standIn;
    if (si && a.levelF === si.levelF && a.levelT === si.levelT) return true;
    return a.levelF >= v.levelF && a.levelF <= v.levelF + 1 &&
      a.levelT >= v.levelT && a.levelT <= v.levelT + 1;
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
   * **What the route says this client holds replaces what this client guessed** (T-959).
   *
   * [[abandon]]'s charge is an estimate — the route's *measured mean* minus what this read has
   * already run — and the mean is the wrong number for the read that matters: under load an
   * overview tile takes 7-9 s, so the charge expires while the route is still producing it, and the
   * page's next reads are refused over slots its own leftover reads still hold. A client that then
   * reads those refusals as contention halves its cap for its own slow reads (T-932's release-
   * candidate red: cap pinned at 1, refused with nothing of its own on the wire).
   *
   * Every tile answer and every refusal already states `cost.in_flight_held` — the slots this
   * client held at that instant, server-side. Against the reads it is still *waiting* for (the one
   * thing the route cannot know) that gives two bounds the charge is squeezed between: it can never
   * exceed the slots the route says it holds, and it can never be fewer than those slots minus the
   * reads still wanted. Extra charges are dropped, missing ones are added, and a charge the
   * statement positively attributes to an abandoned read has its estimate re-anchored to now,
   * because the route's word is newer than an expiry computed when the read was abandoned.
   *
   * The expiry stays, so nothing can wedge: with no further statement each charge lapses after a
   * service time, the client asks again, and a refusal re-states the count. What changes is that
   * the client no longer *asserts* a slot is free while the route is holding it.
   *
   * @param serverHeld `cost.in_flight_held`, this client's holdings as the route counted them.
   * @param includesThisRead whether the read being answered is one of them — true for an admitted
   * answer (its slot is released as the answer is written), false for a refusal and for a hot-tile
   * -cache hit, neither of which took one.
   */
  private reconcileHeld(serverHeld: number, includesThisRead: boolean): void {
    if (!Number.isFinite(serverHeld) || serverHeld < 0) return;
    const t = this.now();
    // Reads this client is still waiting for. It is an UPPER bound on how many of the route's held
    // slots are live rather than abandoned: a batched source (T-573) answers several of these on
    // one worker, so the count of keys in flight is never fewer slots than they occupy and may be
    // many more.
    const waiting = Math.max(0, this.inflight.size - (includesThisRead ? 0 : 1));
    // Hence two SOUND bounds on the abandoned reads the route is still producing, rather than one
    // number that assumes a slot per key:
    //  - at most `serverHeld` — this client cannot be holding more slots than the route says it is;
    //  - at least `serverHeld - waiting` — even if every read it waits for holds a slot of its own.
    const upper = serverHeld;
    const lower = Math.max(0, serverHeld - waiting);
    this.abandonedUntil = this.abandonedUntil.filter((until) => until > t);
    // Ascending, so trimming drops the charges the route has demonstrably finished and the
    // re-anchoring below lands on the ones nearest to being released.
    this.abandonedUntil.sort((a, b) => a - b);
    while (this.abandonedUntil.length > upper) this.abandonedUntil.shift();
    while (this.abandonedUntil.length < lower) this.abandonedUntil.push(t + this.serverMs);
    // **Re-anchored only up to the LOWER bound**, because only those are charges the statement
    // positively attributes to an abandoned read. Re-anchoring a charge the route's held count
    // could equally be explaining with this client's own live reads would be charging the same slot
    // twice, for as long as the answers kept coming — a stall, in the name of not releasing early.
    for (let i = 0; i < Math.min(lower, this.abandonedUntil.length); i++) {
      this.abandonedUntil[i] = Math.max(this.abandonedUntil[i], t + this.serverMs);
    }
    this.heldStatedAt = t;
    this.heldStated = serverHeld;
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
    if (edgeNs > this.edgeNs) this.edgeNs = edgeNs;
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
    if (!Number.isFinite(edgeNs)) return 0;
    // Stamped even when nothing is following, because it is what the NEXT fetch records about itself.
    if (edgeNs > this.edgeNs) this.edgeNs = edgeNs;
    // Nothing following, nothing ahead: a frozen pane's next row is not coming towards it. And
    // nothing is in scope for staleness either — the same reason, stated for [[isStale]] (T-1039).
    if (!following.length) { this.ahead.clear(); this.staleScope.clear(); return 0; }
    const t = this.now();
    if (t < this.nextEdgeScan) return 0;
    this.nextEdgeScan = t + EDGE_SCAN_MS;
    this.recomputeStaleScope(lat, edgeNs, following);
    const ahead = this.lookAhead(lat, edgeNs, following);
    let n = 0;
    for (const e of this.map.values()) {
      const key = e.key;
      if (this.refreshing.has(key) || this.refreshQueued.has(key) || this.inflight.has(key)) continue;
      if (!this.behindTheEdge(lat, e, edgeNs)) continue;
      // Drawn at its own level, or drawn as a stand-in for a missing tile on this or the last frame
      // over a following pane (T-893): either way it is what that pane shows at its live edge.
      const standIn = (this.standInFrame.get(key) ?? -2) >= this.frame - 1;
      const own = following.some((v) => this.drawnBy(lat, v, e.addr));
      if (!own && !(standIn && following.some((v) => e.addr.scheme === lat.scheme && intersects(lat, e.addr, v.box)))) continue;
      if (!own) this.standInQueued.add(key);
      // **A column the row feed is keeping current is not polled** (T-893): its rows arrive as they
      // are recorded. The completing re-ask once the edge has left the tile still goes out — pushed
      // rows may be provisional, and the sealed tile is then the authority (docs/api.md).
      if (this.fedRecently(e.addr, t) && edgeNs < this.endOf(lat, e)) continue;
      // The newest cell of a tile is one cell tall, so a re-ask inside that period cannot come back
      // with a row the copy in hand does not already have. This is the cadence, and it scales itself
      // with the zoom: 1 s at level 0, 32 s five levels out.
      if (t < (this.refreshedAt.get(key) ?? 0) + tCellNs(lat, e.addr.levelT) / 1e6) continue;
      if (edgeNs >= this.endOf(lat, e)) this.stats.edgeRefreshCompletions++;
      const lane = laneOf(e.addr);
      const q = this.refreshLanes.get(lane);
      if (q) q.push(e.addr); else this.refreshLanes.set(lane, [e.addr]);
      this.refreshQueued.add(key);
      n++;
    }
    if (n || ahead) this.pump();
    return n;
  }

  /**
   * **Ask for the row a following edge is about to enter, before it enters it** (T-890). Returns
   * how many reads it queued.
   *
   * # The defect
   *
   * A following pane's tiles are addressed from its box, and its box ends at the edge — so the row
   * the edge is about to enter is not in any box until the edge is already inside it. It was then
   * an ordinary cold miss, joining the queue beside the parent pins (which the LIFO order serves
   * first, by design) and whatever else the view wanted, and on a busy box it was measured taking
   * **7.8 s alone** and over 10 s in a gate. A pane whose span is shorter than a tile — 2.8 s
   * against a 10.24 s level-0 tile in the observed run — has nothing else in hand once its old row
   * scrolls out, so for all of that time it drew *nothing* at its live edge: `0 tiles · 2 coarse
   * stand-ins · 2 drew nothing`, the stand-ins being parent pins that no lane keeps fresh. The
   * live edge was gated on a tile fetch — the thing the product forbids.
   *
   * # The rule
   *
   * For every resident tile a following viewport is DRAWING (its own level, [[drawnBy]]) that
   * holds the edge now, when the edge is within the lead of that tile's end, the same column's next
   * row is queued — once per address ([[aheadAsked]]). It then arrives before the edge does, and the
   * refresh lane revalidates it like any other live tile the moment a box reaches it, so the rows
   * already in hand stay drawn and the new ones follow at the lane's own cadence, with no cold
   * fetch in between. Nothing here decides what a cell shows: an answer for rows not yet recorded
   * reaches its own horizon ([[TileData.asOfNs]]) and the renderer draws nothing past it.
   *
   * Only a column already in hand is extended, which is what keeps this from asking for places the
   * coverage survey answers without a fetch (T-580): the renderer asked for this column's current
   * row, so it is observed spectrum.
   */
  private lookAhead(lat: Lattice, edgeNs: number, following: readonly Viewport[]): number {
    const next = new Set<string>();
    const want: TileAddr[] = [];
    const leadNs = AHEAD_SERVICES * this.serverMs * 1e6;
    // **Every column the renderer has ASKED for, not only the ones that answered** (T-893). A
    // full-width pane is six columns, and on a slow route some of the current row is still queued or
    // in flight when the edge nears its end; extending only resident columns left exactly those with
    // no next row. Queued and in-flight addresses are ones the renderer requested, so the coverage
    // survey did not settle them as never sampled (T-580) — the same guarantee a resident one gives.
    const known: TileAddr[] = [...this.map.values()].map((e) => e.addr);
    for (const k of this.inflight.keys()) { const a = parseKey(k); if (a) known.push(a); }
    for (const a of this.queue) if (!this.ahead.has(keyOf(a))) known.push(a);
    for (const a of known) {
      if (a.scheme !== lat.scheme) continue;
      const ext = extentOf(lat, a);
      if (!(ext.t0Ns <= edgeNs && edgeNs < ext.t1Ns)) continue;
      if (ext.t1Ns - edgeNs > Math.min(ext.t1Ns - ext.t0Ns, leadNs)) continue;
      if (!following.some((v) => this.drawnBy(lat, v, a))) continue;
      const n: TileAddr = { ...a, tIndex: a.tIndex + 1 };
      if (!inLattice(lat, n)) continue;
      const key = keyOf(n);
      if (next.has(key)) continue;
      next.add(key);
      want.push(n);
    }
    this.ahead = next;
    let queued = 0;
    for (const n of want) {
      const key = keyOf(n);
      // Kept fresh in the LRU while it waits for the edge; never re-asked once asked.
      if (this.map.has(key)) { this.peek(n); continue; }
      if (this.aheadAsked.has(key) || this.inflight.has(key) || this.queued.has(key) || this.terminal.has(key)) continue;
      // At the BOTTOM of the LIFO queue: the next row is wanted soon, a visible miss is wanted now,
      // so this read takes a slot only once nothing on screen is waiting for one. On a busy route the
      // lead is the whole tile, which is what leaves room for that. A full queue drops it first,
      // and the ordinary miss path owns the address at the crossing.
      if (this.queue.length >= this.maxQueue) continue;
      this.queue.unshift(n);
      this.queued.add(key);
      queued++;
    }
    return queued;
  }

  /** Was rows pushed for `a`'s column within the last few service times? */
  private fedRecently(a: TileAddr, t: number): boolean {
    const at = this.pushedAt.get(columnOf(a));
    return at !== undefined && t - at < FEED_FRESH_MS;
  }

  /**
   * **The columns a FOLLOWING pane draws at its live edge, and where a row subscription for each
   * should start** (T-893) — what [[LiveRowFeeds.want]] is handed every frame.
   *
   * A column qualifies when the renderer has asked for one of its tiles near the edge (resident,
   * in flight or queued): the same T-580 guarantee [[lookAhead]] relies on, so spectrum the coverage
   * survey settles as never sampled opens no socket. Nearest the pane's centre first, so the
   * client's cap ([[MAX_CLIENT_ROW_FEEDS]]) drops the edges of a wide pane, not its middle.
   *
   * The start row is where the rows in hand stop: the edge tile's own horizon when it is resident,
   * else the edge tile's FIRST row — so the whole of the row the edge is in arrives pushed, and a
   * pane that has no answer for it yet draws it anyway ([[applyRows]]).
   */
  liveColumns(lat: Lattice, edgeNs: number, following: readonly Viewport[]): WantedColumn[] {
    if (!Number.isFinite(edgeNs) || !following.length) return [];
    const known: TileAddr[] = [...this.map.values()].map((e) => e.addr);
    for (const k of this.inflight.keys()) { const a = parseKey(k); if (a) known.push(a); }
    known.push(...this.queue);
    const out: { w: WantedColumn; d: number }[] = [];
    const seen = new Set<string>();
    for (const v of following) {
      const mid = (v.box.f0Hz + v.box.f1Hz) / 2;
      for (const a of known) {
        if (a.scheme !== lat.scheme || a.levelF !== v.levelF || a.levelT !== v.levelT) continue;
        const ext = extentOf(lat, a);
        if (!(ext.f1Hz > v.box.f0Hz && ext.f0Hz < v.box.f1Hz)) continue;
        // The tile the edge is in, or the one it has just left (whose successor may not be known yet).
        if (!(ext.t0Ns <= edgeNs && edgeNs < ext.t1Ns + (ext.t1Ns - ext.t0Ns))) continue;
        const col: ColumnAddr = { device: a.device, scheme: a.scheme, levelF: a.levelF, levelT: a.levelT, fIndex: a.fIndex, cells: a.cells };
        const ck = columnOf(a);
        if (seen.has(ck)) continue;
        seen.add(ck);
        const cell = tCellNs(lat, a.levelT), tile = tTileNs(lat, a.levelT);
        const edgeTile: TileAddr = { ...a, tIndex: Math.floor(edgeNs / tile) };
        const e = this.map.get(keyOf(edgeTile));
        const start = edgeTile.tIndex * tile;
        const asOf = e && !e.synthetic ? e.data.asOfNs : null;
        const from = asOf !== null && Number.isFinite(asOf) ? Math.min(Math.max(asOf, start), edgeNs) : start;
        out.push({ w: { col, fromRow: Math.floor(from / cell) }, d: Math.abs((ext.f0Hz + ext.f1Hz) / 2 - mid) });
      }
    }
    out.sort((x, y) => x.d - y.d);
    return out.map((x) => x.w);
  }

  /**
   * **File a block pushed by `/ws/tiles/rows` and put it on screen** (T-893).
   *
   * A resident copy of the tile gets the rows written into it past its own horizon, and its
   * horizon ([[TileData.asOfNs]]) is carried forward through every row now contiguous with it — so
   * the renderer's one rule (draw a copy only as far as its evidence reaches) draws the new rows the
   * frame after they arrive. A tile not yet in hand whose rows have arrived from its FIRST row on is
   * built from them ([[TileEntry.synthetic]]): the row the live edge has just entered is on screen
   * before any tile read for it could be. Every cell is decoded by the tile route's own rule — the
   * block's `coverage` alone decides grey — so nothing here invents a state.
   *
   * An `unobserved` stretch is written only into tiles already in hand, as grey past their horizon;
   * it is never expanded into tiles of its own (it may span billions of rows).
   */
  applyRows(col: ColumnAddr, block: RowBlock | GapBlock): void {
    const lat = this.lat;
    if (!lat || col.scheme !== lat.scheme) return;
    this.pushedAt.set(columnOf({ ...col, tIndex: 0 }), this.now());
    const cells = col.cells;
    if (block.kind === "unobserved") {
      const r0 = block.row0, r1 = block.row0 + block.rows;
      for (const e of [...this.map.values()]) {
        if (columnOf(e.addr) !== columnOf({ ...col, tIndex: 0 })) continue;
        const t0 = e.addr.tIndex * cells;
        const lo = Math.max(r0, t0), hi = Math.min(r1, t0 + cells);
        if (hi <= lo) continue;
        const grey: TileRows = {
          addr: e.addr,
          maxDb: new Float32Array(cells * cells).fill(NaN),
          coverage: new Array<string | null>(cells * cells).fill("unobserved"),
          rowsSeen: new Uint8Array(cells).map((_, y) => (y >= lo - t0 && y < hi - t0 ? 1 : 0)),
          resolution: null,
        };
        this.patchEntry(lat, e, grey);
      }
      return;
    }
    const addrs = this.pushed.apply(col, block);
    this.stats.rowsPushed += block.rows;
    for (const addr of addrs) {
      const rows = this.pushed.get(addr)!;
      const e = this.map.get(keyOf(addr));
      if (e) this.patchEntry(lat, e, rows);
      else if (rows.rowsSeen[0]) this.synthesize(lat, addr, rows);
      // The column has moved on: the rows of tiles the edge left two rows ago are the sealed tile's.
      this.pushed.prune(col, addr.tIndex - 1);
    }
  }

  /** Mark a resident tile as drawn this frame as a stand-in for a missing one ([[standInFrame]]). */
  standIn(e: TileEntry<T>): void { this.standInFrame.set(e.key, this.frame); }

  /**
   * Write `rows` into `e` past its horizon and carry the horizon forward through the contiguous run
   * — the whole of how a pushed row reaches a texture. Returns the entry now in the map.
   */
  private patchEntry(lat: Lattice, e: TileEntry<T>, rows: TileRows): TileEntry<T> {
    const c = e.addr.cells, d = e.data;
    if (d.nf !== c || d.nt !== c) return e;
    const ext = extentOf(lat, e.addr);
    const cell = (ext.t1Ns - ext.t0Ns) / c;
    const h = d.asOfNs;
    // Rows before the horizon are the answer's, which is the authority for them (it is the newer
    // read of a row that may have been provisional when pushed).
    const first = h === null || !Number.isFinite(h) ? 0 : Math.max(0, Math.floor((h - ext.t0Ns) / cell));
    let lo = c, hi = -1;
    for (let y = first; y < c; y++) {
      if (!rows.rowsSeen[y]) continue;
      for (let f = 0; f < c; f++) {
        const i = y * c + f;
        const cov = rows.coverage[i];
        if (cov === null) continue;
        const v = rows.maxDb[i];
        if (cov === "unobserved") {
          // A last-known level the answer carried here is kept: coverage says nobody looked, and the
          // shadow tier is exactly the mark for that (T-520).
          if (d.state[i] !== CELL.SHADOW) { d.state[i] = CELL.UNOBSERVED; d.value[i] = NaN; }
        } else if (cov === "unknown") { d.state[i] = CELL.UNKNOWN; d.value[i] = NaN; }
        else if (Number.isFinite(v)) { d.state[i] = cov === "excluded" ? CELL.EXCLUDED : CELL.OBSERVED; d.value[i] = v; }
        else { d.state[i] = CELL.NO_LEVEL; d.value[i] = NaN; }
      }
      lo = Math.min(lo, y);
      hi = Math.max(hi, y);
    }
    let asOf = h;
    if (h !== null && Number.isFinite(h)) {
      let y = first;
      while (y < c && rows.rowsSeen[y]) y++;
      const reach = Math.min(ext.t1Ns, ext.t0Ns + y * cell);
      if (reach > h) asOf = reach;
    }
    // The tile states what its drawn rows were measured at (T-902): a tile built from pushed rows
    // carries their merged claim outright; an answered tile patched past its horizon keeps the
    // answer's claim unless the pushed rows state a weaker one, which then wins.
    const pushed = hi >= 0 ? rows.resolution : null;
    const tier = !pushed ? d.tier : e.synthetic ? pushed.tier : weakerTier(d.tier, pushed.tier);
    if (hi < 0 && asOf === h) return e;
    let data: TileData = asOf === h && tier === d.tier ? d : { ...d, asOfNs: asOf, tier };
    if (pushed && e.synthetic) data = { ...data, ...claimOf(pushed, c) };
    let tex = e.tex;
    if (hi >= 0) {
      if (this.tex.patch) this.tex.patch(tex, data, lo, hi - lo + 1);
      else { const old = tex; tex = this.tex.upload(data); this.tex.destroy(old); this.stats.uploads++; }
    }
    const next: TileEntry<T> = { ...e, data, tex };
    this.map.set(e.key, next);
    return next;
  }

  /** Build a tile from pushed rows alone ([[TileEntry.synthetic]]). */
  private synthesize(lat: Lattice, addr: TileAddr, rows: TileRows): void {
    const c = addr.cells, n = c * c;
    const ext = extentOf(lat, addr);
    // **What the pushed rows themselves say they were measured at** (T-902) — the route states it on
    // every block, by the tile route's own rule. Never borrowed from the tile below (T-893's first
    // cut did that, and the pane then stated a level it was not drawn at), and never defaulted:
    // rows with no stated claim say `unknown`.
    const data: TileData = {
      addr, key: keyOf(addr), nf: c, nt: c, t1Ns: ext.t1Ns, asOfNs: ext.t0Ns,
      value: new Float32Array(n).fill(NaN), state: new Uint8Array(n).fill(CELL.NO_LEVEL),
      ...claimOf(rows.resolution, c),
      rangeDb: null, bytes: n * BYTES_PER_CELL,
      serverInFlightLimit: null, serverInFlightShare: null,
      // Pushed rows carry no shadow: a synthesized tile is live rows and nothing else (T-916).
      shadowSource: null,
    };
    const tex = this.tex.upload(data);
    this.stats.uploads++;
    this.stats.rowTilesSynthesized++;
    const e: TileEntry<T> = {
      key: data.key, addr, data, tex, lastUsed: ++this.clock, pinnedFrame: this.frame,
      // Never asked for, so never fresh: the refresh lane fetches the real answer for it.
      edgeAtFetchNs: Number.NEGATIVE_INFINITY, synthetic: true,
    };
    this.map.set(data.key, e);
    this.bytes += data.bytes;
    this.patchEntry(lat, e, rows);
    this.evict();
  }

  /**
   * **Where the route says this tile's span ends, in ns.**
   *
   * The answer's own `extent.t1_s` wherever it stated one, and otherwise the same number computed
   * from the address — which is where the route computes it from too, so the fallback is exact
   * rather than a guess. The answer is preferred because the answer is the party that decided it,
   * and because a client that only ever trusts its own arithmetic cannot notice the route disagreeing.
   */
  private endOf(lat: Lattice, e: TileEntry<T>): number {
    return e.data.t1Ns ?? extentOf(lat, e.addr).t1Ns;
  }

  /**
   * **Is this resident copy behind the data that now exists for it?** (T-495.)
   *
   * This is the question the renderer needs, and [[acquire]]'s residency answers a different one.
   * The rule is one line — *a copy is fresh when the edge it was asked at already reached the end of
   * what it could hold* — and everything below is why the previous rule, "is the live edge still
   * inside this tile", is not that question.
   *
   * ## The defect, measured
   *
   * T-460 fixed the **growing** edge: a live tile was fetched once and frozen, so a following pane
   * redrew the same rows for the 256 s it took to scroll into a new address. The fix made the tile
   * eligible for revalidation *while the edge was inside it* — [[atEdge]]. That is sound for as long
   * as the pane is looking at it, and it has a hole one axis over, which is what the user found:
   *
   * > *retune; pan RIGHT so the new tile is in view, staying in Live; pan LEFT so it goes OFF-SCREEN;
   * > pan RIGHT so it is back. The rows that arrived while it was off screen are grey forever.*
   *
   * Driven through this cache with a 1 s cell and an 8 s tile, thirty frames away and forty back
   * (`ui/test/surface-cache.test.ts`, "a live tile panned off screen and back"):
   *
   * ```
   *                                    fetches of the tile under test
   *                                    on screen   while away   AFTER RE-ENTRY
   * the edge is still inside it on return   3            0            2     <- T-460's case, fine
   * the edge LEFT it while it was away      3            0            0     <- grey forever
   * ```
   *
   * Nothing about re-entry was broken. **The tile simply stopped being eligible while nobody was
   * looking**, and eligibility never came back: `atEdge` goes false the instant the edge crosses the
   * tile's own end, and there is no other path in this file that can ask for a resident key. The
   * copy in hand held rows to wherever the edge was at its last on-screen refresh, and the rows
   * between there and the tile's end were served — honestly, at the time — as `unobserved`. Measured
   * against a real server, a 32 s live tile read 2.5 s in answers three rows `observed` and
   * **twenty-nine `unobserved`**: THE grey. It is true when it is served and it is a lie one second
   * later, and only never asking again makes the lie permanent.
   *
   * The tile above it in time is a *different address*, so it is an ordinary miss on return and
   * arrives complete — which is exactly the shape the user described: present, a band of grey,
   * present.
   *
   * ## Why this predicate, and why it cannot become a poll
   *
   * `edgeAtFetchNs` rises with every fetch and `min(edge, end)` is capped by the tile's own end, so
   * the test is **monotone and terminating**: once a copy is taken at an edge past the tile's end it
   * is fresh for the rest of the session and can never be asked for again. That is the sealed /
   * unsealed distinction the ticket demands, read from the route's stated extent ([[endOf]]) rather
   * than from the address or from how long ago the copy was taken. In steady state it costs **one**
   * extra request per tile per lifetime: the completing re-ask after the edge leaves.
   *
   * **All six of the properties above this one still hold**, because this changes *which* resident
   * tiles are eligible and nothing else. Only the live-edge address (this is that address, later);
   * only for a FOLLOWING viewport (the `drawnBy` test below is untouched); only at the level it is
   * drawn at; never more often than one `tCellNs(level_t)`; one revalidation in flight, into a slot
   * the ordinary queue could not use; per-level lanes on their own measured clocks. A re-entering
   * tile takes the ordinary refresh lane, which is why it cannot starve a visible fetch:
   * [[pumpRefresh]] runs at the end of [[pump]], after the queue has had first refusal on every slot.
   *
   * A **frozen** pane's tiles are still out of scope, because the caller only offers following
   * viewports — the same narrowing T-460 chose, and widening it is not needed for this defect.
   */
  private behindTheEdge(lat: Lattice, e: TileEntry<T>, edgeNs: number): boolean {
    if (e.addr.scheme !== lat.scheme) return false;
    return e.edgeAtFetchNs < Math.min(edgeNs, this.endOf(lat, e));
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

  /**
   * **Ask for the tile the view is panning INTO, and only while it is actually panning** (T-538).
   *
   * Call once a frame with the viewports that are **frozen** — the ones a gesture moves. The live
   * edge's own advance belongs to [[refreshEdge]], which the caller feeds the *following* viewports;
   * splitting them that way is what keeps a pane that is merely scrolling with the record out of a
   * lane whose whole subject is user motion, and it is why the minimap (which follows whatever the
   * panes do) contributes nothing here. See the file header for the two mechanisms that reverted
   * T-471 and how this shape forecloses each.
   *
   * Returns how many reads it issued: **0 or 1**. It queues nothing, so there is no state left
   * behind for a later frame to drain.
   */
  prefetchAhead(lat: Lattice, viewports: readonly MovingViewport[]): number {
    const candidates: TileAddr[] = [];
    const seen = new Set<string>();
    for (const v of viewports) {
      seen.add(v.id);
      const l = v.lat ?? lat;
      const was = this.lastBox.get(v.id);
      this.lastBox.set(v.id, { box: v.box, levelF: v.levelF, levelT: v.levelT, scheme: l.scheme });
      // Nothing to difference yet, or the pane changed what it is looking at rather than where: a
      // zoom (either axis, or the tier) replaces the working set wholesale and the ordinary queue is
      // already enumerating it, so predicting a *direction* from it would be predicting noise.
      if (!was || was.scheme !== l.scheme || was.levelF !== v.levelF || was.levelT !== v.levelT) continue;
      if (!sameSize(v.box.f1Hz - v.box.f0Hz, was.box.f1Hz - was.box.f0Hz)) continue;
      if (!sameSize(v.box.t1Ns - v.box.t0Ns, was.box.t1Ns - was.box.t0Ns)) continue;
      // **Moved means moved by at least one cell.** The level is chosen so a cell is about a pixel
      // ([[levelForHzPerPx]]), so below this nothing on screen moved and there is no gesture to
      // extrapolate — and no float wobble in an animated box can manufacture one.
      const df = v.box.f0Hz - was.box.f0Hz, dt = v.box.t0Ns - was.box.t0Ns;
      const movedF = Math.abs(df) >= fCellHz(l, v.levelF), movedT = Math.abs(dt) >= tCellNs(l, v.levelT);
      if (!movedF && !movedT) continue;
      const sf = movedF ? Math.sign(df) * fTileHz(l, v.levelF) : 0;
      const st = movedT ? Math.sign(dt) * tTileNs(l, v.levelT) : 0;
      const ahead: Box = {
        f0Hz: v.box.f0Hz + sf, f1Hz: v.box.f1Hz + sf,
        t0Ns: v.box.t0Ns + st, t1Ns: v.box.t1Ns + st,
      };
      const inner = new Set(tilesFor(l, v.box, v.levelF, v.levelT).map(keyOf));
      for (const a of tilesFor(l, ahead, v.levelF, v.levelT)) {
        if (!inner.has(keyOf(a))) candidates.push(a);
      }
    }
    for (const id of [...this.lastBox.keys()]) if (!seen.has(id)) this.lastBox.delete(id);
    if (!candidates.length) return 0;
    if (!this.maySpeculate()) return 0;
    for (const addr of candidates) {
      const key = keyOf(addr);
      if (this.speculated.has(key) || this.map.has(key) || this.inflight.has(key) ||
          this.queued.has(key) || this.terminal.has(key)) continue;
      // A guess over spectrum the survey settles as never sampled is not a guess worth a slot: the
      // answer is already known (T-905). Not remembered as speculated, so a later survey that says
      // the band WAS sampled leaves it askable.
      if (this.settled?.(addr)) continue;
      this.speculated.add(key);
      while (this.speculated.size > SPECULATED_MEMORY) {
        this.speculated.delete(this.speculated.values().next().value as string);
      }
      this.speculating.add(key);
      this.stats.speculativeIssued++;
      this.issue(addr, -1);
      return 1; // one at a time, ever
    }
    return 0;
  }

  /**
   * **May this client spend a slot on a guess right now?** Every clause is a foreclosure of one of
   * the two mechanisms that reverted T-471, so none of them is a tuning knob.
   *
   *  - [[idle]]: nothing in flight, nothing queued, no refresh outstanding, nothing abandoned. This
   *    is the precondition that makes "never abort" affordable — the one read this lane starts is
   *    the only thing on the budget, so letting it finish costs nobody anything, and there is by
   *    definition no visible miss it could be taking a slot from.
   *  - `limit >= ceiling`: **the CAP, not the budget.** While AIMD is backed off at all, speculation
   *    is silent. T-471's guard asked `inflight < effectiveLimit`, which is still true at a halved
   *    cap, so it kept taking the one remaining slot for ever and the cap never climbed back.
   *  - the `busyUntil` / `silentUntil` / `silences` gates, for the same reason [[pump]] has them: a
   *    route that has just refused us, or that is not answering at all, is not owed a guess.
   */
  private maySpeculate(): boolean {
    // The clock half of AIMD recovery, so an idle client's cap is current before it is read (T-539).
    this.recoverElapsed();
    const t = this.now();
    return this.speculating.size === 0 && this.silences === 0 &&
      t >= this.busyUntil && t >= this.silentUntil &&
      this.limit >= this.ceiling && this.idle;
  }

  /** Drop one tile so the growing edge can rewrite it (T-439's live tiles are not immutable). */
  invalidate(addr: TileAddr): boolean {
    const key = keyOf(addr);
    const e = this.map.get(key);
    if (!e) return false;
    this.tex.destroy(e.tex);
    this.map.delete(key);
    this.refreshedAt.delete(key);
    this.lastGoodAt.delete(key);
    this.staleScope.delete(key);
    this.speculativeResident.delete(key);
    // A retune drops the look-ahead copy too; the next row may be asked for again.
    this.aheadAsked.delete(key);
    this.pushed.drop(addr);
    this.standInFrame.delete(key);
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
    this.refreshLanes.clear();
    this.refreshPassLeft.clear();
    this.refreshNextIssue.clear();
    this.refreshCosts.clear();
    this.refreshingLane = null;
    this.refreshQueued.clear();
    this.refreshedAt.clear();
    this.lastGoodAt.clear();
    this.staleScope.clear();
    this.terminal.clear();
    this.silences = 0;
    this.silentUntil = 0;
    this.silenced.clear();
    this.lastBox.clear();
    this.speculating.clear();
    this.speculated.clear();
    this.speculativeResident.clear();
    this.ahead.clear();
    this.aheadAsked.clear();
    this.pushed = new RowAccumulator();
    this.pushedAt.clear();
    this.standInFrame.clear();
    this.standInQueued.clear();
    this.lanes.clear();
  }

  /**
   * How many requests may be outstanding **right now**: the AIMD cap, or **one** while the route is
   * silent (T-499).
   *
   * While the gate is armed the client is probing, not working, and a probe is one request. Four
   * would learn exactly the same thing at four times the cost — and four times the console noise the
   * user reported.
   */
  private get effectiveLimit(): number {
    return this.silences > 0 ? 1 : this.limit;
  }

  /** Whether this client is waiting out a silent route (T-499) — for a readout, and for the tests
   * that assert the wire goes quiet rather than merely slower. */
  get silent(): boolean { return this.silences > 0; }

  private pump(): void {
    // **Before the gates, because it is the one thing that must happen when they hold** (T-539):
    // a cap that only falls is what a frozen view was left with. This sends nothing; see
    // [[recoverElapsed]].
    this.recoverElapsed();
    if (this.now() < this.busyUntil) return;
    // **Nothing at all while the silence gate is armed** (T-499). The queue keeps filling — it is
    // deduplicated and bounded — so recovery is immediate on the frame after the gate opens.
    if (this.now() < this.silentUntil) return;
    // The budget is what the ROUTE has out on this client's behalf — the requests being waited on
    // *and* the ones walked away from, which it is still producing. See [[abandonedSlots]].
    while (this.inflight.size + this.abandonedSlots < this.effectiveLimit && this.queue.length) {
      const next = this.nextAddr();
      if (!next) break; // every viewport is at its share; the rest of the queue waits
      const { addr, owner } = next;
      const key = keyOf(addr);
      this.queued.delete(key);
      if (this.map.has(key) || this.inflight.has(key)) continue;
      // Queued before the survey settled it (or by a lane that does not read the survey, like the
      // next-row look-ahead): dropped, never issued (T-905). The renderer re-asks every frame, so a
      // place the next survey calls sampled is queued again then.
      if (this.settled?.(addr)) { this.stats.cancelled++; continue; }
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
    // **At most one revalidation in flight, ever.** It is what makes the duty limit below a bound on
    // the lanes rather than on each of their members, and it is why a cadence can be set from the
    // one completion rather than from a running estimate.
    if (this.refreshing.size) return;
    const t = this.now();
    if (t < this.busyUntil) return;
    // While the gate is armed, nothing — and once it opens, this lane is allowed to *be* the probe
    // (T-499). It has to be: with every tile resident the ordinary queue is empty, so gating the
    // refresh on `silences > 0` would mean a client that never touches the route again until the
    // user moves a pane. The recovery has to be able to happen with nobody touching anything.
    if (t < this.silentUntil) return;
    if (this.inflight.size + this.abandonedSlots >= this.effectiveLimit) return;
    const picked = this.nextRefresh(t);
    if (!picked) return;
    const { lane, addr } = picked;
    const key = keyOf(addr);
    this.refreshQueued.delete(key);
    // Evicted, retuned away, or otherwise no longer in hand while it waited: there is nothing to
    // revalidate, and the ordinary miss path owns the address now.
    if (!this.map.has(key)) return;
    // A stand-in whose missing tile has since arrived is no longer on screen: nothing to refresh.
    if (this.standInQueued.delete(key)) {
      if ((this.standInFrame.get(key) ?? -2) < this.frame - 1) return;
      this.refreshedAsStandIn.add(key);
    }
    this.refreshing.add(key);
    this.refreshingLane = lane;
    this.refreshedAt.set(key, t);
    this.stats.edgeRefreshes++;
    this.issue(addr, -1);
  }

  /**
   * The next revalidation to issue: **round-robin over the lanes that are due**, so a cheap lane is
   * never blocked behind an expensive one (T-490).
   *
   * Two separate things had to change together and neither works alone. Round-robin alone would
   * still stall the live edge, because one global `refreshNextIssue` set from a 725 ms minimap read
   * gates every lane for 2.2 s. A per-lane gate alone would still stall it, because a single FIFO
   * hands the one in-flight slot to whatever was queued first. Together they give each cost class a
   * duty cycle of `1/REFRESH_DUTY` of **its own** measured service time.
   *
   * The cursor advances per *pick*, not per lane visited, so lanes take turns rather than the
   * lowest-keyed one being tried first forever.
   */
  private nextRefresh(t: number): { lane: string; addr: TileAddr } | null {
    const lanes = [...this.refreshLanes.keys()].sort();
    for (let i = 0; i < lanes.length; i++) {
      const lane = lanes[(this.refreshCursor + i) % lanes.length];
      const q = this.refreshLanes.get(lane)!;
      if (!q.length) {
        this.refreshLanes.delete(lane);
        this.refreshPassLeft.delete(lane);
        continue;
      }
      // **The duty gate is charged to a PASS over the live edge, not to each tile on it** (T-532).
      //
      // The thing being kept fresh is the EDGE. How many tiles it is cut into is an accident of the
      // addressing, and it changed under this code: T-501's fidelity floor makes a level-0 tile
      // 150 kHz × 10.3 s where the old floor made it 1.6 MHz × 256 s, so a pane that used to draw
      // its live edge in one tile now draws it in four. Gating each tile separately made the period
      // for any ONE of them `members × REFRESH_DUTY × cost` — measured in a browser on this branch:
      // a mean 1.5 s and a worst 4.4 s between re-asks of the same live address, against a server
      // whose own answer is never more than 90 ms behind the newest recorded row. The lag was a
      // function of the tile size, which is exactly what a refresh rule must not be.
      //
      // So: once a pass starts, its members go out back to back — still one in flight, still last
      // refusal on the slot — and the gate is armed when the last of them lands ([[issue]]'s
      // `done`). The period becomes `(members + REFRESH_DUTY - 1) × cost`, which is **unchanged at
      // one member** (the old floor's case, and every unit test's) and stops growing with the tile
      // count. The lane's share of the route is then `members / (members + REFRESH_DUTY - 1)` of
      // its ONE in-flight slot, rising to at most that whole slot — and one slot of the route's
      // `limit` (four) is exactly what [[REFRESH_DUTY]] says the lane may have. **The stated bound
      // is now the bound the code enforces**; the per-tile gate spent only a quarter of it, which
      // is why the edge could fall a second and a half behind a route answering in 90 ms.
      const mid = (this.refreshPassLeft.get(lane) ?? 0) > 0;
      if (!mid && t < (this.refreshNextIssue.get(lane) ?? 0)) continue;
      // A new pass is exactly what is queued for this lane right now. Tiles queued after it starts
      // wait for the next one, so a lane that re-queues faster than it drains cannot hold the slot
      // forever by never letting the pass end.
      const left = mid ? this.refreshPassLeft.get(lane)! : q.length;
      this.refreshPassLeft.set(lane, Math.min(left, q.length) - 1);
      this.refreshCursor = (this.refreshCursor + i + 1) % lanes.length;
      return { lane, addr: q.shift()! };
    }
    return null;
  }

  /**
   * **What one revalidation in this lane costs the route: the MINIMUM of its recent completions,
   * not the last one** (T-491).
   *
   * The duty limit is `REFRESH_DUTY x` this number, so this is the only input to how often the live
   * edge is re-asked, and getting it from a single sample is what made the opening seconds of a
   * session a lottery. A completion's wall time is **production plus however long another tenant
   * held the history lock**, and the route serialises tile reads on that lock — so a sample taken
   * while a neighbour is reading is a measurement of the neighbour.
   *
   * Measured in a browser, same address, same server, one run:
   *
   * ```
   * 4628  9/0  miss     1303 ms   \
   * 4707  9/0  miss     1369 ms    |  the minimap's INITIAL FILL, 15-19 ordinary misses
   * 5380  9/0  miss      673 ms    |  at 2 097 152 source cells and 37 lock holds each
   * 5434  0/0  REFRESH   844 ms   <-  the live lane's sample, taken inside that
   * 5465  9/0  miss      837 ms   /
   * ...
   * 8477  0/0  REFRESH   133 ms   \
   * 8958  0/0  REFRESH    80 ms    |  the SAME address once the fill is done
   * 9282  0/0  REFRESH    81 ms   /
   * ```
   *
   * The lane's work did not change; the 8x is the minimap. `(REFRESH_DUTY - 1) x 844 ms` then
   * bought **2.5 s of silence** for a tile that costs 80 ms, and the e2e's first sample — taken at
   * `firstFill + 8 s` — landed inside that hole in **2 of 10 runs**. This is T-490's own finding
   * surviving as a *sample* rather than as a mean: *"a mean that folds in the minimap's coarse read
   * charges the live edge for a tile it is not"*, and one contended sample charges it just as
   * wrongly. (It is not a wire effect: the route's own `cost.build_ms` was **807 ms** against the
   * client's 844 ms, so the time was spent inside `build`, waiting on the history mutex.)
   *
   * The minimum over a window is the standard estimator of uncontended service time, and the
   * **window is the route's own in-flight cap** rather than a tuned number: that is how many readers
   * the route admits at once, so it is the most tenants that can be in this lane's way, and if any
   * of the last `ceiling` completions ran with a gap in that traffic, the estimate is the lane's
   * real cost. When the cap is 1 the window is 1 — correctly, because a route that admits one reader
   * has no contention to reject.
   *
   * **The duty bound is not weakened, it is aimed.** It still says "at most `1/REFRESH_DUTY` of what
   * this lane costs the route", and a lane that is *genuinely* expensive still gets rarer on its own,
   * because a sustained slowdown fills the window and raises the minimum within `ceiling`
   * completions. What it no longer does is charge the live edge for someone else's tile.
   */
  private refreshCostOf(lane: string, spent: number): number {
    const hist = this.refreshCosts.get(lane) ?? [];
    hist.push(spent);
    while (hist.length > Math.max(1, this.ceiling)) hist.shift();
    this.refreshCosts.set(lane, hist);
    let est = Infinity;
    for (const v of hist) est = Math.min(est, v);
    return est;
  }

  /** Start one request for `addr`, on the budget. The single place a fetch begins — an ordinary
   * miss and a live-edge revalidation differ only in what [[insert]] does with the answer. */
  private issue(addr: TileAddr, owner: number): void {
    const key = keyOf(addr);
    const ctrl = typeof AbortController === "function" ? new AbortController() : null;
    const started = this.now();
    // **The edge as it is NOW, not as it will be when the answer lands** (T-495): the answer is
    // built somewhere between the two, so this under-states what the copy holds, and under-stating
    // costs a request while over-stating leaves a permanent gap. See [[TileEntry.edgeAtFetchNs]].
    const edgeAtFetchNs = this.edgeNs;
    // Asking is the opposite of quiet (T-539), and so is being answered — `done` stamps it again,
    // so a tile that took five seconds does not hand back five seconds of credit when it lands.
    this.lastWireAt = started;
    this.inflight.set(key, { ctrl, startedAt: started, owner });
    if (this.evicted.has(key)) this.stats.refetchAfterEvict++;
    // The request is off the in-flight list BEFORE anything is re-queued, or `schedule` would see
    // its own request still outstanding and silently drop the retry.
    const done = (wanted: boolean) => {
      let requeue = wanted;
      this.lastWireAt = Math.max(this.lastWireAt, this.now());
      this.inflight.delete(key);
      // The lane tag belongs to the request, not to the place: a re-ask from the ordinary miss path
      // must not inherit the stand-in lane it once rode in.
      this.lanes.delete(key);
      if (this.refreshing.delete(key)) {
        // **The lane's cadence is a share of the LANE'S OWN cost** — measured on its own requests
        // ([[refreshCostOf]]), which since T-491 is the minimum of its last few rather than the last
        // one alone, because one sample taken while a neighbour holds the history lock measures the
        // neighbour.
        //
        // It was `REFRESH_DUTY x serverMs`, and a browser run showed why that is the wrong number:
        // `serverMs` folds in every completion, including the minimap's coarse read, which T-450
        // measured at **5.2 s** and which `MAX_SERVER_MS` clamps at 6 s — so one unrelated map tile
        // set the live edge's refresh interval to 24 s and the newest quarter-minute of a 17 s
        // window went unasked-for. A fine live-edge tile is not that tile and must not be charged
        // for it. Waiting (REFRESH_DUTY - 1) further service times after it lands makes the lane's
        // duty cycle 1/REFRESH_DUTY of what this lane has been observed to cost, whatever that is.
        const spent = Math.max(MIN_RESIDUAL_MS, this.now() - started);
        // …and charged to the lane that spent it (T-490). One `refreshNextIssue` for every
        // following viewport is the same mistake as one `serverMs` for every tile, one level up:
        // the minimap is a following viewport too, so a 725 ms coarse revalidation gated the live
        // pane's 183 ms one for 2.2 s and the newest rows went unasked-for. Measured: the pane's
        // own refresh count fell 68 -> 14 over the same 30 s the moment the minimap's address
        // became servable.
        const lane = this.refreshingLane ?? laneOf(addr);
        this.refreshingLane = null;
        const cost = this.refreshCostOf(lane, spent);
        // **Armed when the PASS ends, not when this tile lands** (T-532 — see [[nextRefresh]]).
        // Mid-pass the next member goes straight out, so the edge is walked at the lane's own
        // service time; the wait is paid once per walk, whatever the edge was cut into.
        if ((this.refreshPassLeft.get(lane) ?? 0) <= 0) {
          this.refreshPassLeft.delete(lane);
          this.refreshNextIssue.set(lane, this.now() + (REFRESH_DUTY - 1) * cost);
        }
      }
      // **A guess is never re-driven** (T-538). A speculative read that failed, was refused or was
      // overtaken by a retune has no one waiting for it; the ordinary miss path owns the address the
      // moment the user actually pans onto it. Requeueing here is how a lane becomes a retry loop.
      if (this.speculating.delete(key)) {
        if (this.map.has(key)) this.speculativeResident.add(key);
        requeue = false;
      }
      if (requeue) this.schedule(addr);
      this.pump();
    };
    // A live-edge revalidation (`owner === -1`, [[refreshEdge]]) is asked for ON ITS OWN: the lane's
    // cadence is a share of what ITS request costs, and a source that coalesces requests (T-573's
    // batch) would otherwise charge it — and hold its rows — for every cold tile it rode beside.
    const lane = this.lanes.get(key);
    void this.source(addr, ctrl?.signal, { solo: owner === -1, lane }).then(
      (data) => {
        this.observe(started);
        this.succeeded();
        // A tile the retune overtook is requeued, not kept: `insert` says which happened, and the
        // requeue has to run in `done`, after the key leaves the in-flight map, or `schedule`
        // would see this very request still outstanding and silently drop the retry.
        let requeue = false;
        try {
          this.adoptStatedCaps(data);
          requeue = !this.insert(addr, data, edgeAtFetchNs);
        } catch { this.stats.failures++; }
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
      // A look-ahead read (T-890) is an orphan by construction; it rides in a batch like any other
      // ordinary miss rather than alone, which is reserved for the revalidation lane.
      if (orphan) {
        this.queue.splice(i, 1);
        const key = keyOf(addr);
        if (!this.ahead.has(key)) return { addr, owner: -1 };
        // Asked means ISSUED: one that waited in the queue and was dropped may be queued again.
        this.aheadAsked.add(key);
        this.stats.aheadIssued++;
        while (this.aheadAsked.size > SPECULATED_MEMORY) {
          this.aheadAsked.delete(this.aheadAsked.values().next().value as string);
        }
        return { addr, owner: AHEAD_OWNER };
      }
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
    // The route answered, so it is not silent. One completion clears the whole ladder: the gate
    // exists to stop asking a server that is not there, and this one demonstrably is (T-499).
    this.silences = 0;
    this.silentUntil = 0;
    this.silenced.clear();
    if (this.limit >= this.ceiling) { this.goodRuns = 0; return; }
    if (++this.goodRuns >= RECOVER_AFTER) { this.limit++; this.goodRuns = 0; }
  }

  /** How long the wire must stay quiet to buy one slot back, ms. See [[RECOVER_QUIET]]. */
  private quietMs(): number { return RECOVER_QUIET * this.serverMs; }

  /** Whether this client wants nothing from the route right now — the precondition for the
   * elapsed-quiet recovery, and the whole reason that recovery is safe. See [[recoverElapsed]]. */
  private get idle(): boolean {
    return this.inflight.size === 0 && this.queue.length === 0
      && this.refreshing.size === 0 && this.refreshDepth === 0
      && this.abandonedSlots === 0;
  }

  /**
   * Raise the cap for every whole quiet window this client has spent wanting nothing (T-539).
   *
   * **It issues nothing.** The cap is a bound, not a schedule, so this is arithmetic on the clock —
   * which is exactly why the recovery is allowed to happen with the view frozen and the wire
   * silent. It is called from [[pump]], which the render loop reaches through `endFrame` on every
   * frame whether or not anything is wanted, and from [[inFlightLimit]] so a readout can never show
   * a cap the policy has already let go of.
   *
   * **Idle is a precondition, not an implementation detail.** A client with work in hand has
   * completions to be paid in and [[RECOVER_AFTER]] already pays it; probing upward *while* asking
   * is what turns a working client's AIMD into a faster sawtooth against a route whose other
   * tenants are not going anywhere, and `ui/test`'s two-client guard bounds exactly that — the
   * refusals a converged client takes per run of completions. An **idle** client's raised cap, by
   * contrast, is unobservable to the route: there is nothing to issue with it. It is felt only on the next burst, which is then
   * served exactly as a freshly loaded page would be, and re-halved by a fresh refusal if the route
   * is still full. So the evidence a refusal carries decays only while this client has stopped
   * competing, which is the one regime in which it is genuinely stale.
   *
   * The window is recomputed per slot because [[serverMs]] is a live measurement: a route that got
   * slower while we were quiet stretches the remaining steps rather than being credited at the old
   * rate. The loop is bounded by the ceiling, which is the route's own small number.
   */
  private recoverElapsed(): void {
    if (this.limit >= this.ceiling || !this.idle) return;
    const t = this.now();
    let quiet = this.quietMs();
    while (this.limit < this.ceiling && t - this.lastWireAt >= quiet) {
      this.limit++;
      this.goodRuns = 0;
      this.lastWireAt += quiet;
      quiet = this.quietMs();
    }
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
      // **`limit` is the whole server's budget, not this client's allowance** — every pane and the
      // bootstrap probe draw on the same four slots, and an abandoned read holds one until it
      // finishes. So it is kept as a *ceiling* and the operating cap is halved: the share that
      // belongs to this cache is not something either side knows, it is something backing off
      // finds. Recovery is [[succeeded]]; together they are AIMD.
      //
      // **`share` is different** (T-630): it IS this client's allowance, stated by the route,
      // because the route now divides its slots between the clients that are asking. It replaces
      // the ceiling rather than only lowering it — a share that went *up* because another tab
      // closed is as true as one that went down, and a monotonically-falling ceiling would keep
      // this cache at one slot for the rest of the session over a tab that is long gone (the
      // stuck-at-1 shape T-455 measured). The halving is untouched: T-455 conditions permission on
      // the MECHANISM — a refusal must be SEEN — and a refusal is exactly what this is.
      this.stats.busyRefusals++;
      if (err.limit && err.limit > 0) this.ceiling = Math.min(this.ceiling, err.limit);
      if (err.share && err.share > 0) {
        this.ceiling = Math.max(1, err.share);
        this.shareStatedAt = this.now();
      }
      // **`held` says WHOSE slots refused this read** (T-959), and it is the one reading that is not
      // evidence about contention: at `held >= share` this client is being refused over reads it
      // owns — the ones it is waiting for, and the ones it abandoned that the route is still
      // producing. Halving the cap there is the client punishing itself for its own slow reads, and
      // it is what pinned a page at a cap of 1 while nothing of its own was on the wire (T-932).
      // The cap is already right; what is wrong is the client's belief that those slots are free, so
      // the charge is corrected instead and the backoff below does the waiting. At `held < share`
      // (`held 0` being the plain case) the slots are somebody else's and the multiplicative
      // decrease is exactly right. A server that states no `held` is pre-T-959: it halves, as before.
      const ownReadsRefusedIt = typeof err.held === "number"
        && err.share !== null && err.share > 0 && err.held >= err.share;
      if (typeof err.held === "number") this.reconcileHeld(err.held, false);
      this.limit = ownReadsRefusedIt
        ? Math.min(this.limit, this.ceiling)
        : Math.max(1, Math.min(Math.floor(this.limit / 2), this.ceiling));
      this.goodRuns = 0;
      this.refusals = Math.min(this.refusals + 1, 4);
      // The refusal itself cost the server nothing, but whatever is holding the slots has not
      // finished — so wait longer each time rather than re-asking on the same cadence. **Jittered**
      // (T-1039): every viewport's tiles refused by the same overload would otherwise retry on the
      // exact same tick, re-creating the burst that got them refused.
      this.busyUntil = this.now() + jittered(this.busyBackoffMs * 2 ** (this.refusals - 1), this.random);
      // **And the quiet clock starts when that backoff ends** (T-539): completions are one way to
      // earn a slot back, elapsed quiet is the other, and a frozen view only ever has the second.
      // Quiet means quiet *after* the route has stopped saying it is busy.
      this.lastWireAt = this.busyUntil;
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
      return false;
    }
    // **The server said nothing, so back off the transport** (T-499). Terminal would be wrong — a
    // server that comes back is a changed answer — but so is asking again on the next frame, which
    // is what the render loop does unless something here says when. See [[OFFLINE_BACKOFF_MS]].
    this.stats.silentFailures++;
    this.silenced.add(keyOf(addr));
    this.silences++;
    // **Jittered** (T-1039): a whole batch (T-573) fails together on a dropped connection, so
    // without jitter every address in it would arm the SAME probe instant and the "one probe at a
    // time" half-open shape would still cost a burst the moment the wire returns.
    this.silentUntil = this.now() + jittered(
      Math.min(OFFLINE_MAX_BACKOFF_MS, OFFLINE_BACKOFF_MS * 2 ** (this.silences - 1)), this.random);
    return false;
  }

  /**
   * Adopt the caps an ANSWER states, whatever then happens to its tile.
   *
   * `cost.in_flight_limit` is the same server-wide number the refusal names, so it sets the
   * ceiling — it is not permission to run at it. `cost.in_flight_share` (T-630) is this client's
   * own allowance of that budget and is authoritative in both directions: it is how a tab that is
   * already drawn learns, on its very next answer, that another tab has arrived and half the slots
   * are no longer its to take. The operating cap only ever falls on a seen refusal (T-455); what
   * moves here is the ceiling it recovers toward.
   *
   * **Every answer, not only the ones [[insert]] keeps.** The share is a fact about this client's
   * admission, not about the tile: an answer for a key already resident, or one a retune overtook,
   * states it just as truly. Adopting it only on an upload left the stated share stale for as long as
   * the answers happened to be duplicates — a status line claiming "share 4" with two clients up.
   */
  private adoptStatedCaps(data: TileData): void {
    if (data.serverInFlightLimit && data.serverInFlightLimit > 0) {
      this.ceiling = Math.min(this.ceiling, data.serverInFlightLimit);
    }
    if (data.serverInFlightShare && data.serverInFlightShare > 0) {
      this.ceiling = Math.max(1, data.serverInFlightShare);
      this.shareStatedAt = this.now();
    }
    // **And what this client HOLDS** (T-959): the one number in the answer that is about the route's
    // slots rather than about this tile, and the only correction the abandoned-read charge can get.
    if (typeof data.serverInFlightHeld === "number") {
      this.reconcileHeld(data.serverInFlightHeld, data.serverHoldsThisRead !== false);
    }
    this.limit = Math.min(this.limit, this.ceiling);
  }

  /**
   * **An answer for a window that had not begun when it was asked for is evidence about none of it**
   * (T-890 follow-up). Returns `data` with its horizon pinned to the tile's own start in that case.
   *
   * The route takes `coverage.horizon.as_of_s` from the tune records that overlap the tile's window,
   * clipped to it — so a tile asked for before the live edge reaches it (the look-ahead,
   * [[lookAhead]]) has no overlapping record, answers `as_of_s: null`, and paints every row
   * `unobserved`. `null` is honestly "no record touches this band" for a window in the past; for one
   * in the future it is only "nothing has happened yet", and drawn as served it put THE grey over
   * the newest rows of a following pane — the rows the radio was recording — from the moment the
   * edge entered the tile until the first revalidation landed (canvas-journey: 6-18 % of the
   * live-edge zone grey over a band the server reports fully observed).
   *
   * The client knows exactly one thing the answer does not: the edge it asked at. When that edge
   * had not reached the tile's start, the copy reaches forward exactly as far as that start — the
   * same statement a copy built from pushed rows starts from (T-893's `synthetic`, horizon at its
   * own start). The renderer then draws nothing past it, the row feed carries the horizon forward
   * row by row, and the refresh lane replaces it with an answer that has records. A tile asked for
   * at or after its own start is untouched, as is any answer that states a horizon, and a client
   * that has never been told an edge (a historical view) has nothing to compare and changes nothing.
   */
  private horizonOf(addr: TileAddr, data: TileData, edgeAtFetchNs: number): TileData {
    const lat = this.lat;
    if (!lat || addr.scheme !== lat.scheme || Number.isFinite(data.asOfNs as number) || !Number.isFinite(edgeAtFetchNs)) return data;
    const t0 = extentOf(lat, addr).t0Ns;
    return edgeAtFetchNs <= t0 ? { ...data, asOfNs: t0 } : data;
  }

  /** Take a fetched tile. **False means "ask again"**: the tuning changed while it was in flight. */
  private insert(addr: TileAddr, data: TileData, edgeAtFetchNs: number): boolean {
    const key = keyOf(addr);
    if (this.stale.delete(key)) return false;
    const prev = this.map.get(key);
    // A **live-edge revalidation replaces** the copy in hand (T-460); anything else that arrives for
    // a resident key is a duplicate, and the same tile is never uploaded twice — except a copy built
    // from pushed rows, which any real answer replaces (T-893).
    if (prev && !this.refreshing.has(key) && !prev.synthetic) return true;
    data = this.horizonOf(addr, data, edgeAtFetchNs);
    const tex = this.tex.upload(data);
    this.stats.uploads++;
    // The replaced texture is destroyed and its bytes returned: a refresh that leaked one would turn
    // the growing edge into a memory leak proportional to how long the view is left running.
    if (prev) {
      this.tex.destroy(prev.tex);
      this.bytes -= prev.data.bytes;
      if (!prev.synthetic) this.stats.edgeRefreshApplied++;
    }
    // A tile that has just arrived is pinned for the frame it arrived on: it cost the server 11.4 ms
    // and evicting it before it has been drawn once would spend that twice.
    const entry: TileEntry<T> = { key, addr, data, tex, lastUsed: ++this.clock, pinnedFrame: this.frame, edgeAtFetchNs };
    this.map.set(key, entry);
    this.bytes += data.bytes;
    this.refreshedAt.set(key, this.now());
    this.lastGoodAt.set(key, this.now());
    // **Rows already pushed past this answer's horizon are laid back over it** (T-893). The route's
    // horizon trails the feed, so an answer is usually older at its top than what is on screen, and
    // replacing the copy without this would take the newest rows off the pane until the next push.
    const rows = this.pushed.get(addr);
    if (rows && this.lat && addr.scheme === this.lat.scheme) this.patchEntry(this.lat, entry, rows);
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
      this.lastGoodAt.delete(victim.key);
      this.staleScope.delete(victim.key);
      this.speculativeResident.delete(victim.key);
      this.standInFrame.delete(victim.key);
      this.bytes -= victim.data.bytes;
      this.evicted.add(victim.key);
      this.stats.evictions++;
    }
  }
}

/**
 * **Is asking again capable of changing the answer?** (T-479.)
 *
 * **The question is: did the server answer?** If it answered anything at all, asking again cannot
 * change it. If we never got an answer, it can. That is the whole enumeration, and everything
 * outside it is terminal by default — the inversion this function exists for. The pre-T-479 rule
 * special-cased `503` and let every other outcome fall through to "ask again", so each unenumerated
 * outcome was wrong by default: `BiasTee::Unknown` is not `Off`, in the retry direction.
 *
 *  1. **`TileBusyError`** — the route's `503` — is the one answer that *is* about now. T-454's
 *     backpressure says the history lock is held; waiting and asking again is the whole of the right
 *     response. It is handled by the AIMD branch above with its own backoff and never reaches here.
 *  2. **A failure that carries no answer at all** — socket closed, server restarting, DNS. The
 *     server said nothing, so nothing it said is permanent, and blanking a place for the rest of the
 *     session over a momentary disconnect would be the opposite defect. It is not re-queued either:
 *     the next frame's `acquire` asks, so the retry is paced by the render loop.
 *  3. **A status the ROUTE cannot emit** (T-523) — see [[fromTheRoute]]. A proxy answers in the same
 *     channel the origin does, so a status line is not by itself proof that the *server* answered:
 *     a `502 Bad Gateway` is a proxy saying the origin did not. It is case 2 wearing a status line,
 *     and counting it as an answer left a live-edge tile terminal until a resize re-addressed it.
 *
 * Everything the server *said* is terminal — a status (400, 404, 413, 500), **and a body this client
 * could not read**. The second half was missing, and merging T-467 proved why it matters rather than
 * arguing it: the new coverage encoding made a stale fixture's `200` responses undecodable, and
 * because a `TileDecodeError` carries no HTTP status it fell into case 2 and was re-asked at frame
 * rate — **157 times in 700 ms**, the very storm T-479 exists to stop, wearing different clothes.
 * A 200 whose body does not decode is an answer; it is just not a readable one, and asking again
 * gets the same bytes back.
 *
 * The status is read structurally rather than by `instanceof` so this does not couple to which error
 * class the fetch layer builds for an HTTP failure; `TileDecodeError` is named because it is the
 * exported way `tile.ts` says "the server answered and I could not read it", which no status carries.
 */
function retryable(err: unknown): boolean {
  if (err instanceof TileBusyError) return true;
  if (err instanceof TileDecodeError) return false;
  const status = (err as { status?: unknown } | null)?.status;
  return typeof status !== "number" || !fromTheRoute(status);
}

/**
 * **Could `/api/tiles` itself have said this?** (T-523.)
 *
 * T-479 asked "did the server answer?" and read a status line as proof that it had. It is not:
 * between this client and `hk serve` there is at least one proxy (the user's cloudflared tunnel,
 * and in any deployment a reverse proxy), and **a proxy answers in the same channel the origin
 * does**. A `502 Bad Gateway` is a status line whose entire content is *the origin did not answer*
 * — case 2 of [[retryable]], not case "everything the server said". A place refused that way is
 * never asked for again, so it stays undrawn until a resize changes the tile addresses — the
 * user's "it stalls until I resize it", exactly. `ui/e2e/live-edge` test 3 holds the fix to it,
 * and measures the defect from **the page's own residency readout** rather than from the request
 * log: with this rule reverted, a pane whose zoom-out the proxy answered 502 sits at `0 tiles ·
 * 3 coarse stand-ins · 0 pending` for as long as anyone watches, because a terminal place is never
 * re-asked and an upscaled coarser ancestor is all the renderer has left to draw. (The request log
 * is the wrong instrument here and the test says why: with the view held still, every refused
 * address is one T-460's refresh lane re-picks anyway, terminal or not.)
 *
 * **The rule is over the route's vocabulary, not over a list of proxy codes**, because the proxy
 * set is open (Cloudflare alone adds 520–527 and 530; another deployment invents its own) while the
 * route's is closed and lives in this repo:
 *
 *  - **Every `4xx` is the route's** — a refusal about the request, which asking again cannot change.
 *    A proxy can also send a `4xx`, and that is fine: those say something about the request too.
 *  - **Of the `5xx`, the route emits exactly `500`, `501` and `503`** — `ApiError::new(500)`,
 *    `/api/analyze`'s T-190 `501`, and T-454's backpressure `503` (which never reaches here; it
 *    arrives as [[TileBusyError]] and takes the AIMD branch). `crates/hk-api/src/http.rs` sends no
 *    other 5xx; its `reason()` table names `502`/`504` only so a *proxy's* status can be printed.
 *  - **Every other `5xx` is therefore something between us and the route**, and is silence.
 *
 * **Why the default may now be "ask again" here, when T-479 made it "terminal".** T-479's danger
 * was a *frame-rate* storm: 157 requests in 700 ms for one place. The unenumerated-status branch no
 * longer lands there — since T-499 it lands on the silence ladder, which backs the whole transport
 * off, probes once while armed, and clears on the first answer. So the cost of being wrong in this
 * direction is a paced probe; the cost of being wrong in the other is a region of the canvas that
 * never draws again this session. Those are not symmetric, and the honest default follows the
 * asymmetry.
 */
function fromTheRoute(status: number): boolean {
  return status < 500 || status === 500 || status === 501 || status === 503;
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

/**
 * The abandoned-read charge, as a readout states it (T-959): how many slots this client is charging
 * itself for reads the route is still producing, and **whether that number is the route's word or
 * this client's estimate** — which is the difference between "my own leftover reads are holding the
 * slots" and "another client is". Empty when there is nothing charged and nothing stated.
 */
export function heldText(charged: number, stated: number | null, ageMs: number | null): string {
  if (charged === 0 && stated === null) return "";
  if (stated === null) return `${charged} abandoned charged (estimated — the route has not stated one)`;
  const s = (ageMs ?? 0) / 1000;
  return `${charged} abandoned charged (route held ${stated}, stated ` +
    `${s < 10 ? s.toFixed(1) : Math.round(s).toFixed(0)} s ago)`;
}

/**
 * The share, as a readout states it: **with its age**, because it is the route's word as of the
 * last answer and nothing newer (see [[TileCache.inFlightShareStatedAt]]). `ageMs` null means the
 * route has never stated one, and the number is this client's own starting assumption.
 *
 * `share 2, stated 0.4 s ago` · `share 4, stated 38 s ago` · `share 4, assumed (the route has not stated one)`
 */
export function shareText(share: number, ageMs: number | null): string {
  if (ageMs === null) return `share ${share}, assumed (the route has not stated one)`;
  const s = ageMs / 1000;
  return `share ${share}, stated ${s < 10 ? s.toFixed(1) : Math.round(s).toFixed(0)} s ago`;
}
