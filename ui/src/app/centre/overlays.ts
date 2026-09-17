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
import type { BoxStyle, TimeBox } from "../../timebox";
import { UNOBSERVED_DB } from "../../waterfall";

/** Pointer travel (px) that turns a press into a drag (the old UI's DRAG_PX rule, ADR-0013 §5). */
export const DRAG_PX = 6;
/** Narrowest bracket drawn, as a fraction of the view (so a 1-bin row stays clickable). */
export const MIN_BRACKET_FRAC = 0.0045;
/** A bracket narrower than this (px) hides its label unless focused or hovered. */
export const LABEL_MIN_PX = 64;
/** docs/api.md observation log: the DC notch is ±15 kHz (GAP 10 interim default). */
export const DC_NOTCH_HALF_HZ = 15e3;
/** Confirmed-band edge drag: a few px either side of the drawn edge take priority over
 * region-select/click (T-193). */
export const EDGE_HIT_PX = 6;

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

export type BracketState = "candidate" | "confirmed";
export interface Bracket extends Span { id: string; state: BracketState; active: boolean; narrow: boolean; label: string }

/**
 * Brackets for the Candidate **and Confirmed** rows in view (deleted rows never), the focused one
 * last so it draws on top. `widthPx` is the live view's width, for the narrow-label rule.
 *
 * Confirmed is back here (T-389). T-193 replaced the confirmed bracket with the full-height
 * [[confirmedBands]] box; T-261 then narrowed that box to the **focused** row and gave every other
 * one the waterfall's time-extent presence box. Between them an *unselected* Confirmed row was left
 * with no marker in the spectrum pane at all — which is the user's report exactly: "the confirmed
 * box appears only after clicking the row". The presence box is a waterfall overlay and, for a
 * station that has been on the air for minutes, it spans the whole pane, so nothing box-shaped is
 * visible there either. A bracket costs nothing (`.bk.confirmed` is already styled teal in
 * centre.css and was dead code), it is the same affordance candidates get, and it makes an
 * unselected confirmed signal clickable on the spectrum. Edge-dragging is untouched: it resolves
 * against [[confirmedBands]], which the caller still feeds the focused row alone.
 */
export function bracketLayout(rows: readonly Row[], v: ax.View, widthPx: number, focusedId: string | null): Bracket[] {
  const out: Bracket[] = [];
  for (const r of rows) {
    if (r.state !== "candidate" && r.state !== "confirmed") continue;
    const p = placeExtent(v, r.f_lo_hz, r.f_hi_hz, MIN_BRACKET_FRAC);
    if (!p) continue;
    out.push({
      ...p, id: r.id, state: r.state, active: r.id === focusedId,
      narrow: (p.widthPct / 100) * widthPx < LABEL_MIN_PX, label: ax.fmtMHz(r.f_center_hz, 1e3),
    });
  }
  return out.sort((a, b) => Number(a.active) - Number(b.active));
}

// ---- Confirmed-signal band boxes (T-193): span both the spectrum trace and the waterfall, drawn
// in a full-height layer over both panes; draggable left/right edges set a user band override
// (docs/api.md `user_band`, T-191) via `explore/inventory.ts`'s `setUserBand`/`clearUserBand`. ----

/** The band a Confirmed row draws: the user override's edges when set, else the measured extent.
 * The measured extent (`f_lo_hz`/`f_hi_hz`) is never overwritten by the override (docs/api.md). */
export function effectiveBand(r: Pick<Row, "user_band" | "f_lo_hz" | "f_hi_hz">): { loHz: number; hiHz: number; hasUserBand: boolean } {
  const u = r.user_band;
  return u ? { loHz: u.f_lo, hiHz: u.f_hi, hasUserBand: true } : { loHz: r.f_lo_hz, hiHz: r.f_hi_hz, hasUserBand: false };
}

export interface ConfirmedBand extends Span {
  id: string; active: boolean; hasUserBand: boolean; label: string;
  /** Measured-edge tick position (percent of the view), only when a user band has moved that edge
   * off the measured one and the measured edge is still in view; null otherwise. */
  measuredLeftPct: number | null; measuredRightPct: number | null;
}

/** One box per Confirmed row in view (deleted, candidate and other states never), the focused one
 * last so it draws on top — same stacking rule as [[bracketLayout]]. `overrides` substitutes a
 * row's band with an in-progress drag's value (T-193, so the box tracks the pointer before the
 * `PUT` commits), forcing `hasUserBand` since a drag always edits the user band. */
export function confirmedBands(rows: readonly Row[], v: ax.View, focusedId: string | null,
  overrides: ReadonlyMap<string, { loHz: number; hiHz: number }> = new Map()): ConfirmedBand[] {
  const out: ConfirmedBand[] = [];
  for (const r of rows) {
    if (r.state !== "confirmed") continue;
    const ov = overrides.get(r.id);
    const eff = ov ? { ...ov, hasUserBand: true } : effectiveBand(r);
    const p = placeExtent(v, eff.loHz, eff.hiHz, MIN_BRACKET_FRAC);
    if (!p) continue;
    let measuredLeftPct: number | null = null, measuredRightPct: number | null = null;
    if (eff.hasUserBand) {
      if (eff.loHz !== r.f_lo_hz && r.f_lo_hz >= v.loHz && r.f_lo_hz <= v.hiHz) measuredLeftPct = ax.hzToFrac(v, r.f_lo_hz) * 100;
      if (eff.hiHz !== r.f_hi_hz && r.f_hi_hz >= v.loHz && r.f_hi_hz <= v.hiHz) measuredRightPct = ax.hzToFrac(v, r.f_hi_hz) * 100;
    }
    out.push({ ...p, id: r.id, active: r.id === focusedId, hasUserBand: eff.hasUserBand, measuredLeftPct, measuredRightPct, label: ax.fmtMHz((eff.loHz + eff.hiHz) / 2, 1e3) });
  }
  return out.sort((a, b) => Number(a.active) - Number(b.active));
}

export type BandEdge = "lo" | "hi";

/** Which edge (if any) of a drawn box is within `hitPx` of pointer position `xPx` (client px from
 * the live element's left edge). Null elsewhere, leaving priority to region-select/click there
 * (T-193 "priority over region select only on the edge hit zone"). */
export function bandEdgeHit(xPx: number, box: Span, widthPx: number, hitPx = EDGE_HIT_PX): BandEdge | null {
  const loPx = (box.leftPct / 100) * widthPx, hiPx = ((box.leftPct + box.widthPct) / 100) * widthPx;
  if (Math.abs(xPx - loPx) <= hitPx) return "lo";
  if (Math.abs(xPx - hiPx) <= hitPx) return "hi";
  return null;
}

export interface BandEdgeTarget { id: string; edge: BandEdge }

/** The topmost (focused-first) Confirmed band whose edge sits under pointer position `xPx`; null
 * when none does. Combines [[confirmedBands]] and [[bandEdgeHit]] for the pointerdown handler. */
export function confirmedEdgeAt(rows: readonly Row[], v: ax.View, widthPx: number, xPx: number, focusedId: string | null,
  overrides?: ReadonlyMap<string, { loHz: number; hiHz: number }>): BandEdgeTarget | null {
  const bands = confirmedBands(rows, v, focusedId, overrides);
  for (let i = bands.length - 1; i >= 0; i--) {
    const hit = bandEdgeHit(xPx, bands[i], widthPx);
    if (hit) return { id: bands[i].id, edge: hit };
  }
  return null;
}

/** The narrowest band a drag may leave: a few pixels' worth of the current view, so an edge drag
 * can't collapse the band to nothing ("pixel snapping and a min width", T-193). Arithmetic over the
 * already-known view/width, not a signal decision. */
export function minUserBandHz(v: ax.View, widthPx: number, minPx = 4): number {
  if (!(widthPx > 0) || !(v.hiHz > v.loHz)) return 1;
  return Math.max(1, (minPx * (v.hiHz - v.loHz)) / widthPx);
}

/** Rounds a horizontal view fraction to the nearest device pixel of a `widthPx`-wide element
 * ("pixel snapping"). */
export function snapFracToPixel(frac: number, widthPx: number): number {
  return widthPx > 0 ? Math.round(frac * widthPx) / widthPx : frac;
}

/** The band a drag of `edge` to pointer fraction `xFrac` produces: the moved edge snapped to a
 * pixel and clamped so the band stays at least [[minUserBandHz]] wide and never crosses 0 Hz; the
 * other edge is untouched. */
export function dragBandEdge(v: ax.View, widthPx: number, edge: BandEdge, startLoHz: number, startHiHz: number, xFrac: number): { loHz: number; hiHz: number } {
  const snappedHz = ax.fracToHz(v, snapFracToPixel(clamp01(xFrac), widthPx));
  const minW = minUserBandHz(v, widthPx);
  return edge === "lo"
    ? { loHz: Math.max(0, Math.min(snappedHz, startHiHz - minW)), hiHz: startHiHz }
    : { loHz: startLoHz, hiHz: Math.max(startLoHz + minW, snappedHz) };
}

// ---- Time-extent boxes (T-261, ADR-0017 TM-4; T-362) ---------------------------------------
//
// A box is a frequency extent × a capture-time extent, and **that is all it is**: these producers
// emit `TimeBox`es carrying a band fraction and two absolute times, with no screen coordinate
// anywhere. The waterfall places and draws them in its own render pass, every animation frame
// (ui/src/timebox.ts, ui/src/waterfall.ts `drawBoxes`).
//
// T-362 is why. T-337 had already made placement a pure function of capture time, but these
// functions returned *percentages* and were called on the ~1 s inventory poll, so between polls the
// rows scrolled under a box that stood still and the poll made it jump. Returning a region instead
// of a rectangle removes the second clock: there is no percentage to go stale, and a poll that
// changes nothing changes nothing on screen.
//
// Growth still needs no animation. An open interval's `t_end_s` moves closer to the live edge each
// poll while `t_start_s` stays fixed; the mapping does the rest. Nothing here extrapolates
// `t_end_s` between polls — the extent is exactly what `/api/inventory` last served (thin-client
// rule); what moves between polls is the *rows*, and the box moves with them because it is placed
// with them. The focused row is excluded: it keeps the full-height bracket/[[confirmedBands]] box
// so T-193's drag-to-adjust-band edges stay where that code expects them (ADR-0017 TM-4 table).

/** Band fraction (0..1 over the bins in order) of `hz` — the coordinate the waterfall's zoom window
 * and its boxes share, so one frequency mapping serves the rows and everything drawn on them. */
export const bandFrac = (g: ax.Geometry, hz: number) => ax.hzToFrac(ax.fullView(g), hz);

/** Overlay colours, RGB 0..1. The waterfall canvas keeps its own always-dark colormap (centre.css),
 * so these are the dark-theme `--teal` / `--lav` / `--amber` tokens the DOM overlays use. */
const TEAL = [0.322, 0.761, 0.682] as const;
const LAV = [0.639, 0.584, 0.878] as const;
const AMBER = [0.941, 0.647, 0.259] as const;
const rgba = (c: readonly [number, number, number], a: number) => [c[0], c[1], c[2], a] as const;

/**
 * The style a Candidate/Confirmed presence box draws with: `open` (still on the air) marks the
 * newest edge amber and thickens it; `chirp` hatches the fill, since ADR-0017 §1.3 names a swept
 * carrier drawn as its bounding box as an approximation, not the truth. Presentation only.
 *
 * T-410 (ADR-0019 §1) makes `open` load-bearing rather than decorative: an open interval's box runs
 * to the **live edge**, so the amber edge it marks is now the live edge itself, and the span
 * between the last measured end and it is the open cap (`ui/src/timebox.ts`, drawn lighter with a
 * rule where measurement stops). The colours here are unchanged; what changed is what the top edge
 * of an open box *is*.
 *
 * Confirmed is drawn **heavier** than Candidate (T-389). It used to be the lighter of the two: a
 * 1 px solid border at 10 % fill against a candidate's 1 px *dashed* border, and dashes read as a
 * box where two thin lines read as nothing — which is the wrong way round, since a confirmed
 * emitter is the stronger claim. It matters most for exactly the case the user hit: a station on
 * the air for minutes has a presence interval longer than the waterfall holds, so its top and
 * bottom edges are off-pane and the vertical border is all there is to see.
 */
export function presenceStyle(state: "candidate" | "confirmed", open: boolean, chirp: boolean): BoxStyle {
  const confirmed = state === "confirmed";
  const c = confirmed ? TEAL : LAV;
  return {
    fill: rgba(c, confirmed ? 0.18 : 0.08), border: rgba(c, 1),
    top: open ? rgba(AMBER, 1) : rgba(c, 1), borderPx: confirmed ? 2 : 1,
    topPx: open ? (confirmed ? 3 : 2) : (confirmed ? 2 : 1),
    dashPx: confirmed ? 0 : 4, hatch: chirp,
  };
}

export interface PresenceBox extends TimeBox {
  state: "candidate" | "confirmed";
  /** An interval open at the window's live edge: still on the air (docs/api.md `presence.liveness`). */
  open: boolean;
  /** The row's arbitrated family is Costas/chirp spread spectrum (`family === "css"`, docs/07 §2.21
   * taxonomy `css` {chirp}) — the one case ADR-0017 §1.3 names where this rectangle is known to
   * misrepresent the signal (a swept carrier drawn as its bounding box, not the diagonal truth). */
  chirp: boolean;
  label: string;
}

/** One box per Candidate/Confirmed row whose `presence.last_interval` exists (docs/api.md
 * `presence`, T-284): frequency from `f_lo_hz`/`f_hi_hz`, time extent the interval's own
 * `t_start_s`/`t_end_s` — both read off the API response, never recomputed or extrapolated. A row
 * with no interval (`last_interval: null`, or no `presence` at all on a pre-T-284 fixture) yields
 * nothing: no zero-width or zero-duration box is ever fabricated. Whether a box is *on screen* is
 * decided where it is drawn, against the rows actually held — one whose span has scrolled off draws
 * nothing rather than a rectangle clamped to a duration it never had.
 *
 * T-410 (ADR-0019): an **open** interval's box is `openEnded`, so it runs to the live edge rather
 * than stopping at `t_end_s`. Nothing here computes that edge — the render pass places it at the
 * newest row it is drawing, which is the only place that knows it per frame (T-362) — and
 * `t_end_s` stays exactly as served, now as the boundary between what was measured and the open
 * cap above it. The one claim this file makes about unmeasured air is drawn as a claim. */
export function presenceBoxes(rows: readonly Row[], g: ax.Geometry, focusedId: string | null): PresenceBox[] {
  const out: PresenceBox[] = [];
  for (const r of rows) {
    if (r.state !== "candidate" && r.state !== "confirmed") continue;
    if (r.id === focusedId) continue; // keeps its full-height bracket/band instead (T-193)
    const iv = r.presence?.last_interval;
    if (!iv) continue;
    const label = ax.fmtMHz(r.f_center_hz, 1e3), chirp = r.family === "css";
    out.push({
      id: r.id, state: r.state, u0: bandFrac(g, r.f_lo_hz), u1: bandFrac(g, r.f_hi_hz),
      tLo: iv.t_start_s, tHi: iv.t_end_s, openEnded: iv.open, open: iv.open, chirp, label,
      style: presenceStyle(r.state, iv.open, chirp),
      title: `${label} MHz · ${r.state}${iv.open ? " · on air (open to the live edge; measured to the rule)" : ""}${chirp ? " · bounding box (chirp: a swept carrier drawn as its extent, not its sweep)" : ""}`,
    });
  }
  return out;
}

/** The style a timed selection box draws with (amber, dashed; solid and filled when focused). */
export const selectionStyle = (active: boolean): BoxStyle => ({
  fill: rgba(AMBER, active ? 0.14 : 0), border: rgba(AMBER, active ? 1 : 0.6),
  top: rgba(AMBER, active ? 1 : 0.6), borderPx: 1, topPx: 1, dashPx: 3, hatch: false,
});

/**
 * Boxes for the selections that carry a time extent (`t_lo`/`t_hi`, absolute capture time from
 * `GET /api/selections` — the same times [[dragSelection]] read off the rows when it was drawn).
 * They are drawn in the waterfall's own pass beside the rows they selected (T-362); a
 * frequency-only selection has no time axis to sit on and stays a full-height DOM box
 * ([[selectionBoxes]]) — the honest rendering of "any time".
 */
export function selectionTimeBoxes(list: readonly Pick<Selection, "id" | "f_lo" | "f_hi" | "t_lo" | "t_hi" | "name">[],
  g: ax.Geometry, focusedId: string | null): TimeBox[] {
  const out: TimeBox[] = [];
  for (const s of list) {
    if (typeof s.t_lo !== "number" || typeof s.t_hi !== "number") continue;
    out.push({
      id: s.id, u0: bandFrac(g, s.f_lo), u1: bandFrac(g, s.f_hi), tLo: s.t_lo, tHi: s.t_hi,
      style: selectionStyle(s.id === focusedId), title: `${s.name ?? regionName(s.f_lo, s.f_hi)} · selection`,
    });
  }
  return out;
}

export interface SelBox extends Span {
  id: string; active: boolean; pending: boolean;
}

/**
 * Full-height boxes for the **untimed** selections in view; `pending` marks ones created here and
 * not yet listed. A selection carrying a time extent is not here at all — it is a time-varying
 * overlay and belongs in the render pass ([[selectionTimeBoxes]]), not in a DOM layer with its own
 * idea of where it is (T-362).
 */
export function selectionBoxes(list: readonly Pick<Selection, "id" | "f_lo" | "f_hi" | "t_lo" | "t_hi">[], v: ax.View, focusedId: string | null,
  pending: { has(id: string): boolean } = new Set()): SelBox[] {
  const out: SelBox[] = [];
  for (const s of list) {
    if (typeof s.t_lo === "number" && typeof s.t_hi === "number") continue;
    const p = placeExtent(v, s.f_lo, s.f_hi, 0.002);
    if (!p) continue;
    out.push({ ...p, id: s.id, active: s.id === focusedId, pending: pending.has(s.id) });
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

/**
 * The waterfall's own time axis — **the one canonical mapping** every time-varying overlay is laid
 * out through (T-337, the user's "one shared time axis" invariant). `timeAt` reads a row's absolute
 * capture time (the backend's own per-record timestamp, `hk-stream` binary record header offset 16,
 * or a review grid's `t0_s + k·t_cell_s`); `rowsBackAt` is its exact inverse. Placement must use
 * these and nothing else, so an overlay sits on the energy it describes.
 *
 * `rowPeriodS` is the **declared** row period (`1 / sample_rate_hz` of the spectrum header, or the
 * review grid's `t_cell_s`). It is a *duration* — how long one row stands for — used to label the
 * time scale and to close the newest row's half-open interval. It is **not** a placement mapping:
 * the declared rate describes row production, not the rows on screen, and on a gated stream it is
 * deliberately up to 10 % above the actual row rate (`hk_pipeline::class::RowPlan::declared_hz`),
 * while gated rows, dropped runs and backlog-skipped frames advance capture time without advancing
 * the ring. Placing an overlay by it drifts against the rows, linearly with age.
 */
export interface RowClock {
  specFrac: number;
  rows: number;
  timeAt(rowsBack: number): number;
  rowsBackAt(tS: number): number;
  rowPeriodS: number;
}

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
      // `t_hi` closes the newest selected row's half-open interval, and it must close it at the
      // *next* row's own capture time where there is one (T-337): then drawing the selection back
      // through `rowsBackAt` lands on exactly the rows that were dragged over — the round trip is
      // the identity. Only past the live edge, where there is no next row, does the declared row
      // period stand in for the row's duration.
      const next = near > 0 ? clock.timeAt(near - 1) : NaN;
      if (Number.isFinite(newer) && Number.isFinite(older)) {
        out.t_lo = older;
        out.t_hi = Number.isFinite(next) ? next : newer + clock.rowPeriodS;
      }
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

/**
 * "↓ 20 s": how far back the waterfall reaches — **measured** off the rows on screen (the newest
 * row's capture time minus the oldest's, plus the oldest row's own duration) whenever a clock is
 * available, so the label describes the same axis the boxes are placed on (T-337). `rows ×
 * rowPeriodS` is only the fallback before any row has arrived: it is what the axis *would* span if
 * every declared row arrived, which is exactly the assumption the placement mapping no longer makes.
 */
export function timeScaleText(rows: number, rowPeriodS: number, clock?: Pick<RowClock, "timeAt"> | null): string {
  let s = rows * rowPeriodS;
  if (clock) {
    const newest = clock.timeAt(0);
    let oldest = NaN, k = 0;
    for (; k < rows; k++) { const t = clock.timeAt(k); if (!Number.isFinite(t)) break; oldest = t; }
    if (Number.isFinite(newest) && Number.isFinite(oldest) && k > 1) s = newest - oldest + rowPeriodS;
  }
  if (!(s > 0) || !Number.isFinite(s)) return "";
  return `↓ ${s < 90 ? `${Math.round(s)} s` : s < 5400 ? `${Math.round(s / 60)} min` : `${(s / 3600).toFixed(1)} h`}`;
}
