// **One shared, bounded backoff on the transport** (T-1035) — what every polling lane in this client
// does after the server stops answering at all.
//
// Each lane already paced itself: `startPoll` (ui/src/app/net.ts) backs a failing poll off to four
// times its interval, the coverage survey waits `SURVEY_EVERY_MS` after a failure, the density layer
// has its own retry, the tile cache has the silence ladder T-499 built for it, and the row feeds wait
// `ROW_FEED_RETRY_MS` per column. Every one of those is a fact about ITS OWN route, and that is the
// defect: with `hk serve` SIGKILLed under a live page, a dozen lanes each asking their own route on
// their own floor add up to a flat wall of connection failures — measured by `ui/e2e/canvas-journey`
// test 4 on main, 2026-09-25: **49 failed requests in the first 5 s and 29 in the second**, with no
// lane individually misbehaving.
//
// "Connection refused" is not a fact about a route; it is a fact about the SERVER. So the wait belongs
// to the transport, once, shared by every caller of `ControlClient` — exactly the reasoning T-499
// wrote down for the tile cache ("the backoff is on the transport, not the place"), applied one layer
// up where the other fifteen callers live.
//
// The shape is the standard one, and each rule is asserted in `ui/test/controls-backoff.test.ts`:
//
//  - **A ladder, doubling per CONSECUTIVE silence** from [[OFFLINE_BASE_MS]] to [[OFFLINE_MAX_MS]].
//  - **Reset on the first answer** — any answer, including an HTTP error. A 404 or a 503 means the
//    server is there and talking; only silence is evidence about the transport.
//  - **Half-open: one probe at a time** while the gate is armed. Fifteen lanes waking up at the same
//    instant cost one request, not fifteen, and the ladder measures the server rather than the fleet.
//  - **Never terminal.** A server that restarts is a changed answer; a client that gave up for the
//    session would need a page reload to come back, which is the worse defect one step on.
//  - **A user's own act is never gated.** A mutating call (a retune, a record, a bookmark) is an
//    explicit press, not a poll: it goes to the wire whatever the ladder says, and its outcome is
//    evidence like any other. Gating it would mean a press did nothing for up to 30 s after a blip.
//  - **An abort is not evidence.** A caller's own deadline or cancellation (T-927's density timeout)
//    says nothing about the server, so it neither raises the ladder nor resets it.
//
// Presentation-side only: this decides WHEN this client asks, and nothing about what an answer means.

/** The first wait after a silence, ms, and the ceiling it doubles up to. Same ladder the tile
 * cache's silence backoff uses (`../surface/tilecache.ts`), for the same reason. */
export const OFFLINE_BASE_MS = 500, OFFLINE_MAX_MS = 30_000;

/**
 * How long a probe may hold the single slot before the gate assumes it is lost, ms.
 *
 * Not every caller passes a deadline (`ControlClient.call`'s `signal` is optional by design — the
 * deadline belongs to the caller that knows what "too long" means for its route), so a probe that
 * hangs until the browser's own connection timeout would otherwise keep the gate shut for minutes
 * with nothing on the wire. Well above any healthy answer, well below that timeout.
 */
export const PROBE_STALE_MS = 15_000;

/**
 * The shared gate. `admit` before touching the wire, then exactly one of `answered` / `silent` /
 * `released` for every admitted request.
 *
 * The clock is injected so the unit tests drive the ladder without waiting for it (and so nothing
 * here reads a browser clock for anything but its own pacing, which is what a backoff is).
 */
export class TransportBackoff {
  /** Consecutive silences. 0 means the gate is open. */
  private fails = 0;
  /** When the gate next opens, ms on `now`'s clock. */
  private until = 0;
  /** The single half-open probe, and when it was let through. */
  private probeAt: number | null = null;

  constructor(
    private readonly now: () => number = () => Date.now(),
    private readonly baseMs: number = OFFLINE_BASE_MS,
    private readonly capMs: number = OFFLINE_MAX_MS,
  ) {}

  /** Consecutive silences — 0 when the server is answering. */
  get failures(): number { return this.fails; }

  /** ms until the gate opens; 0 when it is open (or when a probe may go now). */
  waitMs(): number { return this.fails === 0 ? 0 : Math.max(0, this.until - this.now()); }

  /**
   * May a request touch the wire? Claims the probe slot when the gate is armed.
   *
   * `mutating` is a user's own act and is always admitted (it does not claim the slot: a press must
   * not be able to lock out the probe that notices the server is back, and vice versa).
   */
  admit(mutating = false): boolean {
    if (mutating) return true;
    if (this.fails === 0) return true;
    const t = this.now();
    if (this.probeAt !== null) {
      if (t - this.probeAt < PROBE_STALE_MS) return false;
      this.probeAt = null;   // lost, never returned: the slot is not a permanent lock
    }
    if (t < this.until) return false;
    this.probeAt = t;
    return true;
  }

  /** The server answered — anything at all, an HTTP error included. */
  answered(): void {
    this.fails = 0;
    this.until = 0;
    this.probeAt = null;
  }

  /** No answer at all: a refused socket, a reset, a name that does not resolve, an origin gone. */
  silent(): void {
    this.fails++;
    this.until = this.now() + Math.min(this.capMs, this.baseMs * 2 ** (this.fails - 1));
    this.probeAt = null;
  }

  /** The caller's own abort: neither evidence. Frees the slot without moving the ladder. */
  released(): void { this.probeAt = null; }

  /** What to say in place of a request this gate did not make. */
  reason(): string {
    const s = Math.max(0, Math.round(this.waitMs() / 100) / 10);
    return `server unreachable: ${this.fails} failed connection${this.fails === 1 ? "" : "s"} in a row, ` +
      `not asking again for ${s} s`;
  }
}

/**
 * The error a gated call throws instead of asking. Not a `ControlError` — no status came back,
 * because nothing was sent — so `reactionTo` reads it as `offline`, the same reaction a real refused
 * socket produces. The page therefore says the same thing whether the client asked and failed or
 * declined to ask; the only difference is the wire.
 */
export class OfflineError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "OfflineError";
  }
}

/** Whether `e` is a caller's own abort rather than a transport failure. */
export function isAbort(e: unknown): boolean {
  return e instanceof Error && (e.name === "AbortError" || e.name === "TimeoutError");
}

/** The process-wide gate `ControlClient` uses by default: one server, one ladder. */
export const transportBackoff = new TransportBackoff();
