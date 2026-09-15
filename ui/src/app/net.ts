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
    history.replaceState(null, "", loc.pathname);
  }
  return t ?? sessionStorage.getItem(TOKEN_KEY);
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

export interface StreamHandlers {
  /** First text message: the stream header JSON (already parsed). */
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
    if (!first) { handlers.onText?.(ev.data); return; }
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
