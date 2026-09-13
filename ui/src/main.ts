// hackriff web UI entry: token, stream bridge client, waterfall, history panel.
// Stream contract: docs/stream-contract.md §10 (first text message = header JSON, then one message
// per record; binary records carry the 32-byte little-endian record header).
import { MARK_DROP, MARK_GATED, Waterfall } from "./waterfall";
import { HistoryPanel } from "./history";
import { InventoryTable } from "./inventory";

export type Api = (path: string) => Promise<unknown>;

interface Header {
  stream_id: string; kind: string; content_class: string; datatype?: string;
  fft_size?: number; sample_rate_hz?: number; center_hz?: number; bandwidth_hz?: number;
}

const FLAG_GATED = 1, FLAG_DISCONTINUITY = 2;
const CONTENT_CLASSES = new Set(["unrestricted", "own-key-decrypted"]);
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
  private rows = 0; private dropped = 0; private gated = 0; private lost = 0; private lastSeq = -1;
  private firstT = NaN; private lastT = NaN;
  private statsTimer = 0;

  constructor(private api: Api, private token: string, private historyPanel: HistoryPanel) {
    $("wf").addEventListener("pointermove", (e) => this.readout(e));
  }

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
    ws.onmessage = (ev) => (typeof ev.data === "string" ? this.onHeader(ev.data) : this.onRecord(ev.data as ArrayBuffer));
    let live = false;
    ws.addEventListener("message", () => { live = true; }, { once: true });
    ws.onclose = () => {
      // A stream that was live and ended (e.g. the next replay pass) reconnects at once.
      badge("conn", live ? "stream ended; reconnecting" : "disconnected; retrying", live ? "" : "bad");
      clearInterval(this.statsTimer);
      setTimeout(() => this.start(), live ? 250 : 2000);
    };
  }

  private onHeader(text: string) {
    if (this.header && this.wf) return; // messages streams are not rendered here
    const h = JSON.parse(text) as Header;
    if (h.kind !== "spectrum" || h.datatype !== "rf32_le") { badge("conn", `unsupported ${h.kind}/${h.datatype}`, "bad"); return; }
    this.header = h;
    this.rows = this.dropped = this.gated = this.lost = 0;
    this.lastSeq = -1;
    this.firstT = NaN;
    const bins = h.fft_size ?? 4096;
    if (!this.wf || this.wf.bins !== bins) { this.wf?.destroy(); this.wf = new Waterfall($("wf"), bins, h.sample_rate_hz ?? 25); }
    badge("conn", `live ${h.stream_id}`, "ok");
    badge("class", h.content_class);
    const gatedClass = !CONTENT_CLASSES.has(h.content_class);
    $("gated").hidden = !gatedClass;
    if (gatedClass) $("gated").textContent = `GATED ≤ ${(h.sample_rate_hz ?? 0).toFixed(1)} rows/s`;
    this.axis(h);
    if (this.sel) this.highlight(...this.sel);
    clearInterval(this.statsTimer);
    this.statsTimer = window.setInterval(() => this.stats(), 500);
  }

  private onRecord(buf: ArrayBuffer) {
    if (!this.wf || buf.byteLength < 32) return;
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
    this.wf.push(new Float32Array(buf, 32, n)); // little-endian host assumed (all browser targets)
    this.rows++;
    if (Number.isNaN(this.firstT)) {
      this.firstT = tS;
      const h = this.header!, bw = h.bandwidth_hz ?? 0, fc = h.center_hz ?? 0;
      this.historyPanel.setLive(fc - bw / 2, fc + bw / 2, tS, tS + 10);
    }
    this.lastT = tS;
  }

  private sel: [number, number] | null = null;

  /** Shades a frequency region on the waterfall (hidden when outside the live band). */
  highlight(fLoHz: number, fHiHz: number) {
    this.sel = [fLoHz, fHiHz];
    const el = $("wf-sel"), h = this.header;
    const bw = h?.bandwidth_hz ?? 0, lo = (h?.center_hz ?? 0) - bw / 2;
    const a = Math.max(0, (fLoHz - lo) / bw), b = Math.min(1, (fHiHz - lo) / bw);
    el.hidden = !h || !(bw > 0) || b <= a;
    if (el.hidden) return;
    el.style.left = `${a * 100}%`;
    el.style.width = `max(2px, ${(b - a) * 100}%)`;
  }

  private stats() {
    const w = this.wf;
    if (!w) return;
    const age = Number.isNaN(this.lastT) ? "" : ` row t ${new Date(this.lastT * 1000).toISOString().slice(11, 23)}Z`;
    $("stats").textContent = `${w.fps.toFixed(0)} fps · rows ${this.rows} · dropped ${this.dropped} · gated ${this.gated}` +
      `${this.lost ? ` · seq-gap ${this.lost}` : ""}${w.skipped ? ` · skipped ${w.skipped}` : ""} · ${w.lo.toFixed(0)}..${w.hi.toFixed(0)} dB${age}`;
  }

  private axis(h: Header) {
    const el = $("axis"), bw = h.bandwidth_hz ?? 0, fc = h.center_hz ?? 0;
    el.replaceChildren();
    for (const f of [0.02, 0.25, 0.5, 0.75, 0.98]) {
      const s = document.createElement("span");
      s.style.left = `${f * 100}%`;
      s.textContent = `${((fc - bw / 2 + f * bw) / 1e6).toFixed(3)}${f === 0.5 ? " MHz" : ""}`;
      el.append(s);
    }
  }

  private readout(e: PointerEvent) {
    const h = this.header, w = this.wf;
    if (!h || !w) return;
    const c = e.currentTarget as HTMLCanvasElement;
    const fx = Math.min(1, Math.max(0, e.offsetX / c.clientWidth)), bw = h.bandwidth_hz ?? 0;
    const mhz = ((h.center_hz ?? 0) - bw / 2 + fx * bw) / 1e6;
    const v = w.latest[Math.min(w.latest.length - 1, Math.floor(fx * w.latest.length))];
    $("readout").textContent = `${mhz.toFixed(4)} MHz  ${Number.isFinite(v) ? v.toFixed(1) : "–"} dBFS/Hz`;
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
  const live = new Live(api, token, panel);
  const inventory = new InventoryTable(api, panel, (lo, hi) => {
    live.highlight(lo, hi);
    panel.selectRegion(lo, hi);
  });
  void live.start();
  void inventory.load();
}

main();
