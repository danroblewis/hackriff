// **A visible tile is never abandoned** (T-1057) — the per-address retry ladder the tile lanes share.
//
// The user's report, 2026-09-25: *"Sometimes there are black bars in the waterfall, representing
// tiles that haven't been loaded yet; sometimes those never load. If a tile fails to load at all it
// should be re-requested. It seems like they are getting abandoned. Left alone long enough, all
// tiles on the screen should load. I don't think we should ever see the black tiles."*
//
// So the invariant, stated once, here, because this file is what enforces it:
//
// > **Every pending VISIBLE address is re-requested, with jittered backoff, until it is served or
// > the route states the place does not exist.** Nothing else ends the asking.
//
// ## Why a ladder PER ADDRESS, when T-499 put the silence backoff on the transport
//
// `tilecache.ts`'s [[OFFLINE_BACKOFF_MS]] comment is emphatic that a backoff belongs to the
// transport and not to the place, and it is right about the outcome it was written for: *"connection
// refused" is not a fact about a tile address*, so thirty viewport tiles each running their own
// ladder against a dead socket would multiply the wire traffic by thirty.
//
// This ladder is for the opposite outcome. Here the route **did** answer — 400 "this tile's level
// cannot be built from the levels below it", 500 "history store poisoned", an unreadable body, a
// batch answer that named no entry for the address — and every one of those is a fact about **that
// place at that moment**, not about the server. Two consequences follow, and they are why the two
// backoffs cannot be the same object:
//
//  1. **One bad place must not gate twenty-nine good ones.** A transport-wide gate for a
//     per-address refusal is what T-460 measured as a deadlock: T-479's permanently-refused minimap
//     addresses held the queue non-empty and the live edge never refreshed once in forty seconds.
//  2. **A per-address refusal is transient far more often than the status can say.** `hk-api`
//     answers `400` both for an address that can never exist (`f_index` outside the addressable
//     spectrum) and for one that merely cannot be folded **yet** (`materialize` failing while the
//     pyramid's lower levels are still filling) — the same status, one permanent, one that resolves
//     itself in seconds. A client that reads the first meaning into both leaves a bar of the
//     waterfall black for the rest of the session, which is exactly what the user saw.
//
// The cost of the ladder is bounded by construction and is the number to check this against: a place
// is only re-asked while something is **drawing** it (the renderer's `acquire` is what re-queues;
// nothing here schedules anything), at most once per interval, and the interval doubles from
// [[RETRY_BASE_MS]] to [[RETRY_MAX_MS]]. Thirty refused places therefore cost thirty requests per
// half-second falling to thirty per thirty seconds — against 30 × 60/s before T-479, and against
// *never again* after it.
//
// ## The jitter, and why it is additive
//
// The same shape T-1035 gave the control transport's ladder (`../controls/backoff.ts`: 500 ms
// doubling to 30 s, reset on the first answer, never terminal) and T-1039 gave this file's own two
// gates: **`base` plus up to `base × RETRY_JITTER` of randomness**. Additive, so jitter can only ever
// make the wait longer than the ladder says, never shorter — a spread that could shorten the wait
// would be a way for a herd to arrive *earlier* than the backoff allows. A whole batch (T-573) fails
// together on one dropped connection and a whole viewport's tiles fail together on one retune, so
// without the spread every address in the set would come back on the identical tick and re-create
// the burst that failed.
//
// Presentation-side only: this decides WHEN this client asks again, and nothing about what an answer
// means.

/** The first wait after a refusal, ms, and the ceiling the ladder doubles up to. The same two
 * numbers as `../controls/backoff.ts`'s `OFFLINE_BASE_MS`/`OFFLINE_MAX_MS` and as this file's
 * sibling silence ladder, for the same reason: one shape, one pair of constants to reason about. */
export const RETRY_BASE_MS = 500, RETRY_MAX_MS = 30_000;

/** How much of the interval the jitter may add, at most. Half: enough to break a synchronised herd
 * apart over the first step, small enough that the ladder is still recognisably the ladder. */
export const RETRY_JITTER = 0.5;

/**
 * How many refused addresses keep their attempt count. Bounded because a session can visit a great
 * many addresses and this must not be a leak; **dropping the oldest entry is safe in the direction
 * that matters** — it forgets how long a place has been failing, so the place is asked again sooner
 * (at [[RETRY_BASE_MS]]) rather than being forgotten as a place that is owed a request.
 */
export const RETRY_MEMORY = 512;

/**
 * The ladder's `n`th wait, jittered: `min(cap, base × 2^(n-1))` plus up to [[RETRY_JITTER]] of it.
 *
 * `rand` is `[0, 1)` and is injected so a test can pin the spread; a source that answers outside that
 * range contributes no jitter rather than a wild delay (a clamp is the one safe reading of a broken
 * `random`, since a negative multiplier would shorten the wait).
 */
export function retryDelayMs(
  attempts: number,
  rand: () => number,
  baseMs = RETRY_BASE_MS,
  capMs = RETRY_MAX_MS,
): number {
  const n = Math.max(1, Math.floor(attempts));
  const step = Math.min(capMs, baseMs * 2 ** Math.min(n - 1, 40));
  const r = rand();
  const spread = Number.isFinite(r) && r >= 0 && r < 1 ? r : 0;
  return step + step * RETRY_JITTER * spread;
}

/** What one refused place is remembering. */
interface Entry {
  /** Consecutive refusals of this place. */
  attempts: number;
  /** The instant it may be asked for again, ms on the injected clock. */
  dueAt: number;
  /** The route's own words for the last refusal — what a readout shows, never interpreted here. */
  why: string;
}

/**
 * **Per-address jittered backoff: a place that failed is asked again, later, for as long as anyone
 * draws it** (T-1057).
 *
 * The ladder issues nothing and schedules nothing. It answers one question — *may this place be
 * asked for now?* — so the only thing that can start a request is still the lane that wants the
 * tile, and a place nobody draws is never re-asked at all. That is what keeps "never abandoned"
 * from becoming "polled forever".
 */
export class RetryLadder {
  private readonly places = new Map<string, Entry>();

  constructor(
    private readonly now: () => number,
    private readonly rand: () => number = Math.random,
    private readonly baseMs: number = RETRY_BASE_MS,
    private readonly capMs: number = RETRY_MAX_MS,
    private readonly memory: number = RETRY_MEMORY,
  ) {}

  /**
   * Record a refusal of `key` and return when it may be asked for again.
   *
   * The attempt count is *consecutive*: [[clear]] on a success is what resets it, so a place that
   * fails, recovers and fails again starts its ladder over rather than inheriting a 30 s wait from
   * an hour ago.
   */
  fail(key: string, why: string): number {
    const e = this.places.get(key);
    const attempts = (e?.attempts ?? 0) + 1;
    const dueAt = this.now() + retryDelayMs(attempts, this.rand, this.baseMs, this.capMs);
    // Re-inserted rather than mutated, so the map's iteration order is "least recently refused
    // first" and the bound below drops the entry that has waited longest since it last failed.
    this.places.delete(key);
    this.places.set(key, { attempts, dueAt, why });
    while (this.places.size > this.memory) {
      const oldest = this.places.keys().next().value as string | undefined;
      if (oldest === undefined) break;
      this.places.delete(oldest);
    }
    return dueAt;
  }

  /** May this place be asked for now? True for a place with no history, which is the common case. */
  ready(key: string): boolean {
    const e = this.places.get(key);
    return e === undefined || this.now() >= e.dueAt;
  }

  /** Is this place on the ladder at all — asked, refused, and owed another attempt? */
  has(key: string): boolean { return this.places.has(key); }

  /** The route's own words for the last refusal of this place, or `null`. */
  whyOf(key: string): string | null { return this.places.get(key)?.why ?? null; }

  /** Consecutive refusals of this place; 0 for one that has not failed. */
  attemptsOf(key: string): number { return this.places.get(key)?.attempts ?? 0; }

  /** ms until this place may be asked again; 0 when it may go now, `null` when it never failed. */
  dueIn(key: string): number | null {
    const e = this.places.get(key);
    return e === undefined ? null : Math.max(0, e.dueAt - this.now());
  }

  /** It worked (or the place stopped existing): forget the streak. */
  clear(key: string): void { this.places.delete(key); }

  clearAll(): void { this.places.clear(); }

  /** How many places are owed another attempt. */
  get size(): number { return this.places.size; }

  /** How many of those are still inside their wait — what a readout means by "backing off". */
  get waiting(): number {
    const t = this.now();
    let n = 0;
    for (const e of this.places.values()) if (t < e.dueAt) n++;
    return n;
  }
}
