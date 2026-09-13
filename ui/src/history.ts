// Region-over-time panel: /api/history (T-017) as a heatmap, /api/floor (T-021) as a line.
import type { Api } from "./main";

type Num = number | null;
interface HistoryResp {
  level: number; unit: string; f_cell_hz: number; f_lo_hz: number; nf: number;
  t_cell_s: number; t0_s: number; nt: number; percentiles: [number, number];
  max_db: Num[]; mean_db: Num[]; p_low_db: Num[]; occupancy: Num[]; coverage: Num[]; frames: number[];
  provenance: { frames: number; suspect_fraction: number; gain_states: unknown[] };
}
interface FloorStep { t_s: number; duration_s: number; unit: string | null; value_db_per_hz: Num; uncertainty_db: Num; flag_names: string[] }
interface FloorResp { level: number; t_cell_s: number; steps: FloorStep[] }

const STOPS: [number, number[]][] = [[0, [0, 0, 10]], [0.2, [13, 26, 140]], [0.45, [0, 179, 230]], [0.7, [242, 230, 26]], [0.9, [242, 51, 13]], [1, [255, 255, 255]]];
const LUT = (() => {
  const lut = new Uint8ClampedArray(256 * 3);
  for (let i = 0; i < 256; i++) {
    const x = i / 255;
    let k = 1;
    while (k < STOPS.length - 1 && x > STOPS[k][0]) k++;
    const [x0, c0] = STOPS[k - 1], [x1, c1] = STOPS[k], f = (x - x0) / (x1 - x0);
    for (let j = 0; j < 3; j++) lut[i * 3 + j] = c0[j] + f * (c1[j] - c0[j]);
  }
  return lut;
})();

const $ = <T extends HTMLElement>(id: string) => document.getElementById(id) as T;
export const utcInput = (s: number) => new Date(s * 1000).toISOString().slice(0, 19);
export const fromUtcInput = (v: string) => Date.parse(v.length === 16 ? `${v}:00Z` : `${v}Z`) / 1000;
export const fmtT =(s: number) => new Date(s * 1000).toISOString().replace("T", " ").slice(0, 19);

export class HistoryPanel {
  private form = $<HTMLFormElement>("hist-form");
  private info = $("hist-info");
  private heat = $<HTMLCanvasElement>("heat");
  private floorCanvas = $<HTMLCanvasElement>("floor");
  private last: HistoryResp | null = null;
  private liveSpan: [number, number, number, number] | null = null;

  constructor(private api: Api) {
    this.form.addEventListener("submit", (e) => { e.preventDefault(); void this.load(); });
    $("stat").addEventListener("change", () => this.last && this.drawHeat(this.last));
    $("use-live").addEventListener("click", () => this.applyLive(true));
    window.addEventListener("resize", () => {
      if (this.last) this.drawHeat(this.last);
      if (this.lastFloor) this.drawFloor(this.lastFloor);
    });
  }

  private lastFloor: FloorResp | null = null;

  /** Remembers the live stream's span and row times; fills empty inputs. */
  setLive(fLoHz: number, fHiHz: number, t0s: number, t1s: number) {
    this.liveSpan = [fLoHz, fHiHz, t0s, t1s];
    this.applyLive(false);
  }

  /** The form's region and time window (MHz inputs → Hz; NaN when empty). */
  region(): { fLoHz: number; fHiHz: number; t0: number; t1: number } {
    const v = (id: string) => $<HTMLInputElement>(id).value;
    const mhz = (id: string) => (v(id) === "" ? NaN : +v(id) * 1e6);
    return { fLoHz: mhz("f-lo"), fHiHz: mhz("f-hi"), t0: fromUtcInput(v("t0")), t1: fromUtcInput(v("t1")) };
  }

  /** Selects a frequency region (e.g. an inventory row); loads it when a time window is set. */
  selectRegion(fLoHz: number, fHiHz: number) {
    $<HTMLInputElement>("f-lo").value = (fLoHz / 1e6).toFixed(6);
    $<HTMLInputElement>("f-hi").value = (fHiHz / 1e6).toFixed(6);
    if (this.form.checkValidity()) void this.load();
    else this.info.textContent = "region selected; set a time window and Load";
  }

  /** Selects a region and, when given, a time window (Unix s, widened to whole seconds); loads when complete. */
  selectWindow(fLoHz: number, fHiHz: number, t0?: number, t1?: number) {
    if (t0 !== undefined && t1 !== undefined) {
      const a = Math.floor(t0);
      $<HTMLInputElement>("t0").value = utcInput(a);
      $<HTMLInputElement>("t1").value = utcInput(Math.max(a + 1, Math.ceil(t1)));
    }
    this.selectRegion(fLoHz, fHiHz);
  }

  private applyLive(force: boolean) {
    if (!this.liveSpan) return;
    const [fl, fh, t0, t1] = this.liveSpan;
    const set = (id: string, v: string) => { const el = $<HTMLInputElement>(id); if (force || !el.value) el.value = v; };
    set("f-lo", (fl / 1e6).toFixed(4));
    set("f-hi", (fh / 1e6).toFixed(4));
    set("t0", utcInput(Math.floor(t0 - 10)));
    set("t1", utcInput(Math.ceil(t1 + 10)));
  }

  async load() {
    const fLo = +$<HTMLInputElement>("f-lo").value * 1e6, fHi = +$<HTMLInputElement>("f-hi").value * 1e6;
    const t0 = fromUtcInput($<HTMLInputElement>("t0").value), t1 = fromUtcInput($<HTMLInputElement>("t1").value);
    const maxF = Math.max(64, Math.floor(this.heat.clientWidth * Math.min(devicePixelRatio, 2)));
    const q = `f_lo=${fLo}&f_hi=${fHi}&t0=${t0}&t1=${t1}`;
    this.info.textContent = "loading…";
    const [h, f] = await Promise.allSettled([
      this.api(`/api/history?${q}&max_cells=${Math.min(500000, maxF * 400)}`),
      this.api(`/api/floor?${q}&max_steps=${Math.max(16, Math.floor(maxF / 2))}`),
    ]);
    const msgs: string[] = [];
    if (h.status === "fulfilled") {
      const r = h.value as HistoryResp;
      this.last = r;
      this.drawHeat(r);
      const observed = r.frames.filter((n) => n > 0).length;
      msgs.push(`L${r.level}: ${r.nt}×${r.nf} cells of ${(r.f_cell_hz / 1e3).toFixed(2)} kHz × ${r.t_cell_s} s, ` +
        `${observed} observed, unit ${r.unit}/Hz, ${r.provenance.frames} frames, suspect ${(100 * r.provenance.suspect_fraction).toFixed(1)}%`);
    } else msgs.push(`history: ${(h.reason as Error).message}`);
    if (f.status === "fulfilled") {
      const r = f.value as FloorResp;
      this.lastFloor = r;
      this.drawFloor(r);
      const units = new Set(r.steps.map((s) => s.unit).filter(Boolean));
      msgs.push(`floor L${r.level}: ${r.steps.length} steps, unit ${[...units].join("/") || "—"}`);
    } else {
      msgs.push(`floor: ${(f.reason as Error).message}`);
      this.floorCanvas.getContext("2d")?.clearRect(0, 0, this.floorCanvas.width, this.floorCanvas.height);
    }
    this.info.textContent = msgs.join(" · ");
  }

  private sized(c: HTMLCanvasElement): CanvasRenderingContext2D {
    const dpr = Math.min(devicePixelRatio || 1, 2);
    c.width = Math.floor(c.clientWidth * dpr);
    c.height = Math.floor(c.clientHeight * dpr);
    const ctx = c.getContext("2d")!;
    ctx.font = `${11 * dpr}px ui-monospace, Menlo, monospace`;
    return ctx;
  }

  private drawHeat(r: HistoryResp) {
    const stat = $<HTMLSelectElement>("stat").value as "max_db" | "mean_db" | "p_low_db" | "occupancy";
    const vals = r[stat];
    let lo = 0, hi = 1;
    if (stat !== "occupancy") {
      const finite = vals.filter((v): v is number => v !== null).sort((a, b) => a - b);
      lo = finite[Math.floor(finite.length * 0.02)] ?? 0;
      hi = finite[Math.floor(finite.length * 0.995)] ?? 1;
      if (hi - lo < 10) hi = lo + 10;
    }
    const img = new ImageData(r.nf, r.nt);
    for (let t = 0; t < r.nt; t++) {
      const y = r.nt - 1 - t; // newest at top, like the waterfall
      for (let f = 0; f < r.nf; f++) {
        const v = vals[t * r.nf + f], o = (y * r.nf + f) * 4;
        if (v === null) { img.data[o] = 48; img.data[o + 1] = 48; img.data[o + 2] = 56; } // not observed ≠ quiet
        else {
          const i = Math.max(0, Math.min(255, Math.round(((v - lo) / (hi - lo)) * 255))) * 3;
          img.data[o] = LUT[i]; img.data[o + 1] = LUT[i + 1]; img.data[o + 2] = LUT[i + 2];
        }
        img.data[o + 3] = 255;
      }
    }
    const ctx = this.sized(this.heat);
    const tmp = document.createElement("canvas");
    tmp.width = r.nf; tmp.height = r.nt;
    tmp.getContext("2d")!.putImageData(img, 0, 0);
    ctx.imageSmoothingEnabled = false;
    ctx.drawImage(tmp, 0, 0, this.heat.width, this.heat.height);
    const W = this.heat.width, H = this.heat.height, pad = 4;
    ctx.fillStyle = "rgba(0,0,0,.6)";
    ctx.fillRect(0, 0, W, 16 * Math.min(devicePixelRatio, 2));
    ctx.fillStyle = "#ddd";
    const label = stat === "occupancy" ? "0..1" : `${lo.toFixed(1)}..${hi.toFixed(1)} dB`;
    ctx.fillText(`${fmtT(r.t0_s + r.nt * r.t_cell_s)}  ${stat} ${label}`, pad, 12 * Math.min(devicePixelRatio, 2));
    ctx.fillText(`${fmtT(r.t0_s)}   ${(r.f_lo_hz / 1e6).toFixed(4)}–${((r.f_lo_hz + r.nf * r.f_cell_hz) / 1e6).toFixed(4)} MHz`, pad, H - pad);
  }

  private drawFloor(r: FloorResp) {
    const ctx = this.sized(this.floorCanvas), W = this.floorCanvas.width, H = this.floorCanvas.height;
    const pts = r.steps.filter((s) => s.value_db_per_hz !== null);
    if (!pts.length) { ctx.fillStyle = "#888"; ctx.fillText("no floor observations in range", 6, 16); return; }
    const t0 = r.steps[0].t_s, t1 = r.steps[r.steps.length - 1].t_s + r.t_cell_s;
    let lo = Infinity, hi = -Infinity;
    for (const s of pts) {
      const u = s.uncertainty_db ?? 0;
      lo = Math.min(lo, s.value_db_per_hz! - u); hi = Math.max(hi, s.value_db_per_hz! + u);
    }
    if (hi - lo < 2) { lo -= 1; hi += 1; }
    const x = (t: number) => ((t - t0) / (t1 - t0)) * W, y = (v: number) => H - 14 - ((v - lo) / (hi - lo)) * (H - 28);
    const segments: FloorStep[][] = [];
    let cur: FloorStep[] = [];
    for (const s of r.steps) {
      if (s.value_db_per_hz === null) { if (cur.length) segments.push(cur); cur = []; } else cur.push(s);
    }
    if (cur.length) segments.push(cur);
    for (const seg of segments) {
      ctx.fillStyle = "rgba(96,165,250,.25)";
      ctx.beginPath();
      seg.forEach((s, i) => { const px = x(s.t_s + s.duration_s / 2), py = y(s.value_db_per_hz! + (s.uncertainty_db ?? 0)); i ? ctx.lineTo(px, py) : ctx.moveTo(px, py); });
      for (let i = seg.length - 1; i >= 0; i--) { const s = seg[i]; ctx.lineTo(x(s.t_s + s.duration_s / 2), y(s.value_db_per_hz! - (s.uncertainty_db ?? 0))); }
      ctx.fill();
      ctx.strokeStyle = "#60a5fa";
      ctx.lineWidth = 1.5 * Math.min(devicePixelRatio, 2);
      ctx.beginPath();
      seg.forEach((s, i) => { const px = x(s.t_s + s.duration_s / 2), py = y(s.value_db_per_hz!); i ? ctx.lineTo(px, py) : ctx.moveTo(px, py); });
      ctx.stroke();
    }
    const unit = pts[0].unit === "dbm" ? "dBm/Hz" : "dBFS/Hz";
    ctx.fillStyle = "#ddd";
    ctx.fillText(`floor ${hi.toFixed(1)} ${unit}`, 6, 12);
    ctx.fillText(`${lo.toFixed(1)}   ${fmtT(t0)} → ${fmtT(t1)}`, 6, H - 3);
  }
}
