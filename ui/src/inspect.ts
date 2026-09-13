// Click-to-inspect (T-044): the inventory emitter nearest a clicked frequency. Data comes only from
// the gated /api/inventory: an identity value arrives only when the server returns it in clear, a
// withheld identity shows as withheld, and nothing else is fetched. Text goes in via textContent.
import { fmtBandwidth } from "./axis";
import { fmtT } from "./history";
import type { Row } from "./inventory";
import type { Api } from "./main";

const $ = <T extends HTMLElement>(id: string) => document.getElementById(id) as T;

type Extent = { f_lo_hz: number; f_hi_hz: number; bandwidth_hz: number; last_seen_s: number };

/** Distance from `hz` to an emitter's frequency extent (0 inside it). */
export const extentDistanceHz = (r: { f_lo_hz: number; f_hi_hz: number }, hz: number) =>
  hz < r.f_lo_hz ? r.f_lo_hz - hz : hz > r.f_hi_hz ? hz - r.f_hi_hz : 0;

/** The row nearest `hz` within `maxHz` (ties: narrower, then most recently seen); null if none. */
export function nearestEntry<T extends Extent>(rows: readonly T[], hz: number, maxHz: number): T | null {
  let best: T | null = null, bestD = Infinity;
  for (const r of rows) {
    const d = extentDistanceHz(r, hz);
    if (d > maxHz) continue;
    const better = d < bestD || (d === bestD && best !== null &&
      (r.bandwidth_hz < best.bandwidth_hz || (r.bandwidth_hz === best.bandwidth_hz && r.last_seen_s > best.last_seen_s)));
    if (better) { best = r; bestD = d; }
  }
  return best;
}

/** Search half-width around a click: 4 bins, 0.5 % of the view, or 5 kHz, whichever is widest. */
export const inspectHalfWidthHz = (viewSpanHz: number, binWidthHz: number) => Math.max(4 * binWidthHz, 0.005 * viewSpanHz, 5e3);

function note(text: string): HTMLElement {
  const p = document.createElement("p");
  p.className = "hint";
  p.textContent = text;
  return p;
}

function details(r: Row, hz: number): HTMLElement {
  const dl = document.createElement("dl");
  const item = (term: string, value: string | Node) => {
    const dt = document.createElement("dt"), dd = document.createElement("dd");
    dt.textContent = term;
    dd.append(value);
    dl.append(dt, dd);
    return dd;
  };
  const badge = document.createElement("span");
  badge.className = `badge st-${r.known_status}`;
  badge.textContent = r.known_status === "unexpected-here" ? "unexpected here" : r.known_status;
  const st = item("status", badge);
  const s = r.status;
  if (s) st.append(` ${s.author}: ${s.reason_withheld ? "reason withheld" : `${s.reason ?? ""}${s.prior_ref ? ` (${s.prior_ref})` : ""}`}`);
  item("centre", `${(r.f_center_hz / 1e6).toFixed(6)} MHz`);
  item("bandwidth", fmtBandwidth(r.bandwidth_hz));
  item("extent", `${(r.f_lo_hz / 1e6).toFixed(6)}–${(r.f_hi_hz / 1e6).toFixed(6)} MHz`);
  const d = extentDistanceHz(r, hz);
  if (d > 0) item("clicked", `${fmtBandwidth(d)} outside its extent`);
  item("family", r.family ?? "—");
  const id = document.createElement("span");
  if (r.identity_value !== undefined) id.textContent = `${r.identity_scheme}: ${r.identity_value}`;
  else if (r.withheld) {
    const w = document.createElement("span");
    w.className = "withheld";
    w.textContent = "withheld";
    id.append(`${r.identity_scheme}: `, w);
  } else id.textContent = "—";
  item("identity", id);
  item("first seen", `${fmtT(r.first_seen_s)} UTC`);
  item("last seen", `${fmtT(r.last_seen_s)} UTC`);
  item("count", String(r.count));
  item("tags", `${r.tags.join(", ") || "—"}${r.tags_withheld ? " (others withheld)" : ""}`);
  item("emitter", r.id);
  return dl;
}

/** The inspect panel. */
export class Inspector {
  private seq = 0;

  constructor(private api: Api) {
    $("inspect-close").addEventListener("click", () => { $("inspect").hidden = true; });
  }

  /** Looks up the emitter nearest `hz` (within ±halfWidthHz) and shows it. */
  async show(hz: number, halfWidthHz: number, levelDb: number, t: number) {
    const seq = ++this.seq;
    $("inspect").hidden = false;
    $("inspect-at").textContent = `${(hz / 1e6).toFixed(6)} MHz` +
      (Number.isFinite(levelDb) ? ` · ${levelDb.toFixed(1)} dBFS/Hz now` : "") +
      (Number.isFinite(t) ? ` · row ${new Date(t * 1000).toISOString().slice(11, 23)}Z` : "");
    const body = $("inspect-body");
    body.replaceChildren(note("looking up the inventory…"));
    const lo = Math.max(0, hz - halfWidthHz), hi = hz + halfWidthHz;
    try {
      const page = (await this.api(`/api/inventory?f_lo=${lo}&f_hi=${hi}&limit=50`)) as { entries: Row[] };
      if (seq !== this.seq) return;
      const r = nearestEntry(page.entries, hz, halfWidthHz);
      body.replaceChildren(r ? details(r, hz) : note(`No inventory emitter within ±${fmtBandwidth(halfWidthHz)} of this frequency.`));
    } catch (e) {
      if (seq !== this.seq) return;
      body.replaceChildren(note(`inventory: ${(e as Error).message}`));
    }
  }
}
