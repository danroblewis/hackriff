// **LSR-7: sample→pixel latency and per-row fold cost, MEASURED, never assumed** (T-1048).
//
// The live-rendering invariants this ticket exists to hold the ring to (docs/16, "Live rendering",
// and `./livering.ts`'s own header) are claims about *cost* and *freshness*: "capture-thread cost is
// measured, never assumed" (T-453) and "the live view renders like a classic SDR waterfall". Neither
// claim is checkable from the ring's arithmetic alone — it says where a row lands, not how long it
// took to get there or how stale it is by the time a frame draws it. This module is the two numbers
// that make both checkable, and nothing else: it holds no ring, opens no socket and commands nothing.
//
//  - **fold**: the wall-clock cost of filing one published row into the ring
//    (`LiveRing.push`, timed at its one call site in `app/centre/live-edge.ts`'s `onBinary`) — the
//    client-side half of "per-subscription fold cost per row" (there is one subscription: the
//    spectrum socket `live-edge.ts` already holds, and every row on it is folded into the ring once).
//  - **latency**: **row t vs rAF** — the wall clock at the instant a pane's render pass actually
//    paints a row, minus the wall clock at the instant that row **arrived** over the socket
//    (`live-edge.ts`'s `onBinary`, before this ticket its only reader of "now"). Measured at the draw
//    call (`./surface.ts`'s render loop, the same place every other per-frame quantity on this
//    surface is computed) and **deduplicated per pane** so holding the same newest row across several
//    frames (no new row has arrived) does not manufacture samples out of repeated paints of one row.
//
//    Both ends of this subtraction are `performance.now()` — the ONE clock, never the row's own
//    `tNs`. That is not a stylistic choice: `tNs` is the backend's *capture* time, which is not the
//    wall clock at all once a fixture or a mock scene is involved (`live-edge.ts`'s header, reason 1:
//    "a replay or a time-compressed mock scene runs on a clock of its own, and the fixture that first
//    exposed this sat 3.5 days from wall time"). Subtracting it from `Date.now()` would report that
//    3.5-day offset as "latency" on every replay fixture this project tests against, including the
//    one this ticket's own e2e spec runs on — found the hard way, live, before this comment existed:
//    the first version of this module did exactly that and reported a billion-millisecond latency.
//    Two wall-clock reads of the same client clock, one at arrival and one at paint, are immune to
//    that: whatever clock the row's own timeline runs on, the CLIENT's wait between "the socket
//    delivered it" and "the screen showed it" is a real, small, comparable duration on any source.

/** One rolling window's stats, ms. `n === 0` is the honest "nothing measured yet" — never a guessed 0. */
export interface StatSnapshot {
  readonly n: number;
  readonly meanMs: number;
  readonly p95Ms: number;
  readonly maxMs: number;
}

const EMPTY_STAT: StatSnapshot = { n: 0, meanMs: 0, p95Ms: 0, maxMs: 0 };

/**
 * The most recent `capacity` durations (ms), reporting mean/p95/max over exactly what is retained —
 * never a lifetime average that a long session would smooth an ongoing stall out of.
 */
export class RollingStat {
  private readonly buf: Float64Array;
  private n = 0;
  private next = 0;

  constructor(readonly capacity = 256) {
    this.buf = new Float64Array(Math.max(1, Math.floor(capacity)));
  }

  push(ms: number): void {
    if (!Number.isFinite(ms)) return;
    this.buf[this.next] = ms;
    this.next = (this.next + 1) % this.buf.length;
    this.n = Math.min(this.n + 1, this.buf.length);
  }

  reset(): void {
    this.n = 0;
    this.next = 0;
  }

  snapshot(): StatSnapshot {
    if (this.n === 0) return EMPTY_STAT;
    // `this.buf.slice(0, this.n)` is every retained sample regardless of wrap: while `n < capacity`
    // writes have only ever gone into `[0, n)`; once `n === capacity` every slot is retained.
    const vals = Array.from(this.buf.slice(0, this.n));
    vals.sort((a, b) => a - b);
    let sum = 0;
    for (const v of vals) sum += v;
    const p95 = vals[Math.min(vals.length - 1, Math.ceil(0.95 * vals.length) - 1)];
    return { n: vals.length, meanMs: sum / vals.length, p95Ms: p95, maxMs: vals[vals.length - 1] };
  }
}

/** Both LSR-7 measurements for one page. */
export interface LiveMetricsSnapshot {
  readonly latency: StatSnapshot;
  readonly fold: StatSnapshot;
}

/** The two rolling windows this ticket adds, held together for one `snapshot()`/`reset()`. */
export class LiveMetrics {
  readonly latency = new RollingStat();
  readonly fold = new RollingStat();

  reset(): void {
    this.latency.reset();
    this.fold.reset();
  }

  snapshot(): LiveMetricsSnapshot {
    return { latency: this.latency.snapshot(), fold: this.fold.snapshot() };
  }
}

/**
 * One shared meter for the whole page: `live-edge.ts` records fold cost per row, `surface.ts` records
 * paint latency per pane per frame. A module singleton for the same reason `../app/centre/live-edge.ts`'s
 * `liveRing` is one — the mount that feeds it is mounted exactly once.
 */
export const liveMetrics = new LiveMetrics();

/** Time one call and record its wall-clock cost into `stat`, ms. Returns the call's own result,
 * unchanged — a timer that can alter what it measures is not a timer. */
export function timed<T>(stat: RollingStat, fn: () => T, now: () => number = () => performance.now()): T {
  const t0 = now();
  const out = fn();
  stat.push(now() - t0);
  return out;
}

/**
 * **Row t vs rAF**: how long a row waited between arriving and being painted, ms — one clock, two
 * reads. `paintMs` and `arrivalMs` are both `performance.now()`, taken at the draw call and at the
 * socket's `onBinary` respectively; pure so the arithmetic is tested without a clock, a socket or a
 * GL context.
 */
export function paintLatencyMs(paintMs: number, arrivalMs: number): number {
  return paintMs - arrivalMs;
}

/**
 * **The newest published row's own arrival**, `performance.now()` at the instant `live-edge.ts`'s
 * `onBinary` received it — read by the render pass to compute [[paintLatencyMs]].
 *
 * A single holder, like `live-edge.ts`'s own `liveRow`: there is one spectrum socket, so there is one
 * "the newest row arrived at…" instant, not one per pane. `null` until the first row this session.
 */
export class LastArrival {
  private wallMs: number | null = null;

  set(wallMs: number): void { this.wallMs = wallMs; }

  /**
   * Record "now", reading the clock itself. `live-edge.ts` calls this rather than reading
   * `performance.now()` and passing it to [[set]]: that module is on the CAPTURE clock
   * (T-393/T-386's guard) and must not contain a browser-clock read even for a genuinely
   * wall-clock question — the read happens here, in the one module whose job is the wall clock.
   */
  mark(now: () => number = () => performance.now()): void { this.set(now()); }

  get(): number | null { return this.wallMs; }
}

/** The shared holder: `live-edge.ts` sets it, `surface.ts` reads it. Module singleton for the same
 * reason `liveMetrics` is — the mount that feeds it is mounted exactly once. */
export const lastRowArrival = new LastArrival();

/** The dashboard tile's text: what a person reads, not what a test greps. `"no samples yet"` per
 * window is the honest empty state — never a guessed 0 ms presented as a measurement. */
export function fmtLiveMetrics(s: LiveMetricsSnapshot): string {
  const one = (label: string, w: StatSnapshot) =>
    w.n === 0 ? `${label} no samples yet` : `${label} ${w.meanMs.toFixed(2)} ms mean, ${w.p95Ms.toFixed(2)} ms p95, ${w.maxMs.toFixed(2)} ms max (n=${w.n})`;
  return `${one("fold", s.fold)} · ${one("latency", s.latency)}`;
}
