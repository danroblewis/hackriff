// **`coverage_changed`: a front end moved** (T-1040) — the client half of
// `GET /ws/tiles/changes` (docs/api.md).
//
// The server pushes one event per front-end MOVE: `{f_lo, f_hi, t}` is the union of the band it
// left and the band it arrived at, from the instant it arrived. Every tile meeting
// `[f_lo, f_hi] × [t, ∞)` holds coverage that is now out of date — the band left is fog from `t` —
// and no other tile does. So on an event the host re-asks its coverage survey at once and
// `TileCache.coverageChanged` re-fetches exactly those tiles, in one batch; the survey timer and the
// refresh lane stay as the fallback for a socket that is not open.
//
// Presentation only: the server decides what moved and where the fog is; this parses the message,
// refuses anything that does not add up, and hands the numbers on. It never reaches a device route.

/** The route. One socket per client: the event is about the tune record, not about a column. */
export const CHANGE_FEED_PATH = "/ws/tiles/changes";
/** How long a refused or dropped change socket waits before it is opened again. */
export const CHANGE_FEED_RETRY_MS = 5000;

/** One `coverage_changed`, in the client's units (Hz, ns). */
export interface CoverageChange {
  readonly seq: number;
  readonly device: string;
  /** Low / high edge of the union of the departed and arrived bands, Hz. */
  readonly fLoHz: number;
  readonly fHiHz: number;
  /** When the front end arrived on the new band, capture-clock ns. */
  readonly tNs: number;
  /** The band left, or null when the front end is new. */
  readonly departed: { readonly fLoHz: number; readonly fHiHz: number } | null;
  readonly arrived: { readonly fLoHz: number; readonly fHiHz: number };
}

export type ChangeMessage =
  | { readonly kind: "subscribed" }
  | { readonly kind: "change"; readonly change: CoverageChange }
  | { readonly kind: "refused"; readonly status: number; readonly reason: string };

const fin = (x: unknown): x is number => typeof x === "number" && Number.isFinite(x);

function band(v: unknown): { fLoHz: number; fHiHz: number } | null {
  const b = v as { f_lo?: unknown; f_hi?: unknown } | null;
  if (!b || !fin(b.f_lo) || !fin(b.f_hi) || !(b.f_hi > b.f_lo)) return null;
  return { fLoHz: b.f_lo, fHiHz: b.f_hi };
}

/** Parse one message. Throws on anything that does not add up — it is never patched over. */
export function parseChangeMessage(text: string): ChangeMessage {
  const m = JSON.parse(text) as Record<string, unknown>;
  switch (m.type) {
    case "subscribed":
      return { kind: "subscribed" };
    case "refused":
      return { kind: "refused", status: Number(m.status), reason: String(m.reason ?? "") };
    case "coverage_changed": {
      const union = band(m);
      const arrived = band(m.arrived);
      const departed = m.departed === null ? null : band(m.departed);
      if (!union || !arrived || (m.departed !== null && !departed) || !fin(m.t) || !fin(m.seq)) {
        throw new Error(`malformed coverage_changed: ${text}`);
      }
      return {
        kind: "change",
        change: {
          seq: m.seq, device: String(m.device ?? "unknown"),
          fLoHz: union.fLoHz, fHiHz: union.fHiHz, tNs: Math.round(m.t * 1e9), departed, arrived,
        },
      };
    }
    default:
      throw new Error(`unknown change message type ${String(m.type)}`);
  }
}

/** Opens one socket on `path`; `onText` per text message, `onClose` when it ends not by us. */
export type ChangeOpener = (path: string, onText: (text: string) => void, onClose: () => void) => { close(): void };

/**
 * **One change subscription, kept open** (T-1040). Every `coverage_changed` goes to `sink`; a
 * refusal, a protocol error or a dropped socket closes it and it is opened again after
 * [[CHANGE_FEED_RETRY_MS]] on the next [[keep]] — until then the survey timer is the fallback.
 */
export class CoverageChangeFeed {
  private conn: { close(): void } | null = null;
  private current: object | null = null;
  private retryAt = 0;
  private closed = false;
  /** Every path opened, in order — what ui/test asserts the request against. */
  readonly requests: string[] = [];
  /** Events delivered. */
  received = 0;

  constructor(
    private readonly open: ChangeOpener,
    private readonly sink: (c: CoverageChange) => void,
    private readonly opts: { retryMs?: number; now?: () => number } = {},
  ) {}

  get isOpen(): boolean { return this.current !== null; }

  /** Open the socket if it is not open and not waiting out a retry. Cheap; called every frame. */
  keep(): void {
    if (this.closed || this.current) return;
    const now = (this.opts.now ?? Date.now)();
    if (now < this.retryAt) return;
    this.requests.push(CHANGE_FEED_PATH);
    // The identity of THIS opening, set before `open` so a transport that answers synchronously is
    // still recognised, and so a late message from a socket already cut is ignored.
    const me = { conn: null as { close(): void } | null, cut: false };
    this.current = me;
    const cut = (): void => {
      if (this.current !== me || me.cut) return;
      me.cut = true;
      me.conn?.close();
      this.current = null;
      this.conn = null;
      this.retryAt = (this.opts.now ?? Date.now)() + (this.opts.retryMs ?? CHANGE_FEED_RETRY_MS);
    };
    const conn = this.open(CHANGE_FEED_PATH, (text) => {
      if (this.current !== me || me.cut) return;
      let m: ChangeMessage;
      try { m = parseChangeMessage(text); } catch { cut(); return; }
      if (m.kind === "refused") { cut(); return; }
      if (m.kind === "change") { this.received++; this.sink(m.change); }
    }, cut);
    if (me.cut) { conn.close(); return; }
    me.conn = conn;
    this.conn = conn;
  }

  close(): void {
    this.closed = true;
    this.conn?.close();
    this.conn = null;
    this.current = null;
  }
}

/** The browser transport: one WebSocket, the token as a query parameter like every `/ws/` route. */
export function wsChangeOpener(token: string, loc: { protocol: string; host: string } = location): ChangeOpener {
  return (path, onText, onClose) => {
    const proto = loc.protocol === "https:" ? "wss" : "ws";
    const sep = path.includes("?") ? "&" : "?";
    const ws = new WebSocket(`${proto}://${loc.host}${path}${sep}token=${encodeURIComponent(token)}`);
    let closedByUs = false;
    ws.onmessage = (ev) => { if (typeof ev.data === "string") onText(ev.data); };
    ws.onclose = () => { if (!closedByUs) onClose(); };
    ws.onerror = () => { /* onclose follows */ };
    return { close: () => { closedByUs = true; ws.close(); } };
  };
}
