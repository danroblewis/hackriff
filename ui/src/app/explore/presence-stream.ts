// The live presence-extension socket (T-388): `/ws/presence`, open only while the view follows the
// live edge.
//
// Scope, deliberately narrow. A paused or scrubbed view is answering about a fixed past window
// (T-379/T-384 wired every surface to it); its rows are already complete for that window and a push
// has nothing to add, so the socket is closed and that path stays exactly the poll it was. Only the
// following view subscribes, and all it ever does with a record is patch the `presence` of a row the
// poll already put on screen — `extendPresence` decides whether that is allowed
// (`ui/src/presence.ts`), and refuses rather than guesses.
import type { AppContext } from "../context";
import { backoffMs, openStream, type StreamSocket } from "../net";
import { extendPresence, parsePresenceExtension, PRESENCE_STREAM_ID } from "../../presence";
import { patchInventoryRow } from "./slice";

/** The `/api/streams` fields this needs (docs/api.md discovery). */
interface StreamInfo { stream_id: string; kind: string; remote_permitted: boolean }

/**
 * Subscribes the following view to presence extensions; returns a stop function.
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
    // window this extension is not in.
    if (!following()) return;
    const ext = parsePresenceExtension(text);
    if (!ext) return;
    const row = ctx.store.get().inventory.rows[ext.emitterId];
    if (!row) return; // no row on screen means no box to extend, and none is invented
    const presence = extendPresence(row, ext);
    if (presence) ctx.store.set(patchInventoryRow(ext.emitterId, { presence }));
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
    // and nothing else: boxes grow as they did before, just not as fast.
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
