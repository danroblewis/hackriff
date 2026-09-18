// **The live edge, and the tuned geometry** (T-445) — what survives of the retired
// `live-spectrum.ts` once the waterfall it drew is gone.
//
// The cutover (docs/16 §8.5) removes the separate live renderer: the unified surface draws the live
// edge as the finest growing edge of the same pyramid (T-439), from `GET /api/tiles`, so nothing
// paints spectrum rows any more. Two things the spectrum stream carried are still needed, and
// neither is a picture:
//
//  1. **The live edge on the capture clock** (T-379). `Date.now()` is not an answer — a replay or a
//     time-compressed mock scene runs on a clock of its own, and the fixture that first exposed this
//     sat 3.5 days from wall time. `GET /api/timeline` reports the capture window, but on a 60 s
//     poll: an edge that steps a minute at a time would drag every following pane and every open
//     signal box a minute at a time with it. The stream's per-row time is the only *smooth* source
//     of it this API offers.
//  2. **The tuned geometry** (centre, bandwidth, bins, row rate) that the control panel, the tuning
//     nudges, the bookmarks and the retune gate all read off `state.live`.
//
// So this module opens the same socket the waterfall used and **throws every row away**. It writes
// headers and timestamps into the store; it owns no DOM and renders nothing. That is worth saying
// out loud because it is an honest inefficiency: a full spectrum stream is being consumed for its
// timestamps. A lighter live-edge source is a backend question (there is no route that pushes just
// the edge), and inventing one in the client would mean guessing the edge between polls — which is
// the substitution this repo refuses everywhere else.
//
// It also keeps `conn.spectrum`, which the shell shows: the socket's health is still the honest
// signal for "is this server producing", even though nothing draws its output.
import * as ax from "../../axis";
import type { AppContext } from "../context";
import { apiConnFor, backoffMs, openStream, parseSpectrumRecord, type StreamSocket } from "../net";
import { setLiveEdge } from "./slice";
import { nextView } from "./view";

/** The `/api/streams` fields this needs (docs/api.md discovery). */
interface StreamInfo { stream_id: string; kind: string; remote_permitted: boolean }

/** Subscribes to the spectrum stream for its header and its row times. Returns a stop function. */
export function mountLiveEdge(ctx: AppContext): () => void {
  const { store } = ctx;
  let sock: StreamSocket | null = null;
  let attempt = 0, timer = 0, stopped = false;

  const geom = () => {
    const l = store.get().live;
    return l.centerHz === null || l.bandwidthHz === null || l.bins === null
      ? null : { centerHz: l.centerHz, bandwidthHz: l.bandwidthHz, bins: l.bins };
  };

  const retry = (reason: string) => {
    if (stopped) return;
    store.set((s) => ({ conn: { ...s.conn, spectrum: "reconnecting", message: reason } }));
    timer = window.setTimeout(() => void connect(), backoffMs(attempt++));
  };

  const onHeader = (hd: Record<string, unknown>) => {
    const g = ax.geometryOf(hd as { center_hz?: number; bandwidth_hz?: number; fft_size?: number });
    if (hd.kind !== "spectrum" || hd.datatype !== "rf32_le" || !g) return;
    attempt = 0;
    const rate = typeof hd.sample_rate_hz === "number" ? hd.sample_rate_hz : 25;
    const prev = geom();
    const retuned = !prev || prev.centerHz !== g.centerHz || prev.bandwidthHz !== g.bandwidthHz || prev.bins !== g.bins;
    store.set((s) => ({
      conn: { ...s.conn, spectrum: "live", message: "" },
      live: {
        ...s.live,
        streamId: String(hd.stream_id ?? ""), centerHz: g.centerHz, bandwidthHz: g.bandwidthHz,
        bins: g.bins, rowRateHz: rate,
        view: nextView(g, s.live.view, retuned ? s.live.pendingView : null),
        pendingView: retuned ? null : s.live.pendingView,
        // A retune re-plumbs the stream and the first rows of the new geometry have not arrived, so
        // the old band's edge is not this band's: back to *unknown* (T-379) rather than standing in
        // for a window nothing has yet sampled at this centre.
        edgeTS: retuned ? null : s.live.edgeTS,
        // A new geometry answers whatever the last pan asked about, so a standing offer is stale
        // (T-343): never leave a button that would retune to where the radio already is.
        retuneOffer: retuned ? null : s.live.retuneOffer,
      },
    }));
  };

  // The rows themselves are dropped on the floor: this module draws nothing. Only the record's own
  // absolute time is kept, and only when it is finite. Written whether or not any viewport is
  // following — pausing freezes a *view*, never the capture, so the edge keeps advancing while the
  // user inspects a past window.
  const onBinary = (buf: ArrayBuffer) => {
    const r = parseSpectrumRecord(buf);
    if (r?.type === "data" && Number.isFinite(r.tS)) store.set(setLiveEdge(r.tS));
  };

  async function connect() {
    if (stopped) return;
    sock?.close();
    store.set((s) => ({ conn: { ...s.conn, spectrum: "connecting" } }));
    let streams: StreamInfo[];
    try {
      ({ streams } = await ctx.client.get<{ streams: StreamInfo[] }>("/api/streams"));
    } catch (e) {
      const c = apiConnFor(e);
      store.set((s) => ({ conn: { ...s.conn, ...c } }));
      if (c.api === "unauthorized") return; // the shell asks for the token; reload resumes
      retry(`API: ${c.message}`);
      return;
    }
    const s = streams.find((x) => x.kind === "spectrum" && x.remote_permitted);
    if (!s) { store.set((st) => ({ conn: { ...st.conn, spectrum: "unavailable" } })); retry("no spectrum stream"); return; }
    sock = openStream(`/ws/${s.stream_id}`, ctx.token, {
      onHeader, onBinary,
      // T-417: a retune or re-plumb no longer reaches here — the bridge keeps this socket and
      // delivers the new window's header on it. What is left is a stream that really ended.
      onClose: (wasLive) => { if (wasLive) attempt = 0; retry(wasLive ? "stream ended; reconnecting" : "disconnected; retrying"); },
    });
  }

  void connect();
  return () => { stopped = true; clearTimeout(timer); sock?.close(); sock = null; };
}
