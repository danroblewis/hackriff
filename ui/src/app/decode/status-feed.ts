// Cross-task contract (ADR-0013 §3.2, §8): one shared `/ws/open/inspector?pipeline=<id>` socket per
// pipeline, reference-counted across subscribers, so the workbench (T-153: status -> quality tiles)
// and the packet inspector (T-154: frames) never open two sockets to the same pipeline.
// Owner: T-153, implemented here. T-154 subscribes with `{frame}` and never opens its own socket.
//
// **This transport is the live edge, by design, and the two record types it carries are treated
// differently downstream (T-387).**
//
// - `frame` records are *data about the air*: each carries its own capture-clock `t_ns`, and the
//   pipeline's frames are recorded to a capture. So the packet inspector does **not** treat this
//   socket as its window — it filters what arrives here to the view window and fetches the rest
//   from `GET /api/captures/{id}/frames?from_t&to_t`. This socket is one source of the window's
//   frames, never the definition of it.
// - `status` records are *telemetry of the decoder* — a node's lock, quality and error rate as it
//   is reading now. They are stored verbatim in the capture file (stream contract §14.7) but
//   nothing indexes or serves them by time, and a lock from an hour ago is not this stage's state.
//   The stage strip that renders them is therefore **live-only and says so** (`app/live-only.ts`).
//
// `/ws/open/<name>` is an **on-demand opener** and has no history form. T-387 deliberately left it
// that way: the one surface that needed a past window had an existing windowed route to reach it
// through, and growing a history form on an opener would be an ADR-0004 stream-contract change made
// to serve a surface that did not need it.
import type { AppContext } from "../context";
import { backoffMs, openStream, type StreamSocket } from "../net";

/** A status record (stream-contract §14.3), keys as served (`<node>.lock`, `<node>.quality`, …). */
export interface StatusUpdate { tS: number; values: Readonly<Record<string, number | string | boolean | null>> }

/** A frame record exactly as served (stream-contract §14.2); T-154 builds its view model from it. */
export type FrameRecord = Readonly<Record<string, unknown>>;

/** Feed connection state for inline status; `closed` once the pipeline ended or was refused. */
export type FeedState = "connecting" | "live" | "reconnecting" | "closed";

export interface FeedHandlers {
  status?(u: StatusUpdate): void;
  frame?(f: FrameRecord): void;
  state?(s: FeedState, message: string): void;
}

interface Conn {
  refs: Set<FeedHandlers>;
  socket: StreamSocket | null;
  attempt: number;
  timer: ReturnType<typeof setTimeout> | null;
  /** Terminal: a refusal, or every subscriber left. No more reconnects. */
  stopped: boolean;
  /** Set just before we close the socket ourselves, so the resulting onClose doesn't reconnect. */
  closingIntentionally: boolean;
}

// One conn map per AppContext, so tests (each building their own ctx) never see another test's
// sockets, without any explicit reset hook.
const registry = new WeakMap<AppContext, Map<string, Conn>>();

function connsFor(ctx: AppContext): Map<string, Conn> {
  let m = registry.get(ctx);
  if (!m) { m = new Map(); registry.set(ctx, m); }
  return m;
}

function tell(conn: Conn, s: FeedState, message: string) {
  for (const h of [...conn.refs]) h.state?.(s, message);
}

function values(raw: unknown): Record<string, number | string | boolean | null> {
  const out: Record<string, number | string | boolean | null> = {};
  if (raw && typeof raw === "object") {
    for (const [k, v] of Object.entries(raw as Record<string, unknown>)) {
      if (v === null || typeof v === "number" || typeof v === "string" || typeof v === "boolean") out[k] = v;
    }
  }
  return out;
}

function connect(ctx: AppContext, pipelineId: string, conn: Conn): void {
  if (conn.stopped || conn.refs.size === 0) return;
  tell(conn, conn.attempt > 0 ? "reconnecting" : "connecting", "");
  conn.socket = openStream(`/ws/open/inspector?pipeline=${encodeURIComponent(pipelineId)}`, ctx.token, {
    onHeader() {
      conn.attempt = 0;
      tell(conn, "live", "");
    },
    onRefused(r) {
      conn.stopped = true;
      tell(conn, "closed", r.reason || `${r.status} ${r.code}`);
    },
    onText(msg) {
      let rec: Record<string, unknown>;
      try {
        rec = JSON.parse(msg) as Record<string, unknown>;
      } catch {
        return;
      }
      if (rec.type === "status") {
        // `t_ns` is integer Unix nanoseconds (stream-contract §5.1); the plots' axis is seconds.
        const u: StatusUpdate = { tS: Number(rec.t_ns ?? 0) / 1e9, values: values(rec.metadata) };
        for (const h of [...conn.refs]) h.status?.(u);
      } else if (rec.type === "frame") {
        for (const h of [...conn.refs]) h.frame?.(rec as FrameRecord);
      }
    },
    onClose() {
      conn.socket = null;
      if (conn.closingIntentionally) return;
      if (conn.stopped || conn.refs.size === 0) return;
      const wait = backoffMs(conn.attempt++);
      tell(conn, "reconnecting", `reconnecting in ${Math.round(wait / 1000)}s`);
      conn.timer = setTimeout(() => connect(ctx, pipelineId, conn), wait);
    },
  });
}

/**
 * Subscribes to a pipeline's inspector stream. The first subscriber opens the socket, the last
 * unsubscribe closes it; reconnects with `backoffMs` while at least one subscriber remains and the
 * pipeline hasn't refused the connection. Returns the unsubscribe function.
 */
export function subscribePipelineFeed(ctx: AppContext, pipelineId: string, handlers: FeedHandlers): () => void {
  const conns = connsFor(ctx);
  let conn = conns.get(pipelineId);
  if (!conn) {
    conn = { refs: new Set(), socket: null, attempt: 0, timer: null, stopped: false, closingIntentionally: false };
    conns.set(pipelineId, conn);
  }
  conn.refs.add(handlers);
  if (conn.refs.size === 1 && !conn.socket && conn.timer === null) connect(ctx, pipelineId, conn);

  let done = false;
  return () => {
    if (done) return;
    done = true;
    const c = conns.get(pipelineId);
    if (!c) return;
    c.refs.delete(handlers);
    if (c.refs.size === 0) {
      c.closingIntentionally = true;
      if (c.timer !== null) { clearTimeout(c.timer); c.timer = null; }
      c.socket?.close();
      c.socket = null;
      conns.delete(pipelineId);
    }
  };
}
