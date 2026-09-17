// The live presence-**endpoint** socket (T-388, rebuilt by T-410/ADR-0019): `/ws/presence`, open
// only while the view follows the live edge.
//
// Scope, deliberately narrow and unchanged. A paused or scrubbed view is answering about a fixed
// past window (T-379/T-384 wired every surface to it); every interval's endpoints in that window
// are already known and served by the poll, so a live endpoint stream has nothing to add. The
// socket is closed, that path stays exactly the poll it was, and going live re-subscribes. Only the
// following view subscribes, and all it ever does with a record is patch the `presence` of a row the
// poll already put on screen — `applyPresenceEvent` decides whether that is allowed
// (`ui/src/presence.ts`), and refuses rather than guesses.
import type { AppContext } from "../context";
import { backoffMs, openStream, type StreamSocket } from "../net";
import { applyPresenceEvent, parsePresenceEvent, PRESENCE_STREAM_ID } from "../../presence";
import { patchInventoryRow } from "./slice";

/** The `/api/streams` fields this needs (docs/api.md discovery). */
interface StreamInfo { stream_id: string; kind: string; remote_permitted: boolean }

/**
 * Subscribes the following view to presence endpoints; returns a stop function.
 *
 * Reconnects with the shared backoff, and re-checks liveness on every open: a view that paused
 * while the socket was retrying never reconnects.
 */
export function mountPresenceStream(ctx: AppContext): () => void {
  let sock: StreamSocket | null = null;
  let timer = 0;
  let attempt = 0;
  let stopped = false;

  const following = () => ctx.store.get().time.live;

  function close() {
    clearTimeout(timer);
    sock?.close();
    sock = null;
  }

  function retry() {
    if (stopped || !following()) return;
    clearTimeout(timer);
    timer = window.setTimeout(() => void connect(), backoffMs(attempt++));
  }

  function onText(text: string) {
    // A record that arrives after a pause is simply not applied: the frozen view's rows describe a
    // window this endpoint is not in.
    if (!following()) return;
    const ev = parsePresenceEvent(text);
    if (!ev) return;
    const row = ctx.store.get().inventory.rows[ev.emitterId];
    if (!row) return; // no row on screen means no box to cap or open, and none is invented
    const presence = applyPresenceEvent(row, ev);
    if (presence) ctx.store.set(patchInventoryRow(ev.emitterId, { presence }));
  }

  async function connect() {
    if (stopped || !following() || sock) return;
    let streams: StreamInfo[];
    try {
      ({ streams } = await ctx.client.get<{ streams: StreamInfo[] }>("/api/streams"));
    } catch {
      retry();
      return;
    }
    if (stopped || !following()) return;
    // A server with no presence stream (an older build, a run with no stream sink) keeps the poll
    // and nothing else: an open box still caps, on the 5 s poll rather than within ~1.25 s. That is
    // the same backstop a lost END falls back on (ADR-0019 §4), which is why it is safe to have.
    const s = streams.find((x) => x.stream_id === PRESENCE_STREAM_ID && x.remote_permitted);
    if (!s) { retry(); return; }
    sock = openStream(`/ws/${s.stream_id}`, ctx.token, {
      onHeader: () => { attempt = 0; },
      onText,
      onClose: () => { sock = null; retry(); },
    });
  }

  ctx.store.select((st) => st.time.live, (live) => {
    if (live) void connect();
    else close();
  }, { immediate: true });

  return () => { stopped = true; close(); };
}
