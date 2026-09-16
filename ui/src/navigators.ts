// Edge-navigator arithmetic (T-340). Pure: no DOM, no client, no measurement. Unit-tested in
// ui/test/navigators.test.ts.
//
// **The user's invariant** (CLAUDE.md, "Time, the waterfall, and the live view"), as corrected in
// T-367 — *the two bars control DIFFERENT axes*:
//
// > Each waterfall axis has an edge navigator parallel to it, and the two control DIFFERENT axes.
// > Time runs down the waterfall, frequency across it. The **left vertical bar is the TIME
// > navigator**: it selects the time range and shows a compressed history waterfall **of the
// > currently-selected frequency range only** (not the whole spectrum) — it never changes
// > frequency. The **bottom horizontal bar is the FREQUENCY navigator**: it sets the centre and
// > span (the "survey" across the whole device-available spectrum) and shows the most-recent
// > sample/occupancy across that range — it never scrubs time. Each pans and zooms only its own
// > axis; a dragged region on either zooms the main view along that axis. **(Wiring both bars to
// > scrub time is a bug: the vertical is time-for-this-frequency-range, the horizontal is
// > frequency-across-the-survey.)** The frequency navigator shows every currently-active capture
// > window as a lit segment — the natural home for **multiple SDRs** and for survey/sweep coverage.
//
// **The split, from the user:** *"Backend reports the achievable (centre, span) grid +
// full-spectrum survey overview; UI does the navigators/gestures/snap/styling."* So this module is
// the same class of work as `axis.ts` and `navigation.ts`: placing a range on a bar, reading a
// pointer fraction back out, and moving a range inside bounds. Which centres and spans exist is
// `GET /api/navigation`; which windows are active is the same route's `windows` list; what the
// retained capture window spans is `GET /api/timeline`. Nothing here decides any of those.
//
// Three rules this file must not break:
//
//  1. **The lit segments come from the reported list.** `activeWindows` reads the `windows` array
//     and nothing else — never `frequency.current`, which is one device's tuned state and not an
//     enumeration. A body with a `current` but no `windows` yields **no** segments, because a list
//     invented from a singleton would show one window on a system that never said how many it has.
//  2. **The time navigator's extent is the capture window.** It comes from `GET /api/timeline`'s
//     `window` (the IQ ring's configured retention), never from the spectrum-history horizon —
//     `/api/navigation`'s `time.latest_s` and `tiers[].max_age_s` are a different, longer horizon
//     and are not read here at all.
//  3. **A navigator gesture never moves the radio.** Everything below returns view ranges and
//     placements. Setting the centre is a device action and lives in `app/centre/view.ts`
//     (`applyDeviceAction`); a pan that runs past the tuned band's edge produces an *offer*, which
//     is a store write, exactly as T-343 settled for the frequency axis strip.

import type { CenterGrid } from "./navigation";

/** A closed 1-D range on a navigator's axis: Hz for frequency, Unix seconds for time. */
export interface Range { lo: number; hi: number }

export const spanOf = (r: Range) => r.hi - r.lo;
const finiteRange = (r: Range | null): r is Range =>
  !!r && Number.isFinite(r.lo) && Number.isFinite(r.hi) && r.hi > r.lo;

/**
 * The extent a **frequency** navigator spans: the whole device-available spectrum, as the union of
 * the achievable centre bounds the backend reported.
 *
 * `null` when no grid was reported (a replay has no front end, so there is no navigable spectrum);
 * the navigator then draws nothing rather than inventing a span to draw on.
 *
 * The union of the outermost bounds, not a list of bands: a front end with disjoint ranges is
 * still one axis to travel along, and the gaps are the front end's business to refuse (the snap
 * against `ranges_hz` in `navigation.ts` already does), not the bar's to hide.
 */
export function spectrumExtent(g: CenterGrid | null): Range | null {
  if (!g?.ranges_hz?.length) return null;
  let lo = Infinity, hi = -Infinity;
  for (const r of g.ranges_hz) {
    if (!Array.isArray(r) || !Number.isFinite(r[0]) || !Number.isFinite(r[1])) continue;
    lo = Math.min(lo, r[0]);
    hi = Math.max(hi, r[1]);
  }
  return hi > lo ? { lo, hi } : null;
}

/** One entry of `GET /api/navigation`'s `windows` list: a front end capturing right now. */
export interface ActiveWindow {
  deviceId: string | null;
  driver: string | null;
  centerHz: number;
  spanHz: number;
  loHz: number;
  hiHz: number;
}

/** The `windows` field of `GET /api/navigation`, as it is read here. */
export interface WindowsResponse {
  windows?: readonly {
    device_id?: string | null; driver?: string | null;
    center_hz?: number; span_hz?: number; f_lo_hz?: number; f_hi_hz?: number;
  }[] | null;
}

/**
 * Every currently-active capture window the backend **reported**.
 *
 * `[]` when the field is absent or empty — *no window was reported*, which is what the navigator
 * then shows. Deliberately no fallback to `frequency.current`: that is one device's tuned state,
 * and deriving a one-element list from it would make a single-window picture appear on a server
 * that never enumerated its front ends. The list's length is the backend's answer, and today it is
 * one because that is what this server holds (docs/api.md, `windows`).
 */
export function activeWindows(body: WindowsResponse | null | undefined): ActiveWindow[] {
  const out: ActiveWindow[] = [];
  for (const w of body?.windows ?? []) {
    const { center_hz: c, span_hz: s, f_lo_hz: lo, f_hi_hz: hi } = w;
    if (!Number.isFinite(c) || !Number.isFinite(s) || !Number.isFinite(lo) || !Number.isFinite(hi)) continue;
    if (!(hi! > lo!)) continue;
    out.push({
      deviceId: w.device_id ?? null, driver: w.driver ?? null,
      centerHz: c!, spanHz: s!, loHz: lo!, hiHz: hi!,
    });
  }
  return out;
}

/** A range placed on a navigator's extent, as percentages along it. */
export interface Placement { startPct: number; sizePct: number }

/**
 * Where `[lo, hi]` sits on `ext`, clipped to it, or `null` when it lies wholly outside.
 *
 * `minPct` is a **drawing** floor: a 2.4 MHz capture window on a 6 GHz bar is 0.04 % wide and
 * would round away to nothing, and a window that cannot be seen is the one thing this bar exists
 * to show. Nothing reads the drawn size back as a span — the measured numbers stay on the
 * `ActiveWindow`.
 */
export function placeOn(ext: Range | null, lo: number, hi: number, minPct = 0.5): Placement | null {
  if (!finiteRange(ext) || !Number.isFinite(lo) || !Number.isFinite(hi)) return null;
  const a = Math.min(lo, hi), b = Math.max(lo, hi);
  if (b < ext.lo || a > ext.hi) return null;
  const span = spanOf(ext);
  const startPct = ((Math.max(a, ext.lo) - ext.lo) / span) * 100;
  const endPct = ((Math.min(b, ext.hi) - ext.lo) / span) * 100;
  const sizePct = Math.max(minPct, endPct - startPct);
  return { startPct: Math.min(startPct, 100 - sizePct), sizePct };
}

/** A lit segment: one active capture window drawn on the frequency navigator. */
export interface LitSegment extends Placement {
  deviceId: string | null;
  loHz: number;
  hiHz: number;
}

/**
 * One lit segment per **reported** active window, in the order the backend listed them.
 *
 * A window outside the navigator's extent is omitted rather than clamped to an edge, which would
 * put a capture where it is not. With `windows: []` this returns `[]` — one segment appears
 * because one window was reported, never because the code assumes there is one.
 */
export function litSegments(ws: readonly ActiveWindow[], ext: Range | null): LitSegment[] {
  const out: LitSegment[] = [];
  for (const w of ws) {
    const p = placeOn(ext, w.loHz, w.hiHz);
    if (p) out.push({ ...p, deviceId: w.deviceId, loHz: w.loHz, hiHz: w.hiHz });
  }
  return out;
}

/** The value at fraction `f` (0 = `lo`) of an extent; clamped into it. */
export function valueAt(ext: Range, f: number): number {
  const x = Math.min(1, Math.max(0, Number.isFinite(f) ? f : 0));
  return ext.lo + x * spanOf(ext);
}

/** The fraction of `ext` at which `v` sits, clamped to 0…1. */
export function fractionAt(ext: Range, v: number): number {
  const span = spanOf(ext);
  return span > 0 ? Math.min(1, Math.max(0, (v - ext.lo) / span)) : 0;
}

/**
 * The range a drag from fraction `a` to fraction `b` of `ext` selects, ordered low-to-high;
 * `null` when the drag has no width (a click, or a drag inside one pixel's worth of the axis).
 *
 * This is the "dragged region zooms the main view to it" gesture, on **either** navigator: the
 * caller then snaps it (frequency: `navigation.snapState`; time: `navigation.snapTimeCell`) and
 * applies it as a view change.
 */
export function regionFromDrag(ext: Range | null, a: number, b: number): Range | null {
  if (!finiteRange(ext)) return null;
  const lo = valueAt(ext, Math.min(a, b)), hi = valueAt(ext, Math.max(a, b));
  return hi > lo ? { lo, hi } : null;
}

/**
 * `v` moved by `delta` along its axis, keeping its width and **clamped inside `bounds`**;
 * `overflow` is how far past a bound the move asked to go (negative below, positive above, 0
 * inside).
 *
 * The clamp is the whole point: a navigator pan stops at the edge of what is being shown and
 * reports the overshoot, so a caller can *offer* whatever would be needed to go further. It never
 * performs it — on the frequency axis "further" means moving the radio (T-343).
 */
export function panWithin(bounds: Range, v: Range, delta: number): { range: Range; overflow: number } {
  const w = Math.min(spanOf(v), spanOf(bounds));
  let lo = v.lo + (Number.isFinite(delta) ? delta : 0), overflow = 0;
  if (lo < bounds.lo) { overflow = lo - bounds.lo; lo = bounds.lo; }
  if (lo + w > bounds.hi) { overflow = lo + w - bounds.hi; lo = bounds.hi - w; }
  return { range: { lo, hi: lo + w }, overflow };
}

/**
 * `v` scaled by `factor` (> 1 zooms in) about the point at fraction `at` of `v`, clamped into
 * `bounds` and never narrower than `minSpan` (nor wider than `bounds`).
 */
export function zoomWithin(bounds: Range, v: Range, at: number, factor: number, minSpan: number): Range {
  if (!(factor > 0) || !Number.isFinite(factor)) return v;
  const x = Math.min(1, Math.max(0, at));
  const pivot = v.lo + x * spanOf(v);
  const maxSpan = spanOf(bounds);
  const w = Math.min(maxSpan, Math.max(Math.min(minSpan, maxSpan), spanOf(v) / factor));
  return clampInto(bounds, { lo: pivot - x * w, hi: pivot - x * w + w });
}

/** `v` shifted (never resized) to lie inside `bounds`; wider than `bounds` becomes `bounds`. */
export function clampInto(bounds: Range, v: Range): Range {
  const w = Math.min(spanOf(v), spanOf(bounds));
  let lo = v.lo;
  if (lo < bounds.lo) lo = bounds.lo;
  if (lo + w > bounds.hi) lo = bounds.hi - w;
  return { lo, hi: lo + w };
}

// ---- the time navigator's extent, and why it is the capture window ----

/** The `window` of `GET /api/timeline`, as `app/capture/timeline.ts` parses it. */
export interface CaptureWindowLike { t0S: number; t1S: number; spanS: number }

/**
 * The extent a **time** navigator spans: exactly the retained capture window (T-338) — the IQ
 * ring's configured retention, `[t0_s, t1_s]`, which is `span_s` long.
 *
 * `null` when the server reported no capture window: the bar then scrubs nothing rather than
 * offering times with no capture behind them.
 *
 * **Not the history horizon.** `GET /api/history` reaches back over the spectrum-history pyramid —
 * tiered, lossy, and in general far longer — and `/api/navigation`'s `time` block describes that
 * one. Sizing this bar from either would put positions on it that the ring has already overwritten,
 * which is the exact failure T-338 removed from the capture band (a hard-coded 48 h against a
 * two-minute ring). This module never reads those fields.
 */
export function timeExtent(w: CaptureWindowLike | null): Range | null {
  if (!w || !Number.isFinite(w.t0S) || !Number.isFinite(w.t1S) || !(w.spanS > 0)) return null;
  return w.t1S > w.t0S ? { lo: w.t0S, hi: w.t1S } : null;
}

/**
 * Time runs **down** the waterfall, so the vertical navigator is oldest at the top: fraction 0 of
 * the bar is `t0`, fraction 1 is the live edge. This is the one place the vertical orientation is
 * stated; everything else is `valueAt`/`fractionAt` over the extent.
 */
export const timeAtFraction = (ext: Range, f: number) => valueAt(ext, f);
export const fractionAtTime = (ext: Range, tS: number) => fractionAt(ext, tS);

// ---- what the time navigator's overview is *of* (T-367) ----
//
// The user's correction: *"the left vertical bar is the TIME navigator: it selects the time range
// and shows a compressed history waterfall **of the currently-selected frequency range only** (not
// the whole spectrum)"*.
//
// So the bar has two independent inputs and they come from different places. Its **extent** is the
// capture window (above, `/api/timeline`'s `window`) and its **picture** is that window folded over
// one frequency range — the range the main view is on, which the caller supplies. A bar that asked
// for no range gets no picture at all rather than the whole spectrum's: `/api/timeline` answers a
// `null` grid without `f_lo`/`f_hi`, and that is the honest answer to "which frequencies?" when
// nothing is tuned, where an unscoped overview would be a different measurement drawn in its place.
//
// This is *not* the frequency navigator's content. That bar shows the survey across the whole
// device-available spectrum; nothing here reads or produces a spectrum extent.

/** A frequency range, exactly as the main view holds one. */
export interface Band { loHz: number; hiHz: number }

/** A band as one comparable value, for noticing that the selected range moved. `""` is *no range*
 * — distinct from every real one, and never equal to another `""`-producing band. */
export const bandKey = (b: Band | null): string =>
  b && Number.isFinite(b.loHz) && Number.isFinite(b.hiHz) && b.hiHz > b.loHz ? `${b.loHz}:${b.hiHz}` : "";

/** Whether two bands name the same frequency range (both absent counts as the same). */
export const sameBand = (a: Band | null, b: Band | null): boolean => bandKey(a) === bandKey(b);

/**
 * The `GET /api/timeline` request the **time** navigator makes: the capture window, and the
 * overview folded onto `columns × rows` **over `band` only**.
 *
 * `band` is the frequency range the main view is on. With no band the request still asks for the
 * window — the bar can say how long it spans before it can say what was in it — and the server
 * answers a `null` grid, which is drawn as no picture. Nothing here widens a missing band to the
 * whole spectrum: that would put a different frequency range's energy on a bar the user reads as
 * "what has been happening *here*".
 */
export function timelineRequest(band: Band | null, columns: number, rows: number): string {
  const q = `columns=${columns}&rows=${rows}`;
  return bandKey(band) ? `/api/timeline?f_lo=${band!.loHz}&f_hi=${band!.hiHz}&${q}` : `/api/timeline?${q}`;
}
