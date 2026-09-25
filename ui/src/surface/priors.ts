// T-812 (MAP-12): the band-plan **priors** overlay layer — docs/24 §3f/§7, `GET /api/priors`.
//
// ## What it draws, and what it must never look like
//
// The backend's ranked band-plan allocations for a pane's window, drawn as **suggestions beside the
// measurement**: each allocation's two edges as dashed full-height rules and a dashed bracket along
// the pane's top, with a DOM label `prior #n · <allocation>` whose hover/focus text is the
// backend-rendered `reason`. It is the exploration-first rule rendered: the database is never truth,
// so a prior
//
//  - is a **stroke, never a wash** — it lives in the overlay plane (z 60, on top so it never hides a
//    measurement *behind* it) and `overlay.ts` can draw nothing but strokes, so an allocation can
//    never tint the energy under it or read as occupancy;
//  - is **dashed and labelled "prior"**, never drawn in the solid outline of a detection box, so it
//    cannot be mistaken for something that was measured;
//  - **adds nothing to the inventory** and hides nothing: the layer is additive, off by default, and
//    an unexplained detection stays drawn whatever this layer shows (docs/24 §3f).
//
// ## Thin client
//
// No signal logic here. The ranking, the "does a measured emission lie in it", the off-raster flag
// and every sentence are the backend's (`crates/hk-api/src/priors.rs`); this file builds the request
// for a pane's box, keeps the answer as served, and maps Hz → pixels through the same `toClip` the
// data pass uses. Fetching priors is a `GET` of reference data: it never reaches a device route
// (the spy-client empty-call-list rule), and a pan or zoom only changes which window is asked for.
import type { Box } from "./lattice";
import type { OverlayQuad } from "./minimap";
import type { PaneRect } from "./surface";
import { toClip } from "./surface";

const NS_PER_S = 1e9;

/** One served allocation, exactly as `GET /api/priors` ranked and worded it. */
export interface Prior {
  readonly rank: number;
  readonly id: string;
  readonly fLoHz: number;
  readonly fHiHz: number;
  readonly service: string | null;
  readonly allocation: string;
  readonly source: string;
  /** `cited` / `in-band` / `context` / `no-inventory` — the backend's word for what backs it. */
  readonly support: string;
  /** Signed offset of the furthest off-raster measured emission, Hz; `null` when none is flagged. */
  readonly offRasterHz: number | null;
  readonly reason: string;
  readonly unverified: boolean;
}

/** A pane's priors as last served, and the request that produced them. */
export interface PriorsAnswer {
  readonly path: string;
  readonly rows: readonly Prior[];
  readonly statement: string;
  readonly truncated: boolean;
}

/** How finely a request tracks the pane: 1/64 of its span in frequency, whole minutes in time. A
 * following pane's live edge advancing therefore re-asks once a minute, not once a frame — the
 * ranking is over the window's emitters, which change on that scale, not per row. */
const F_QUANTA = 64;
const T_QUANTUM_S = 60;

/** The request for a pane's window: its box, quantized so a sub-pixel pan re-uses the last answer.
 * `null` for a degenerate box — a request the server would refuse is not sent. */
export function priorsPath(box: Box): string | null {
  const span = box.f1Hz - box.f0Hz;
  if (!(span > 0) || !(box.t1Ns > box.t0Ns) || !Number.isFinite(span)) return null;
  const q = span / F_QUANTA;
  const fLo = Math.max(0, Math.floor(box.f0Hz / q) * q);
  const fHi = Math.max(fLo + q, Math.ceil(box.f1Hz / q) * q);
  const t0 = Math.floor(box.t0Ns / NS_PER_S / T_QUANTUM_S) * T_QUANTUM_S;
  const t1 = Math.max(t0 + T_QUANTUM_S, Math.ceil(box.t1Ns / NS_PER_S / T_QUANTUM_S) * T_QUANTUM_S);
  const n = (x: number) => String(Math.round(x));
  return `/api/priors?f_lo=${n(fLo)}&f_hi=${n(fHi)}&t0=${n(t0)}&t1=${n(t1)}`;
}

const str = (v: unknown, d = ""): string => (typeof v === "string" ? v : d);
const num = (v: unknown): number | null => (typeof v === "number" && Number.isFinite(v) ? v : null);

/** Parse a `GET /api/priors` body defensively: a malformed row is dropped, never guessed at. */
export function parsePriors(path: string, body: unknown): PriorsAnswer | null {
  if (!body || typeof body !== "object") return null;
  const b = body as Record<string, unknown>;
  if (!Array.isArray(b.priors)) return null;
  const rows: Prior[] = [];
  for (const raw of b.priors as unknown[]) {
    if (!raw || typeof raw !== "object") continue;
    const r = raw as Record<string, unknown>;
    const fLo = num(r.f_lo_hz), fHi = num(r.f_hi_hz), rank = num(r.rank);
    if (fLo === null || fHi === null || rank === null || !(fHi > fLo)) continue;
    rows.push({
      rank, id: str(r.id, "allocation"), fLoHz: fLo, fHiHz: fHi,
      service: typeof r.service === "string" ? r.service : null,
      allocation: str(r.allocation), source: str(r.source), support: str(r.support, "context"),
      offRasterHz: num(r.off_raster_hz), reason: str(r.reason), unverified: r.unverified === true,
    });
  }
  rows.sort((a, b) => a.rank - b.rank);
  return { path, rows, statement: str(b.statement), truncated: b.truncated === true };
}

/** The priors ink: the mockup's lavender, dimmed and always dashed — a suggestion's stroke, which
 * the solid outline of a detection box never is. */
export const PRIOR_INK: readonly [number, number, number, number] = [0.639, 0.584, 0.878, 0.55];
/** A prior some measured emission in the window backs (`cited` / `in-band`) is drawn a little
 * stronger than a context-only one — the backend's ranking, made visible, never a new judgement. */
export const PRIOR_BACKED_INK: readonly [number, number, number, number] = [0.639, 0.584, 0.878, 0.8];
const EDGE_PX = 1, BRACKET_PX = 3, DASH_PX = 4, GAP_PX = 5;
/** The bracket is inset this far below the pane's top, device px, in lanes so nested allocations
 * (`ism-433-part15` inside `amateur-70cm`) stay apart. */
const BRACKET_TOP_PX = 4, LANE_PX = 6, LANES = 3;

const backed = (p: Prior) => p.support === "cited" || p.support === "in-band";

/**
 * The strokes of every prior in one pane, through the pane's own box — so a pan or a zoom moves
 * them with the rows. An edge outside the pane draws **nothing** (an edge pinned to the pane border
 * would claim an allocation boundary where there is none); the bracket spans only the visible part.
 */
export function priorQuads(rows: readonly Prior[], paneBox: Box, rect: PaneRect): OverlayQuad[] {
  if (!(paneBox.f1Hz > paneBox.f0Hz) || !(rect.w > 0) || !(rect.h > 0)) return [];
  const out: OverlayQuad[] = [];
  const pxX = 2 / rect.w, pxY = 2 / rect.h;
  const xOf = (f: number) => toClip({ f0Hz: f, f1Hz: f, t0Ns: paneBox.t0Ns, t1Ns: paneBox.t0Ns }, paneBox)[0];
  rows.forEach((p, i) => {
    const rgba = backed(p) ? PRIOR_BACKED_INK : PRIOR_INK;
    const id = `prior:${p.id}`;
    for (const f of [p.fLoHz, p.fHiHz]) {
      if (f < paneBox.f0Hz || f > paneBox.f1Hz) continue;
      const x = xOf(f);
      const x0 = Math.max(-1, Math.min(1 - EDGE_PX * pxX, x - (EDGE_PX * pxX) / 2));
      for (let y = 0; y < rect.h; y += DASH_PX + GAP_PX) {
        const y1 = 1 - y * pxY, y0 = 1 - Math.min(rect.h, y + DASH_PX) * pxY;
        out.push({ clip: [x0, y0, x0 + EDGE_PX * pxX, y1], rgba, kind: "prior-band", id });
      }
    }
    const lo = Math.max(p.fLoHz, paneBox.f0Hz), hi = Math.min(p.fHiHz, paneBox.f1Hz);
    if (!(hi > lo)) return;
    const [bx0, bx1] = [xOf(lo), xOf(hi)];
    const top = 1 - (BRACKET_TOP_PX + (i % LANES) * LANE_PX) * pxY;
    const bot = top - BRACKET_PX * pxY;
    for (let x = bx0; x < bx1; x += (DASH_PX + GAP_PX) * pxX * 2) {
      out.push({ clip: [x, bot, Math.min(bx1, x + DASH_PX * pxX * 2), top], rgba, kind: "prior-band", id });
    }
  });
  return out;
}

/** One prior's DOM label, CSS px from the canvas's top-left. */
export interface PriorLabel {
  readonly paneId: string;
  readonly x: number;
  readonly y: number;
  /** `prior #1 · fm-broadcast` — always says "prior", so it can never read as a detection. */
  readonly text: string;
  /** Service, allocation and the off-raster flag, all as served. */
  readonly sub: string;
  /** The backend's ranked reasoning, shown on hover and read by assistive tech. */
  readonly reason: string;
  readonly offRaster: boolean;
}

/** Labels at most this many priors per pane (the strokes still draw every one), and not a band
 * narrower than this on screen — a label wider than its band would name a place it is not. */
export const MAX_PRIOR_LABELS = 6;
const MIN_LABEL_BAND_CSS = 48;
const LABEL_TOP_CSS = 14, LABEL_LANE_CSS = 30;

function fmtKhz(hz: number): string {
  return `${(Math.abs(hz) / 1e3).toFixed(1)} kHz`;
}

/** The labels of one pane's priors, placed at the centre of each band's visible part. */
export function priorLabels(
  paneId: string, rows: readonly Prior[], paneBox: Box, rect: PaneRect, canvasHpx: number, dpr = 1,
): PriorLabel[] {
  const k = dpr > 0 ? dpr : 1;
  const span = paneBox.f1Hz - paneBox.f0Hz;
  if (!(span > 0)) return [];
  const left = rect.x / k, top = (canvasHpx - (rect.y + rect.h)) / k, w = rect.w / k;
  const out: PriorLabel[] = [];
  for (const [i, p] of rows.entries()) {
    if (out.length >= MAX_PRIOR_LABELS) break;
    const lo = Math.max(p.fLoHz, paneBox.f0Hz), hi = Math.min(p.fHiHz, paneBox.f1Hz);
    if (!(hi > lo)) continue;
    const x0 = ((lo - paneBox.f0Hz) / span) * w, x1 = ((hi - paneBox.f0Hz) / span) * w;
    if (x1 - x0 < MIN_LABEL_BAND_CSS) continue;
    const parts = [p.service ?? "no primary service", p.allocation].filter((s) => s !== "");
    if (p.offRasterHz !== null) parts.push(`⚑ ${fmtKhz(p.offRasterHz)} off raster — flagged, not snapped`);
    else if (p.support === "context") parts.push("context only");
    out.push({
      paneId, x: left + (x0 + x1) / 2, y: top + LABEL_TOP_CSS + (i % LANES) * LABEL_LANE_CSS,
      text: `prior #${p.rank} · ${p.id}`, sub: parts.join(" · "), reason: p.reason,
      offRaster: p.offRasterHz !== null,
    });
  }
  return out;
}

/**
 * The priors' DOM labels (band 2, over the canvas). Re-laid-out inside the render frame from the
 * frame's own `PaneView`, like the HUD rulers — never on a poll, which would be T-388 again. Pooled
 * per pane so a 60 Hz frame reuses nodes.
 */
export class PriorLabelLayer {
  private readonly pools = new Map<string, HTMLElement[]>();

  constructor(private readonly root: HTMLElement) {}

  update(paneId: string, labels: readonly PriorLabel[]): void {
    const doc = this.root.ownerDocument;
    let pool = this.pools.get(paneId);
    if (!pool) { pool = []; this.pools.set(paneId, pool); }
    while (pool.length < labels.length) {
      const el = doc.createElement("div");
      el.className = "sf-prior-label";
      el.tabIndex = 0;
      el.appendChild(doc.createElement("b"));
      el.appendChild(doc.createElement("span"));
      this.root.appendChild(el);
      pool.push(el);
    }
    for (let i = 0; i < pool.length; i++) {
      const el = pool[i];
      const l = labels[i];
      if (!l) { if (!el.hidden) el.hidden = true; continue; }
      if (el.hidden) el.hidden = false;
      el.classList.toggle("off-raster", l.offRaster);
      const b = el.firstChild as HTMLElement, s = el.lastChild as HTMLElement;
      if (b.textContent !== l.text) b.textContent = l.text;
      if (s.textContent !== l.sub) s.textContent = l.sub;
      if (el.title !== l.reason) { el.title = l.reason; el.setAttribute("aria-label", `${l.text}. ${l.reason}`); }
      el.style.transform = `translate(${l.x.toFixed(1)}px, ${l.y.toFixed(1)}px) translateX(-50%)`;
    }
  }

  /** Drop the labels of panes that no longer exist. */
  retain(ids: ReadonlySet<string>): void {
    for (const [id, pool] of this.pools) {
      if (ids.has(id)) continue;
      for (const el of pool) el.remove();
      this.pools.delete(id);
    }
  }
}
