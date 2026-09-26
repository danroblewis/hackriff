// MUI transport layer (ADR-0013 §3.2): token, polling with backoff, WebSocket stream sockets with
// the stream-contract framing (docs/stream-contract.md §10: first text message is the header or,
// for `/ws/open/*`, a refusal), and the mapping of API errors onto the store's connection state.
// Wire-format parsing only (record headers); no signal logic.
import { ControlError, reactionTo } from "../controls/client";
import type { ApiConn } from "./state";

const TOKEN_KEY = "hk-token"; // shared with the old UI: one token per tab

/** Token from `#token=` / `?token=` (then stripped from the address bar) or this tab's storage. */
export function takeToken(loc: Location = location): string | null {
  const t = new URLSearchParams(loc.hash.slice(1)).get("token") ?? new URLSearchParams(loc.search).get("token");
  if (t) {
    sessionStorage.setItem(TOKEN_KEY, t);
    history.replaceState(null, "", withoutToken(loc));
  }
  return t ?? sessionStorage.getItem(TOKEN_KEY);
}

/**
 * The same address **without the token** — the credential goes, everything else stays (T-1042).
 *
 * It used to be `loc.pathname`, which also threw away the **query string**, so a client flag
 * (`?live-ring=1`, `src/flags.ts`) was gone from `location.search` before anything could read it and
 * silently did nothing. The token is the one thing that may not stay in the address bar; a flag is
 * the opposite — it is *meant* to be visible and to survive a reload, which is why the flags are
 * query parameters at all.
 */
export function withoutToken(loc: Pick<Location, "pathname" | "search">): string {
  const q = new URLSearchParams(loc.search);
  q.delete("token");
  const rest = q.toString();
  return rest ? `${loc.pathname}?${rest}` : loc.pathname;
}

/**
 * T-955: **a `#token=` navigation to this page is not a reload.** `takeToken` strips the hash, so the
 * address bar reads `/`, and navigating to `/#token=…` from there is a same-document fragment change:
 * no script re-runs, and the page keeps whatever it held — the explorer's "reloaded" page kept its
 * 162.2 MHz Go-to view and retune offer across a server restart and two retunes (shots 0428), and a
 * page stuck on "token needed" stayed stuck after being handed a token. So a token arriving in the
 * fragment is stored and the page genuinely reloads, booting exactly as a fresh open would.
 */
export function reloadOnTokenHash(
  win: { addEventListener(t: "hashchange", fn: () => void): void; location: { hash: string; reload(): void } },
  store: Pick<Storage, "setItem">,
): void {
  win.addEventListener("hashchange", () => {
    const t = new URLSearchParams(win.location.hash.slice(1)).get("token");
    if (!t) return;
    store.setItem(TOKEN_KEY, t);
    win.location.reload();
  });
}

export function storeToken(t: string) { sessionStorage.setItem(TOKEN_KEY, t); }
export function forgetToken() { sessionStorage.removeItem(TOKEN_KEY); }

/** Reconnect / retry delay for the n-th consecutive failure (0-based): 250 ms doubling, capped. */
export function backoffMs(attempt: number, baseMs = 250, capMs = 10_000): number {
  return Math.min(capMs, baseMs * 2 ** Math.max(0, Math.min(attempt, 16)));
}

/** A WebSocket URL for an origin-relative `/ws/...` path; GET-only, so `?token=` is allowed (docs/api.md). */
export function wsUrl(loc: { protocol: string; host: string }, path: string, token: string): string {
  const proto = loc.protocol === "https:" ? "wss" : "ws";
  return `${proto}://${loc.host}${path}${path.includes("?") ? "&" : "?"}token=${encodeURIComponent(token)}`;
}

/** The store's API connection state after a failed call. */
export function apiConnFor(e: unknown): { api: ApiConn; message: string } {
  const { reaction, message } = reactionTo(e);
  if (reaction === "reauth") return { api: "unauthorized", message };
  if (reaction === "offline") return { api: "offline", message };
  // T-1063: `503 overloaded` is the server ANSWERING at its connection cap, with `Retry-After`.
  // A 5xx normally means the server is not coping and the UI says offline; this one means it is
  // coping — it refused one connection and told us when to come back. The caller (every poller
  // here) retries on its own cadence, so the connection state stays `ok` rather than painting the
  // whole UI offline for a single refused socket.
  if (e instanceof ControlError && e.code === "overloaded") return { api: "ok", message };
  return { api: e instanceof ControlError && e.status >= 500 ? "offline" : "ok", message };
}

/**
 * Runs `task` now and then every `intervalMs` after each completion (never overlapping). Failures
 * back off (`backoffMs`) up to the interval × 4 and reset on success. Returns stop.
 */
export function startPoll(task: () => Promise<void>, intervalMs: number, onError: (e: unknown) => void = () => {}): () => void {
  let stopped = false, timer = 0, fails = 0;
  const run = async () => {
    if (stopped) return;
    try {
      await task();
      fails = 0;
    } catch (e) {
      fails++;
      onError(e);
    }
    if (!stopped) timer = window.setTimeout(run, fails ? Math.min(intervalMs * 4, Math.max(intervalMs, backoffMs(fails))) : intervalMs);
  };
  void run();
  return () => { stopped = true; clearTimeout(timer); };
}

// ---- binary record header (stream contract §5.2) ----

export const REC_DATA = 1, REC_DROPPED = 2, REC_STATUS = 3;
export const FLAG_GATED = 1, FLAG_DISCONTINUITY = 2;

export type SpectrumRecord =
  | { type: "data"; seq: number; tS: number; gated: boolean; discontinuity: boolean; row: Float32Array | null }
  | { type: "dropped"; seq: number; count: number; gated: boolean }
  | { type: "other"; seq: number };

/** Parses one binary spectrum-stream message; null when shorter than the 32-byte record header. */
export function parseSpectrumRecord(buf: ArrayBuffer): SpectrumRecord | null {
  if (buf.byteLength < 32) return null;
  const dv = new DataView(buf);
  const type = dv.getUint8(0), flags = dv.getUint8(1), seq = Number(dv.getBigUint64(8, true));
  if (type === REC_DROPPED) {
    const count = buf.byteLength >= 40 ? Number(dv.getBigUint64(32, true)) : 0;
    return { type: "dropped", seq, count, gated: !!(flags & FLAG_GATED) };
  }
  if (type !== REC_DATA) return { type: "other", seq };
  const tS = Number(dv.getBigInt64(16, true) / 1000n) / 1e6;
  const gated = !!(flags & FLAG_GATED);
  const n = (buf.byteLength - 32) >> 2;
  // Little-endian host assumed (every browser target), as in the old UI.
  return { type: "data", seq, tS, gated, discontinuity: !!(flags & FLAG_DISCONTINUITY), row: gated || n === 0 ? null : new Float32Array(buf, 32, n) };
}

// ---- stream sockets ----

/** `schema` of a stream header (docs/stream-contract.md §4), which no record carries. */
export const STREAM_SCHEMA = "hackriff.stream";

export interface StreamHandlers {
  /**
   * The stream header JSON (already parsed). The first text message always; and again, on the
   * same socket, whenever the producer re-offers the stream — a retune or a re-plumb finishes one
   * publisher and offers the next under the same id, and the bridge carries the connection across
   * (T-417, docs/stream-contract.md §10). Every record after it belongs to the new header.
   */
  onHeader(h: Record<string, unknown>): void;
  /** `/ws/open/*` refusal instead of a header (`{type: "refused", status, code, reason}`). */
  onRefused?(r: { status: number; code: string; reason: string }): void;
  onBinary?(buf: ArrayBuffer): void;
  /** Later text messages (messages streams: one NDJSON record each). */
  onText?(text: string): void;
  /** The socket closed; `live` says whether a header was received first. */
  onClose(live: boolean): void;
}

export interface StreamSocket { close(): void }

/** Opens one stream socket (no internal reconnect: the owner decides, using `backoffMs`). */
export function openStream(path: string, token: string, handlers: StreamHandlers): StreamSocket {
  const ws = new WebSocket(wsUrl(location, path, token));
  ws.binaryType = "arraybuffer";
  let first = true, live = false, closed = false;
  ws.onmessage = (ev) => {
    if (typeof ev.data !== "string") { if (live) handlers.onBinary?.(ev.data as ArrayBuffer); return; }
    if (!first) {
      // T-417: a stream id outlives its publishers. A retune or a re-plumb offers a new publisher
      // under the same id and the bridge carries this connection across to it, so a later text
      // message carrying the stream schema is the NEW header, not a record — the honest seam that
      // says the window moved. (The substring test keeps message streams, whose records are all
      // text, from being parsed twice; no record carries `schema`.)
      if (ev.data.includes(`"${STREAM_SCHEMA}"`)) {
        try {
          const h = JSON.parse(ev.data) as Record<string, unknown>;
          if (h.schema === STREAM_SCHEMA) { handlers.onHeader(h); return; }
        } catch { /* not a header after all: it is a record */ }
      }
      handlers.onText?.(ev.data);
      return;
    }
    first = false;
    let j: Record<string, unknown>;
    try { j = JSON.parse(ev.data) as Record<string, unknown>; } catch { ws.close(); return; }
    if (j.type === "refused") { handlers.onRefused?.(j as unknown as { status: number; code: string; reason: string }); return; }
    live = true;
    handlers.onHeader(j);
  };
  ws.onclose = () => { if (!closed) { closed = true; handlers.onClose(live); } };
  return { close: () => { closed = true; ws.close(); } };
}
