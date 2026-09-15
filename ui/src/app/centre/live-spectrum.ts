// MUI centre (skeleton, T-149): mounts the existing WebGL waterfall (ui/src/waterfall.ts) and feeds
// it from the first remote-permitted `spectrum` stream in `GET /api/streams`, reconnecting with
// backoff. T-152 owns this file from here: brackets, DC mask, region select, hover readout, zoom,
// axis, and the capture-timeline review mode.
import * as ax from "../../axis";
import { MARK_DROP, MARK_GATED, Waterfall } from "../../waterfall";
import type { AppContext } from "../context";
import { h } from "../dom";
import { apiConnFor, backoffMs, openStream, parseSpectrumRecord, type StreamSocket } from "../net";

interface StreamInfo { stream_id: string; kind: string; remote_permitted: boolean }

export function mountLiveSpectrum(el: HTMLElement, ctx: AppContext) {
  const { store } = ctx;
  let canvas = h("canvas", { class: "live-canvas", "aria-label": "Spectrum and waterfall" });
  const note = h("div", { class: "live-note", role: "status" });
  el.replaceChildren(canvas, note);

  let wf: Waterfall | null = null, sock: StreamSocket | null = null, attempt = 0, lastSeq = -1;
  const say = (text: string) => { note.textContent = text; note.hidden = !text; };

  const retry = (reason: string) => {
    store.set((s) => ({ conn: { ...s.conn, spectrum: "reconnecting", message: reason } }));
    say(reason);
    window.setTimeout(() => void connect(), backoffMs(attempt++));
  };

  const onHeader = (hd: Record<string, unknown>) => {
    const g = ax.geometryOf(hd as { center_hz?: number; bandwidth_hz?: number; fft_size?: number });
    if (hd.kind !== "spectrum" || hd.datatype !== "rf32_le" || !g) { say(`unsupported spectrum header (${String(hd.kind)}/${String(hd.datatype)})`); return; }
    attempt = 0;
    lastSeq = -1;
    const rate = typeof hd.sample_rate_hz === "number" ? hd.sample_rate_hz : 25;
    if (!wf || wf.bins !== g.bins) {
      if (wf) {
        wf.destroy(); // a lost WebGL context cannot be reused: fresh canvas
        const fresh = canvas.cloneNode(false) as HTMLCanvasElement;
        canvas.replaceWith(fresh);
        canvas = fresh;
      }
      try {
        wf = new Waterfall(canvas, g.bins, rate);
      } catch (e) {
        wf = null;
        say(`waterfall unavailable: ${(e as Error).message}`);
        return;
      }
    }
    const full = ax.fullView(g);
    store.set((s) => ({
      conn: { ...s.conn, spectrum: "live", message: "" },
      live: { streamId: String(hd.stream_id ?? ""), centerHz: g.centerHz, bandwidthHz: g.bandwidthHz, bins: g.bins, rowRateHz: rate, view: s.live.view ?? full },
    }));
    const v = store.get().live.view ?? full;
    wf.setView(...ax.textureWindow(g, v));
    say("");
  };

  const onBinary = (buf: ArrayBuffer) => {
    const r = parseSpectrumRecord(buf);
    if (!r || !wf) return;
    if (r.type === "dropped") { wf.mark(r.gated ? MARK_GATED : MARK_DROP); lastSeq = r.seq + r.count - 1; return; }
    if (r.type !== "data") return;
    if (lastSeq >= 0 && r.seq > lastSeq + 1) wf.mark(MARK_DROP);
    lastSeq = r.seq;
    if (r.gated) { wf.mark(MARK_GATED); return; }
    if (r.discontinuity) wf.mark(MARK_DROP);
    if (r.row && store.get().time.live) wf.push(r.row, r.tS); // reviewing: T-152 renders history instead
  };

  async function connect() {
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
      // A stream that was live and ended (a new replay pass, a re-plumb) reconnects at once.
      onClose: (wasLive) => { if (wasLive) attempt = 0; retry(wasLive ? "stream ended; reconnecting" : "disconnected; retrying"); },
    });
  }

  // Zoom set elsewhere (T-152) reaches the renderer.
  store.select((s) => s.live.view, (v) => {
    const st = store.get().live;
    if (!wf || !v || st.centerHz === null || st.bandwidthHz === null || st.bins === null) return;
    wf.setView(...ax.textureWindow({ centerHz: st.centerHz, bandwidthHz: st.bandwidthHz, bins: st.bins }, v));
  });

  void connect();
}
