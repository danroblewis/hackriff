// Centre overlays (T-152; ADR-0013 §4.3, §5): pure placement and formatting for the live view's DOM
// overlays. Brackets come from `/api/inventory` rows (`f_lo_hz`, `f_hi_hz`, `state`) exactly as
// served; nothing here detects or measures a signal. Pixel↔Hz is axis.ts; clamping and the narrow
// label rule are presentation.
//
// DC notch (API GAP 10, closed by T-167): the spectrum stream header now carries its own
// `dc_excluded_hz` half-width, preferred whenever present. A server that doesn't send it (an older
// build, or a stream with no DC mask applied) falls back to `GET /api/observations`
// `records[].window.dc_excluded` for the current tune when the server has an observation log, and
// finally to the documented ±15 kHz default, labelled "assumed". Only the fallback default is ever
// "assumed"; both the header value and the observation-log value are measured/configured, not
// guessed.
import * as ax from "../../axis";
import { nearestEntry } from "../../inspect";
import type { Row } from "../../inventory";
import { validateSelection, type NewSelection, type Selection } from "../../selections";
import { UNOBSERVED_DB } from "../../waterfall";

/** Pointer travel (px) that turns a press into a drag (the old UI's DRAG_PX rule, ADR-0013 §5). */
export const DRAG_PX = 6;
/** Narrowest bracket drawn, as a fraction of the view (so a 1-bin row stays clickable). */
export const MIN_BRACKET_FRAC = 0.0045;
/** A bracket narrower than this (px) hides its label unless focused or hovered. */
export const LABEL_MIN_PX = 64;
/** docs/api.md observation log: the DC notch is ±15 kHz (GAP 10 interim default). */
export const DC_NOTCH_HALF_HZ = 15e3;

const clamp01 = (x: number) => Math.min(1, Math.max(0, x));

export interface Span { leftPct: number; widthPct: number }

/** [loHz, hiHz] placed in a view: clamped to it, at least `minFrac` wide (centred on the clamped
 * extent, kept inside); null when wholly outside. */
export function placeExtent(v: ax.View, loHz: number, hiHz: number, minFrac = 0): Span | null {
  if (!(v.hiHz > v.loHz) || !(hiHz >= loHz) || hiHz < v.loHz || loHz > v.hiHz) return null;
  const l = clamp01(ax.hzToFrac(v, loHz)), r = clamp01(ax.hzToFrac(v, hiHz));
  let w = r - l, left = l;
  if (w < minFrac) {
    w = Math.min(1, minFrac);
    left = Math.min(1 - w, Math.max(0, (l + r) / 2 - w / 2));
  }
  return { leftPct: left * 100, widthPct: w * 100 };
}

export type BracketState = "confirmed" | "candidate";
export interface Bracket extends Span { id: string; state: BracketState; active: boolean; narrow: boolean; label: string }

/** Brackets for the confirmed and candidate rows in view (deleted rows never), the focused one last
 * so it draws on top. `widthPx` is the live view's width, for the narrow-label rule. */
export function bracketLayout(rows: readonly Row[], v: ax.View, widthPx: number, focusedId: string | null): Bracket[] {
  const out: Bracket[] = [];
  for (const r of rows) {
    if (r.state !== "confirmed" && r.state !== "candidate") continue;
    const p = placeExtent(v, r.f_lo_hz, r.f_hi_hz, MIN_BRACKET_FRAC);
    if (!p) continue;
    out.push({
      ...p, id: r.id, state: r.state, active: r.id === focusedId,
      narrow: (p.widthPct / 100) * widthPx < LABEL_MIN_PX, label: ax.fmtMHz(r.f_center_hz, 1e3),
    });
  }
  return out.sort((a, b) => Number(a.active) - Number(b.active));
}

export interface SelBox extends Span { id: string; active: boolean; pending: boolean }

/** Frequency boxes for selections in view; `pending` marks ones created here and not yet listed. */
export function selectionBoxes(list: readonly Pick<Selection, "id" | "f_lo" | "f_hi">[], v: ax.View, focusedId: string | null,
  pending: { has(id: string): boolean } = new Set()): SelBox[] {
  const out: SelBox[] = [];
  for (const s of list) {
    const p = placeExtent(v, s.f_lo, s.f_hi, 0.002);
    if (p) out.push({ ...p, id: s.id, active: s.id === focusedId, pending: pending.has(s.id) });
  }
  return out;
}

export interface DcMask { loHz: number; hiHz: number; assumed: boolean }

/** GAP 10 interim: the documented ±15 kHz notch around the tuned centre. */
export const assumedDc = (g: ax.Geometry): DcMask => ({ loHz: g.centerHz - DC_NOTCH_HALF_HZ, hiHz: g.centerHz + DC_NOTCH_HALF_HZ, assumed: true });

/** The spectrum stream header's own `dc_excluded_hz` half-width (T-167), centred on the tune; null
 * when the header carries none (an older server, or a producer applying no DC mask to this
 * stream), in which case the observation-log query or the assumed default applies instead. */
export function dcFromHeader(hd: { dc_excluded_hz?: unknown }, g: ax.Geometry): DcMask | null {
  const half = hd.dc_excluded_hz;
  if (typeof half !== "number" || !(half > 0)) return null;
  return { loHz: g.centerHz - half, hiHz: g.centerHz + half, assumed: false };
}

/** The observation-log query for the current tune's notch, around the first row time `tS`. */
export function dcQuery(g: ax.Geometry, tS: number): string {
  const f = ax.fullView(g);
  return `/api/observations?f_lo=${Math.max(0, Math.floor(f.loHz))}&f_hi=${Math.ceil(f.hiHz)}&t0=${tS - 120}&t1=${tS + 1}&tier=interactive&limit=1`;
}

/** The first record's `window.dc_excluded` centred on this tune (within its own half width); null
 * when none (sweep records, another tune, or a malformed body). */
export function dcFromObservations(body: unknown, g: ax.Geometry): DcMask | null {
  const recs = (body as { records?: unknown } | null)?.records;
  if (!Array.isArray(recs)) return null;
  for (const rec of recs) {
    const d = (rec as { window?: { dc_excluded?: { lo_hz?: unknown; hi_hz?: unknown } } } | null)?.window?.dc_excluded;
    const lo = d?.lo_hz, hi = d?.hi_hz;
    if (typeof lo !== "number" || typeof hi !== "number" || !(hi > lo)) continue;
    if (Math.abs((lo + hi) / 2 - g.centerHz) > (hi - lo) / 2) continue;
    return { loHz: lo, hiHz: hi, assumed: false };
  }
  return null;
}

/** Texture fraction of the bin under `hz` (what `Waterfall.levelAt` reads). */
export const levelU = (g: ax.Geometry, hz: number) => ax.hzToFrac(ax.fullView(g), ax.snapHz(g, hz));

/** Hover readout: the bin centre under the pointer, the newest row's level there, and the row time
 * when the pointer is over the waterfall. */
export function hoverText(g: ax.Geometry, hz: number, level: number, tS = NaN): string {
  const f = ax.snapHz(g, hz);
  const lv = !Number.isFinite(level) ? "– dBFS/Hz" : level <= UNOBSERVED_DB / 10 ? "not observed" : `${level.toFixed(1)} dBFS/Hz`;
  const time = Number.isFinite(tS) ? ` · ${new Date(tS * 1000).toISOString().slice(11, 23)}Z` : "";
  return `${ax.fmtMHz(f, ax.binWidthHz(g))} MHz · ${lv}${time}`;
}

/** The readout sits left of the crosshair past 60 % of the width. */
export const tipOnLeft = (x: number) => x > 0.6;

export const isDrag = (dxPx: number, dyPx: number) => Math.hypot(dxPx, dyPx) >= DRAG_PX;

/**
 * T-194: whether a completed waterfall drag is in "add" mode — the add-mode toggle (reachable on
 * touch), or Shift held for this drag (desktop). Add mode keeps every earlier selection made here;
 * a plain drag replaces only the one selection this tool itself last made (never one built in add
 * mode, and never anything the user made another way), so casual re-dragging doesn't clutter the
 * selections list while deliberately marking several regions still works.
 */
export const addModeActive = (toggleOn: boolean, shiftKey: boolean): boolean => toggleOn || shiftKey;

/** A pointer position as fractions (0..1) of the live element. */
export interface DragPoint { x: number; y: number }

/** What a drag needs of the waterfall to add a time extent. */
export interface RowClock { specFrac: number; rows: number; timeAt(rowsBack: number): number; rowPeriodS: number }

/** "100.25–100.40 MHz": a default selection name, resolved to a twentieth of its width. */
export function regionName(loHz: number, hiHz: number): string {
  const r = Math.max(1, (hiHz - loHz) / 20);
  return `${ax.fmtMHz(loHz, r)}–${ax.fmtMHz(hiHz, r)} MHz`;
}

/**
 * The selection a drag from `a` to `b` makes: its frequency extent (clamped at 0 Hz), plus a time
 * extent when both ends are on the waterfall and it moved at least DRAG_PX vertically. Null when it
 * has no width or is invalid (`validateSelection`).
 */
export function dragSelection(v: ax.View, a: DragPoint, b: DragPoint, heightPx: number, clock: RowClock | null): NewSelection | null {
  const s = ax.selectionHz(v, clamp01(a.x), clamp01(b.x));
  const lo = Math.max(0, s.loHz);
  if (!(s.hiHz > lo)) return null;
  const out: NewSelection = { name: regionName(lo, s.hiHz), f_lo: lo, f_hi: s.hiHz };
  if (clock) {
    const ha = ax.yHit(a.y, clock.specFrac, clock.rows), hb = ax.yHit(b.y, clock.specFrac, clock.rows);
    if (ha.area === "waterfall" && hb.area === "waterfall" && Math.abs(b.y - a.y) * heightPx >= DRAG_PX) {
      const near = Math.min(ha.rowsBack, hb.rowsBack);
      let far = Math.max(ha.rowsBack, hb.rowsBack);
      while (far > near && !Number.isFinite(clock.timeAt(far))) far--; // below the rows received: the oldest
      const newer = clock.timeAt(near), older = clock.timeAt(far);
      if (Number.isFinite(newer) && Number.isFinite(older)) { out.t_lo = older; out.t_hi = newer + clock.rowPeriodS; }
    }
  }
  return validateSelection(out) ? null : out;
}

/** The drag rectangle (percent of the live element): full height unless it selects time too. */
export function draftBox(a: DragPoint, b: DragPoint, timed: boolean) {
  const l = clamp01(Math.min(a.x, b.x)), r = clamp01(Math.max(a.x, b.x));
  const t = timed ? clamp01(Math.min(a.y, b.y)) : 0, bt = timed ? clamp01(Math.max(a.y, b.y)) : 1;
  return { leftPct: l * 100, widthPct: (r - l) * 100, topPct: t * 100, heightPct: (bt - t) * 100 };
}

export function selectionLabel(s: NewSelection, binWidthHz: number): string {
  const timed = s.t_lo !== undefined && s.t_hi !== undefined ? ` · ${(s.t_hi - s.t_lo).toFixed(2)} s` : "";
  return `${ax.fmtMHz(s.f_lo, binWidthHz)}–${ax.fmtMHz(s.f_hi, binWidthHz)} MHz · ${ax.fmtBandwidth(s.f_hi - s.f_lo)}${timed}`;
}

export type ClickTarget = { kind: "signal"; id: string } | { kind: "selection"; id: string } | null;

/** A click at `hz`: the nearest loaded confirmed/candidate row within `halfWidthHz`
 * (`inspect.nearestEntry`), else the narrowest selection containing it, else nothing. */
export function clickTarget(rows: readonly Row[], sels: readonly Pick<Selection, "id" | "f_lo" | "f_hi">[], hz: number, halfWidthHz: number): ClickTarget {
  const r = nearestEntry(rows.filter((x) => x.state === "confirmed" || x.state === "candidate"), hz, halfWidthHz);
  if (r) return { kind: "signal", id: r.id };
  let best: Pick<Selection, "id" | "f_lo" | "f_hi"> | null = null;
  for (const s of sels) if (hz >= s.f_lo && hz <= s.f_hi && (!best || s.f_hi - s.f_lo < best.f_hi - best.f_lo)) best = s;
  return best ? { kind: "selection", id: best.id } : null;
}

/** "↓ 20 s": how far back the waterfall reaches. */
export function timeScaleText(rows: number, rowPeriodS: number): string {
  const s = rows * rowPeriodS;
  if (!(s > 0) || !Number.isFinite(s)) return "";
  return `↓ ${s < 90 ? `${Math.round(s)} s` : s < 5400 ? `${Math.round(s / 60)} min` : `${(s / 3600).toFixed(1)} h`}`;
}
