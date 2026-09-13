// hackriff web UI entry: token, stream bridge client, live view (waterfall, frequency axis,
// hover/touch readout, drag-to-select, click-to-inspect, zoom), selections, history, inventory.
// Stream contract: docs/stream-contract.md §10 (first text message = header JSON, then one message
// per record; binary records carry the 32-byte little-endian record header).
import * as ax from "./axis";
import { HistoryPanel } from "./history";
import { Inspector, inspectHalfWidthHz } from "./inspect";
import { InventoryTable } from "./inventory";
import { installListen } from "./listen";
import { SelectionPanel } from "./selection-panel";
import { SelectionStore, type NewSelection } from "./selections";
import { MARK_DROP, MARK_GATED, Waterfall } from "./waterfall";

export type Api = (path: string) => Promise<unknown>;

interface Header {
  stream_id: string; kind: string; content_class: string; datatype?: string;
  fft_size?: number; sample_rate_hz?: number; center_hz?: number; bandwidth_hz?: number;
}

/** A pointer position on the live canvas. */
interface Point { x: number; y: number; hz: number; hit: ax.YHit; t: number; heightPx: number }

const FLAG_GATED = 1, FLAG_DISCONTINUITY = 2;
const CONTENT_CLASSES = new Set(["unrestricted", "own-key-decrypted"]);
/** Pointer travel (CSS px) that turns a press into a drag instead of a click. */
const DRAG_PX = 6;
const $ = <T extends HTMLElement>(id: string) => document.getElementById(id) as T;

// Token: `#token=` (never sent to the server) or `?token=`; kept for this tab only, and stripped
// from the address bar so it does not linger in history or screenshots.
function takeToken(): string | null {
  const t = new URLSearchParams(location.hash.slice(1)).get("token") ?? new URLSearchParams(location.search).get("token");
  if (t) {
    sessionStorage.setItem("hk-token", t);
    history.replaceState(null, "", location.pathname);
  }
  return t ?? sessionStorage.getItem("hk-token");
}

function badge(id: string, text: string, cls = "") {
  const el = $(id);
  el.textContent = text;
  el.className = `badge ${cls}`.trim();
  el.hidden = false;
}

class Live {
  private wf: Waterfall | null = null;
  private header: Header | null = null;
  private geom: ax.Geometry | null = null;
  private view: ax.View | null = null;
  private rows = 0; private dropped = 0; private gated = 0; private lost = 0; private lastSeq = -1;
  private firstT = NaN; private lastT = NaN;
  private statsTimer = 0;
  private drag: { id: number; start: Point; cx: number; cy: number; moved: boolean } | null = null;
  private highlightHz: [number, number] | null = null;

  constructor(private api: Api, private token: string, private historyPanel: HistoryPanel,
    private selections: SelectionStore, private inspector: Inspector) {
    // Listeners sit on the wrapper, which is also the pointer-capture target: the canvas is
    // replaced when the bin count changes, and overlays inside never take pointer events. Touch
    // and pen get the readout on press (a tap has no pointermove; T-044 hover root cause).
    const wrap = $("wf-wrap");
    wrap.addEventListener("pointerdown", (e) => this.onDown(e));
    wrap.addEventListener("pointermove", (e) => this.onMove(e));
    wrap.addEventListener("pointerup", (e) => this.onUp(e));
    wrap.addEventListener("pointercancel", () => this.endDrag());
    $("zoom-reset").addEventListener("click", () => this.setView(null));
    selections.subscribe(() => this.drawOverlays());
    window.addEventListener("resize", () => { this.drawAxis(); this.drawOverlays(); });
    // Time-bounded selections scroll with the waterfall.
    window.setInterval(() => { if (this.selections.list().some((s) => s.t_lo !== undefined)) this.drawOverlays(); }, 200);
  }

  private get canvas() { return $<HTMLCanvasElement>("wf"); }

  async start() {
    try {
      const { streams } = (await this.api("/api/streams")) as { streams: { stream_id: string; kind: string; remote_permitted: boolean }[] };
      const want = new URLSearchParams(location.search).get("stream");
      const s = streams.find((x) => (want ? x.stream_id === want : x.kind === "spectrum" && x.remote_permitted));
      if (!s) { badge("conn", "no spectrum stream", "bad"); setTimeout(() => this.start(), 3000); return; }
      this.open(s.stream_id);
    } catch (e) {
      const msg = (e as Error).message;
      badge("conn", msg.startsWith("401") ? "bad token" : `API: ${msg}`, "bad");
      if (msg.startsWith("401")) { sessionStorage.removeItem("hk-token"); showAuth(); return; }
      setTimeout(() => this.start(), 3000);
    }
  }

  private open(streamId: string) {
    const proto = location.protocol === "https:" ? "wss" : "ws";
    const ws = new WebSocket(`${proto}://${location.host}/ws/${streamId}?token=${encodeURIComponent(this.token)}`);
    ws.binaryType = "arraybuffer";
    badge("conn", `connecting ${streamId}`);
    let first = true, live = false;
    ws.onmessage = (ev) => {
      live = true;
      if (typeof ev.data === "string") {
        if (first) this.onHeader(ev.data); // later text messages belong to messages streams: not rendered
      } else this.onRecord(ev.data as ArrayBuffer);
      first = false;
    };
    ws.onclose = () => {
      // A stream that was live and ended (e.g. the next replay pass) reconnects at once.
      badge("conn", live ? "stream ended; reconnecting" : "disconnected; retrying", live ? "" : "bad");
      clearInterval(this.statsTimer);
      setTimeout(() => this.start(), live ? 250 : 2000);
    };
  }

  /**
   * Applies the header every connection starts with. A new replay pass, a restarted server or a
   * retuned source always re-derives the geometry (T-045: the header of every connection after
   * the first used to be ignored, so the axis and readout kept a previous stream's centre/span).
   */
  private onHeader(text: string) {
    const h = JSON.parse(text) as Header;
    if (h.kind !== "spectrum" || h.datatype !== "rf32_le") { badge("conn", `unsupported ${h.kind}/${h.datatype}`, "bad"); return; }
    const g = ax.geometryOf(h);
    if (!g) { badge("conn", "spectrum header lacks center_hz/bandwidth_hz/fft_size", "bad"); return; }
    const old = this.geom;
    const same = !!old && old.centerHz === g.centerHz && old.bandwidthHz === g.bandwidthHz && old.bins === g.bins;
    this.header = h;
    this.geom = g;
    this.rows = this.dropped = this.gated = this.lost = 0;
    this.lastSeq = -1;
    this.firstT = NaN;
    if (!this.wf || this.wf.bins !== g.bins) {
      let canvas = this.canvas;
      if (this.wf) {
        this.wf.destroy();
        // A lost WebGL context cannot be reused: the new waterfall gets a fresh canvas.
        const fresh = canvas.cloneNode(false) as HTMLCanvasElement;
        canvas.replaceWith(fresh);
        canvas = fresh;
      }
      this.wf = new Waterfall(canvas, g.bins, h.sample_rate_hz ?? 25);
    }
    badge("conn", `live ${h.stream_id}`, "ok");
    badge("class", h.content_class);
    const gatedClass = !CONTENT_CLASSES.has(h.content_class);
    $("gated").hidden = !gatedClass;
    if (gatedClass) $("gated").textContent = `GATED ≤ ${(h.sample_rate_hz ?? 0).toFixed(1)} rows/s`;
    this.setView(same ? this.view : null);
    clearInterval(this.statsTimer);
    this.statsTimer = window.setInterval(() => this.stats(), 500);
  }

  private onRecord(buf: ArrayBuffer) {
    if (!this.wf || !this.geom || buf.byteLength < 32) return;
    const dv = new DataView(buf);
    const type = dv.getUint8(0), flags = dv.getUint8(1), seq = Number(dv.getBigUint64(8, true));
    const tS = Number(dv.getBigInt64(16, true) / 1000n) / 1e6;
    if (type === 2 && buf.byteLength >= 40) {
      const count = Number(dv.getBigUint64(32, true));
      if (flags & FLAG_GATED) { this.gated += count; this.wf.mark(MARK_GATED); }
      else { this.dropped += count; this.wf.mark(MARK_DROP); }
      this.lastSeq = seq + count - 1;
      return;
    }
    if (type !== 1) return; // unknown record type: skip (minor-version rule)
    if (this.lastSeq >= 0 && seq > this.lastSeq + 1) { this.lost += seq - this.lastSeq - 1; this.wf.mark(MARK_DROP); }
    this.lastSeq = seq;
    if (flags & FLAG_GATED) { this.gated++; this.wf.mark(MARK_GATED); return; } // header-only record
    if (flags & FLAG_DISCONTINUITY) this.wf.mark(MARK_DROP);
    const n = (buf.byteLength - 32) >> 2;
    this.wf.push(new Float32Array(buf, 32, n), tS); // little-endian host assumed (all browser targets)
    this.rows++;
    if (Number.isNaN(this.firstT)) {
      this.firstT = tS;
      const f = ax.fullView(this.geom);
      this.historyPanel.setLive(f.loHz, f.hiHz, tS, tS + 10);
    }
    this.lastT = tS;
  }

  /** Shades a frequency region (e.g. an inventory row) on the live view. */
  highlight(fLoHz: number, fHiHz: number) {
    this.highlightHz = [fLoHz, fHiHz];
    this.drawOverlays();
  }

  /** Zooms the live view to a region, padded by 10 % each side. */
  zoomTo(fLoHz: number, fHiHz: number) {
    if (!this.geom) return;
    const pad = 0.1 * (fHiHz - fLoHz);
    this.setView(ax.zoomTo(this.geom, fLoHz - pad, fHiHz + pad));
  }

  /** Sets the zoom (null: the whole band) and redraws what depends on it. */
  private setView(v: ax.View | null) {
    const g = this.geom;
    if (!g) return;
    const full = ax.fullView(g);
    this.view = v ?? full;
    this.wf?.setView(...ax.textureWindow(g, this.view));
    $("zoom-reset").hidden = this.view.loHz <= full.loHz && this.view.hiHz >= full.hiHz;
    $("view-info").textContent = ax.describe(g, this.view);
    this.drawAxis();
    this.drawOverlays();
  }

  private stats() {
    const w = this.wf;
    if (!w) return;
    const age = Number.isNaN(this.lastT) ? "" : ` row t ${new Date(this.lastT * 1000).toISOString().slice(11, 23)}Z`;
    $("stats").textContent = `${w.fps.toFixed(0)} fps · rows ${this.rows} · dropped ${this.dropped} · gated ${this.gated}` +
      `${this.lost ? ` · seq-gap ${this.lost}` : ""}${w.skipped ? ` · skipped ${w.skipped}` : ""} · ${w.lo.toFixed(0)}..${w.hi.toFixed(0)} dB${age}`;
  }

  private drawAxis() {
    const el = $("axis"), v = this.view;
    el.replaceChildren();
    if (!v) return;
    const ts = ax.ticks(v, Math.max(2, Math.floor((el.clientWidth || 640) / 110)));
    const step = ts.length > 1 ? ts[1].hz - ts[0].hz : v.hiHz - v.loHz;
    for (const t of ts) {
      if (t.frac < 0.02 || t.frac > 0.98) continue;
      const s = document.createElement("span");
      s.style.left = `${t.frac * 100}%`;
      s.textContent = ax.fmtMHz(t.hz, step);
      el.append(s);
    }
  }

  private drawOverlays() {
    const box = $("wf-overlays"), v = this.view, w = this.wf;
    box.replaceChildren();
    if (!v || !w) return;
    const place = (el: HTMLElement, loHz: number, hiHz: number) => {
      const a = ax.hzToFrac(v, loHz), b = ax.hzToFrac(v, hiHz);
      if (!(b > 0 && a < 1)) return false;
      const l = Math.max(0, a), r = Math.min(1, b);
      el.style.left = `${l * 100}%`;
      el.style.width = `max(2px, ${(r - l) * 100}%)`;
      return true;
    };
    if (this.highlightHz) {
      const el = document.createElement("div");
      el.className = "hl-box";
      if (place(el, ...this.highlightHz)) box.append(el);
    }
    const period = 1 / Math.max(1e-3, this.header?.sample_rate_hz ?? 25);
    for (const s of this.selections.list()) {
      const el = document.createElement("div");
      el.className = "sel-box";
      if (!place(el, s.f_lo, s.f_hi)) continue;
      let top = 0, bottom = 1;
      if (s.t_lo !== undefined && s.t_hi !== undefined) {
        const span = ax.timeSpanY(s.t_lo, s.t_hi, w.timeAt(0), period, w.specFrac, w.rows);
        if (span) [top, bottom] = span;
        else { el.classList.add("scrolled"); bottom = w.specFrac; } // time range off screen: band only
      }
      el.style.top = `${top * 100}%`;
      el.style.height = `${(bottom - top) * 100}%`;
      const label = document.createElement("span");
      label.textContent = s.name;
      el.append(label);
      box.append(el);
    }
  }

  private locate(e: PointerEvent): Point | null {
    const v = this.view, w = this.wf;
    if (!v || !w) return null;
    const r = this.canvas.getBoundingClientRect();
    const x = ax.pointerFrac(e.clientX, r);
    const y = r.height > 0 ? Math.min(1, Math.max(0, (e.clientY - r.top) / r.height)) : 0;
    const hit = ax.yHit(y, w.specFrac, w.rows);
    return { x, y, hz: ax.fracToHz(v, x), hit, t: hit.area === "waterfall" ? w.timeAt(hit.rowsBack) : NaN, heightPx: r.height };
  }

  /** Frequency (the centre of the bin under the pointer) and that bin's level in the newest row. */
  private binAt(hz: number): { hz: number; level: number } {
    const g = this.geom!, f = ax.snapHz(g, hz);
    return { hz: f, level: this.wf!.levelAt(ax.hzToFrac(ax.fullView(g), f)) };
  }

  private readout(p: Point) {
    const g = this.geom!, b = this.binAt(p.hz);
    const time = Number.isFinite(p.t) ? `  ${new Date(p.t * 1000).toISOString().slice(11, 23)}Z` : "";
    $("readout").textContent = `${ax.fmtMHz(b.hz, ax.binWidthHz(g))} MHz  ${Number.isFinite(b.level) ? b.level.toFixed(1) : "–"} dBFS/Hz${time}`;
  }

  private onDown(e: PointerEvent) {
    if (e.pointerType === "mouse" && e.button !== 0) return;
    const p = this.locate(e);
    if (!p) return;
    this.readout(p);
    this.drag = { id: e.pointerId, start: p, cx: e.clientX, cy: e.clientY, moved: false };
    $("wf-wrap").setPointerCapture(e.pointerId);
    e.preventDefault();
  }

  private onMove(e: PointerEvent) {
    const p = this.locate(e);
    if (!p) return;
    this.readout(p);
    const d = this.drag;
    if (!d || d.id !== e.pointerId) return;
    if (!d.moved && Math.hypot(e.clientX - d.cx, e.clientY - d.cy) < DRAG_PX) return;
    d.moved = true;
    this.drawDraft(d.start, p);
  }

  private onUp(e: PointerEvent) {
    const d = this.drag;
    if (!d || d.id !== e.pointerId) return;
    this.endDrag();
    const p = this.locate(e);
    if (!p) return;
    if (!d.moved) { this.inspect(p); return; }
    const sel = this.draftSelection(d.start, p);
    if (!sel) return;
    try {
      this.selections.add(sel);
    } catch (err) {
      $("sel-info").textContent = `selection refused: ${(err as Error).message}`;
    }
  }

  private endDrag() {
    this.drag = null;
    $("wf-draft").hidden = true;
  }

  /** A drag from `a` to `b`: its frequency extent, plus a time extent when it moved vertically within the waterfall. */
  private draftSelection(a: Point, b: Point): NewSelection | null {
    const s = ax.selectionHz(this.view!, a.x, b.x);
    if (!(s.bandwidthHz > 0)) return null;
    const out: NewSelection = { f_lo: s.loHz, f_hi: s.hiHz };
    const w = this.wf!, ha = a.hit, hb = b.hit;
    if (ha.area === "waterfall" && hb.area === "waterfall" && Math.abs(b.y - a.y) * b.heightPx >= DRAG_PX) {
      const near = Math.min(ha.rowsBack, hb.rowsBack);
      let far = Math.max(ha.rowsBack, hb.rowsBack);
      while (far > near && !Number.isFinite(w.timeAt(far))) far--; // below the rows received so far: clamp to the oldest
      const newer = w.timeAt(near), older = w.timeAt(far);
      const period = 1 / Math.max(1e-3, this.header?.sample_rate_hz ?? 25);
      if (Number.isFinite(newer) && Number.isFinite(older)) { out.t_lo = older; out.t_hi = newer + period; }
    }
    return out;
  }

  private drawDraft(a: Point, b: Point) {
    const el = $("wf-draft"), sel = this.draftSelection(a, b);
    if (!sel) { el.hidden = true; return; }
    const l = Math.min(a.x, b.x), r = Math.max(a.x, b.x);
    const timed = sel.t_lo !== undefined;
    const top = timed ? Math.min(a.y, b.y) : 0, bottom = timed ? Math.max(a.y, b.y) : 1;
    el.style.left = `${l * 100}%`;
    el.style.width = `${(r - l) * 100}%`;
    el.style.top = `${top * 100}%`;
    el.style.height = `${(bottom - top) * 100}%`;
    const df = ax.binWidthHz(this.geom!);
    $("wf-draft-label").textContent = `${ax.fmtMHz(sel.f_lo, df)}–${ax.fmtMHz(sel.f_hi, df)} MHz · ${ax.fmtBandwidth(sel.f_hi - sel.f_lo)}` +
      (timed ? ` · ${(sel.t_hi! - sel.t_lo!).toFixed(2)} s` : "");
    el.hidden = false;
  }

  private inspect(p: Point) {
    const g = this.geom!, v = this.view!, b = this.binAt(p.hz);
    void this.inspector.show(b.hz, inspectHalfWidthHz(v.hiHz - v.loHz, ax.binWidthHz(g)), b.level, p.t);
  }
}

function showAuth() {
  const form = $<HTMLFormElement>("auth");
  form.hidden = false;
  form.onsubmit = (e) => {
    e.preventDefault();
    sessionStorage.setItem("hk-token", $<HTMLInputElement>("token-input").value.trim());
    location.reload();
  };
}

function main() {
  const token = takeToken();
  if (!token) { badge("conn", "token needed", "bad"); showAuth(); return; }
  const api: Api = async (path) => {
    const r = await fetch(path, { headers: { Authorization: `Bearer ${token}` }, cache: "no-store" });
    const body = await r.json().catch(() => ({}));
    if (!r.ok) throw new Error(`${r.status} ${(body as { error?: string }).error ?? r.statusText}`);
    return body;
  };
  const panel = new HistoryPanel(api);
  const selections = new SelectionStore();
  const listen = installListen(token); // T-043
  const live = new Live(api, token, panel, selections, new Inspector(api, listen.onShown));
  new SelectionPanel(selections, {
    zoom: (s) => live.zoomTo(s.f_lo, s.f_hi),
    history: (s) => panel.selectWindow(s.f_lo, s.f_hi, s.t_lo, s.t_hi),
    listen: listen.selection,
  });
  const inventory = new InventoryTable(api, panel, (lo, hi) => {
    live.highlight(lo, hi);
    panel.selectRegion(lo, hi);
  });
  void live.start();
  void inventory.load();
}

main();
