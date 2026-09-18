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
// So this module opened the same socket the waterfall used and **threw every row away**, writing
// only headers and timestamps into the store. T-457 took the third thing back — and only the third
// thing:
//
//  3. **The newest row itself** (`./trace.ts`'s [[LiveRow]]). The cutover's finding 1 is that the
//     surface draws *folded cells over time* while a trace is *this frame across frequency*: a cell
//     is a fold over at least one row, so the instantaneous trace is the one quantity on that screen
//     no tile can supply. It is kept in a one-row holder and **not** in the app store — rows arrive
//     tens of times a second, and the retired live view's own note ("spectrum rows go straight to
//     `Waterfall.push` and never enter the store") is the reason. Only the newest row is retained:
//     this is not the client-side accumulator T-457 declined to build, and it answers no question
//     about the past. The past is the pyramid's, and the trace's max-hold reads it from the tiles.
//
// The stream is therefore no longer consumed purely for its timestamps, which was the honest
// inefficiency this header used to admit. A lighter live-edge source is still a backend question;
// with a trace on the screen there is now less reason to want one.
//
// It also keeps `conn.spectrum`, which the shell shows: the socket's health is the honest signal for
// "is this server producing".
import * as ax from "../../axis";
import { LiveRow } from "../../surface/trace";
import type { AppContext } from "../context";
import { apiConnFor, backoffMs, openStream, parseSpectrumRecord, type StreamSocket } from "../net";
import { setLiveEdge } from "./slice";
import { nextView } from "./view";

/**
 * The newest spectrum row, shared with the centre's trace.
 *
 * A module singleton because `mountLiveEdge` is already mounted exactly once, by `./index.ts`'s own
 * `edgeStarted` latch, and because a row must not travel through the store (see above). The centre
 * mount reads it inside its per-frame callback, never on a poll — a trace laid out on the poll
 * cadence beside a per-frame scroll is T-388 on the other axis.
 */
export const liveRow = new LiveRow();

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
    // A retune re-plumbs the stream, and the row held here is of the band that has just ended. Drop
    // it for the same reason `edgeTS` goes back to null: a picture of the old band drawn over the new
    // one is not a stale picture, it is a false one.
    if (retuned) liveRow.clear();
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

  // The record's own absolute time, and the record's own row. Both are written whether or not any
  // viewport is following — pausing freezes a *view*, never the capture, so the edge keeps advancing
  // and the newest frame keeps being the newest frame while the user inspects a past window. (What
  // a *paused* pane does with that row is the trace's decision, not this one's: see
  // `./surface.ts`'s `traceFor`, which refuses to draw a frame outside the window it would sit on.)
  const onBinary = (buf: ArrayBuffer) => {
    const r = parseSpectrumRecord(buf);
    if (r?.type !== "data" || !Number.isFinite(r.tS)) return;
    store.set(setLiveEdge(r.tS));
    // A gated row carries no samples (`row === null`); it is a statement about the gate, not a
    // spectrum, so the last real frame stands rather than being replaced by nothing.
    const g = geom();
    if (r.row && g && r.row.length > 0) {
      liveRow.set({
        f0Hz: g.centerHz - g.bandwidthHz / 2,
        f1Hz: g.centerHz + g.bandwidthHz / 2,
        // Copied, not aliased: `parseSpectrumRecord` returns a view onto the socket's buffer, which
        // the next message reuses. A retained view would silently become the next row.
        db: new Float32Array(r.row),
        tNs: r.tS * 1e9,
      });
    }
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
