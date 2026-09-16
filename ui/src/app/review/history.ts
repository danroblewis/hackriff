// History tab (ADR-0013 §2, §8; T-155): region-over-time over docs/api.md `GET /api/history` and
// `GET /api/floor`, the same routes and colour convention as the old ui/src/history.ts (unobserved
// cells are drawn grey, never as quiet) — reimplemented against the drawer's own canvas elements
// rather than the old page's fixed ids, per ADR-0013 §7 ("renders new DOM rather than moving the
// old classes").
import type { ControlClient } from "../../controls/client";
import { fmtT, fromUtcInput, utcInput } from "../../history";
import type { TimeCursor } from "../capture/slice";
import type { LiveSlice } from "../centre/slice";
import { h } from "../dom";
import type { AppState, ReviewSlice } from "../state";
import type { Store } from "../store";
import { errText } from "./util";

type Num = number | null;
interface HistoryResp {
  level: number; unit: string; f_cell_hz: number; f_lo_hz: number; nf: number;
  t_cell_s: number; t0_s: number; nt: number;
  max_db: Num[]; mean_db: Num[]; p_low_db: Num[]; occupancy: Num[]; frames: number[];
  provenance: { frames: number; suspect_fraction: number };
}
interface FloorStep { t_s: number; duration_s: number; unit: string | null; value_db_per_hz: Num }
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

/** The form's default region/span: the drawer's region (e.g. a selection's History action) else
 * the current live view, widened by 10 s either side; already-known UI state only. */
export function defaultHistoryRegion(
  region: ReviewSlice["region"], live: Pick<LiveSlice, "view">, time: TimeCursor, nowS: number,
): { fLoHz: number; fHiHz: number; t0: number; t1: number } | null {
  const box = region ?? (live.view ? { loHz: live.view.loHz, hiHz: live.view.hiHz } : null);
  if (!box) return null;
  const t1 = region?.t1 ?? (time.live ? nowS : time.tS);
  const t0 = region?.t0 ?? t1 - 600;
  return { fLoHz: box.loHz, fHiHz: box.hiHz, t0: t0 - 10, t1: t1 + 10 };
}

export class HistoryTab {
  private lastRegionKey = "";
  private readonly info = h("div", { class: "rv-info hint" });
  private readonly fLo = h("input", { class: "mono", inputmode: "decimal" });
  private readonly fHi = h("input", { class: "mono", inputmode: "decimal" });
  private readonly t0 = h("input", { type: "datetime-local", step: "1" });
  private readonly t1 = h("input", { type: "datetime-local", step: "1" });
  private readonly stat = h("select", {}, ...["max_db", "mean_db", "p_low_db", "occupancy"].map((s) => h("option", { value: s }, s)));
  private readonly heat = h("canvas", { class: "rv-heat" });
  private readonly floorCanvas = h("canvas", { class: "rv-floor" });
  private last: HistoryResp | null = null;
  private readonly root: HTMLElement;

  constructor(private client: ControlClient, private store: Store<AppState>) {
    this.stat.addEventListener("change", () => this.last && this.drawHeat(this.last));
    const form = h("form", { class: "rv-form", onsubmit: (e) => { e.preventDefault(); void this.load(); } },
      h("label", {}, "f lo (MHz)", this.fLo), h("label", {}, "f hi (MHz)", this.fHi),
      h("label", {}, "from (UTC)", this.t0), h("label", {}, "to (UTC)", this.t1),
      h("label", {}, "stat", this.stat),
      h("button", { class: "mini", type: "submit" }, "Load"));
    this.root = h("div", { class: "rv-panel" }, form, this.info, this.heat, this.floorCanvas);
  }

  el(): HTMLElement { return this.root; }

  activate(region: ReviewSlice["region"]) {
    const key = region ? JSON.stringify(region) : "";
    if (key === this.lastRegionKey && this.info.textContent) return;
    this.lastRegionKey = key;
    const s = this.store.get();
    const r = defaultHistoryRegion(region, s.live, s.time, Date.now() / 1000);
    if (!r) { this.info.textContent = "open a region in Explore, or set f lo/f hi/from/to below."; return; }
    this.fLo.value = (r.fLoHz / 1e6).toFixed(6);
    this.fHi.value = (r.fHiHz / 1e6).toFixed(6);
    this.t0.value = utcInput(Math.floor(r.t0));
    this.t1.value = utcInput(Math.ceil(r.t1));
    void this.load();
  }

  private async load() {
    const fLo = +this.fLo.value * 1e6, fHi = +this.fHi.value * 1e6;
    const t0 = fromUtcInput(this.t0.value), t1 = fromUtcInput(this.t1.value);
    if (![fLo, fHi, t0, t1].every(Number.isFinite) || fHi <= fLo || t1 <= t0) { this.info.textContent = "set f lo, f hi, from and to"; return; }
    const maxF = Math.max(64, Math.floor((this.heat.clientWidth || 400) * Math.min(devicePixelRatio || 1, 2)));
    const maxT = Math.max(16, Math.floor((this.heat.clientHeight || 140) * Math.min(devicePixelRatio || 1, 2)));
    const q = `f_lo=${fLo}&f_hi=${fHi}&t0=${t0}&t1=${t1}`;
    this.info.textContent = "loading…";
    const [h1, f] = await Promise.allSettled([
      // T-334: `max_f` is the heat-map's own pixel width and `max_t` its height, so the grid comes
      // back shaped for what is drawn instead of a product that any shape can satisfy.
      this.client.get<HistoryResp>(
        `/api/history?${q}&max_cells=${Math.min(500000, maxF * 400)}&max_f=${maxF}&max_t=${maxT}`,
      ),
      this.client.get<FloorResp>(`/api/floor?${q}&max_steps=${Math.max(16, Math.floor(maxF / 2))}`),
    ]);
    const msgs: string[] = [];
    if (h1.status === "fulfilled") {
      this.last = h1.value;
      this.drawHeat(h1.value);
      const observed = h1.value.frames.filter((n) => n > 0).length;
      msgs.push(`${h1.value.nt}×${h1.value.nf} cells, ${observed} observed, unit ${h1.value.unit}/Hz, ${h1.value.provenance.frames} frames`);
    } else msgs.push(`history: ${errText(h1.reason)}`);
    if (f.status === "fulfilled") this.drawFloor(f.value);
    else { msgs.push(`floor: ${errText(f.reason)}`); this.floorCanvas.getContext("2d")?.clearRect(0, 0, this.floorCanvas.width, this.floorCanvas.height); }
    this.info.textContent = msgs.join(" · ");
  }

  private sized(c: HTMLCanvasElement): CanvasRenderingContext2D {
    const dpr = Math.min(devicePixelRatio || 1, 2);
    c.width = Math.floor((c.clientWidth || 300) * dpr);
    c.height = Math.floor((c.clientHeight || 140) * dpr);
    const ctx = c.getContext("2d")!;
    ctx.font = `${11 * dpr}px ui-monospace, Menlo, monospace`;
    return ctx;
  }

  private drawHeat(r: HistoryResp) {
    const stat = this.stat.value as "max_db" | "mean_db" | "p_low_db" | "occupancy";
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
      const y = r.nt - 1 - t;
      for (let f = 0; f < r.nf; f++) {
        const v = vals[t * r.nf + f], o = (y * r.nf + f) * 4;
        if (v === null) { img.data[o] = 48; img.data[o + 1] = 48; img.data[o + 2] = 56; }
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
    const W = this.heat.width, pad = 4;
    ctx.fillStyle = "rgba(0,0,0,.6)";
    ctx.fillRect(0, 0, W, 16 * Math.min(devicePixelRatio || 1, 2));
    ctx.fillStyle = "#ddd";
    const label = stat === "occupancy" ? "0..1" : `${lo.toFixed(1)}..${hi.toFixed(1)} dB`;
    ctx.fillText(`${fmtT(r.t0_s + r.nt * r.t_cell_s)}  ${stat} ${label}`, pad, 12 * Math.min(devicePixelRatio || 1, 2));
  }

  private drawFloor(r: FloorResp) {
    const ctx = this.sized(this.floorCanvas), W = this.floorCanvas.width, H = this.floorCanvas.height;
    const pts = r.steps.filter((s) => s.value_db_per_hz !== null);
    if (!pts.length) { ctx.fillStyle = "#888"; ctx.fillText("no floor observations in range", 6, 16); return; }
    const t0 = r.steps[0].t_s, t1 = r.steps[r.steps.length - 1].t_s + r.t_cell_s;
    let lo = Infinity, hi = -Infinity;
    for (const s of pts) { lo = Math.min(lo, s.value_db_per_hz!); hi = Math.max(hi, s.value_db_per_hz!); }
    if (hi - lo < 2) { lo -= 1; hi += 1; }
    const x = (t: number) => ((t - t0) / (t1 - t0)) * W, y = (v: number) => H - 14 - ((v - lo) / (hi - lo)) * (H - 28);
    ctx.strokeStyle = "#60a5fa";
    ctx.lineWidth = 1.5 * Math.min(devicePixelRatio || 1, 2);
    ctx.beginPath();
    pts.forEach((s, i) => { const px = x(s.t_s + s.duration_s / 2), py = y(s.value_db_per_hz!); i ? ctx.lineTo(px, py) : ctx.moveTo(px, py); });
    ctx.stroke();
    const unit = pts[0].unit === "dbm" ? "dBm/Hz" : "dBFS/Hz";
    ctx.fillStyle = "#ddd";
    ctx.fillText(`floor ${hi.toFixed(1)} ${unit}`, 6, 12);
    ctx.fillText(`${lo.toFixed(1)}   ${fmtT(t0)} → ${fmtT(t1)}`, 6, H - 3);
  }
}
