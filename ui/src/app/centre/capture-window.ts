// The capture window on the canvas's time axis (T-506; formerly `app/capture/timeline.ts`, T-150/
// T-338). Pure: no DOM, no fetch, no clock, no RF or retention constant.
//
// ## What moved here, and why the Capture panel could not simply be deleted
//
// The collapsible Capture panel under the canvas was the only place three things lived:
//
//  - **the IQ-availability boundary** — where raw IQ ends on the time axis. Playback has two
//    horizons (CLAUDE.md, "Playback"): spectrum history is long, raw IQ exists only in the ring, and
//    past the ring there is waterfall only and **no audio**. That boundary must be visible and
//    predictable, drawn from the ring window the backend reports — never from the spectrum-history
//    horizon.
//  - **the retention bound** — the configured capture window, `GET /api/timeline`'s
//    `window.retention_s` ending at the capture clock's live edge (T-338).
//  - **the capture clock** — `state.captureWindow`, the non-stream fallback for every live edge in
//    the UI (T-379). That poll now lives in `./capture-clock.ts`.
//
// The first two are now two rules drawn across every pane of the surface ([[ringRules]]), and the
// per-pane readout says which side of them the pane's own time position is on ([[iqNote]]).
//
// ## The span has no default, still
//
// T-338's regression was a hard-coded `WINDOW_S = 48 h` sizing the band, which offered times the
// ring had overwritten. Nothing here takes or holds a default span: `null` in, `null` out, and the
// caller renders *unknown* rather than a window of its own.
import type { Box } from "../../surface/lattice";
import { timeRuleQuads } from "../../surface/marks";
import type { OverlayQuad } from "../../surface/minimap";
import type { PaneRect } from "../../surface/surface";

/** The capture window `GET /api/timeline` reports: the IQ ring's configured retention (ADR-0014),
 * never the longer, lossy spectrum-history horizon. */
export interface CaptureWindow {
  /** Window start / end / span, Unix s, on the capture clock. `t0S = t1S − spanS`. */
  t0S: number; t1S: number; spanS: number;
  /** What the ring currently holds, inside the window; `null` when it holds nothing. `drops`: the
   * ring's next whole-slot evictions (T-845), oldest first, when the server states them. */
  buffered: { t0S: number; t1S: number; drops?: readonly RingDrop[] } | null;
}

/**
 * One scheduled whole-slot eviction of the ring (T-845, `buffered.drops` on `GET /api/timeline`):
 * when capture reaches `atS`, the ring's oldest sample jumps to `t0S`. `t0S` is exact (the start of
 * IQ already written); `atS` is the server's prediction at the current rate.
 */
export interface RingDrop { atS: number; t0S: number }

/** The `GET /api/timeline` response fields read here: the window, and nothing about a band. */
export interface TimelineResponse {
  window?: {
    enabled?: boolean;
    retention_s?: number | null;
    t0_s?: number | null; t1_s?: number | null; span_s?: number | null;
    buffered?: { t0_s: number; t1_s: number; drops?: readonly { at_s: number; t0_s: number }[] } | null;
  } | null;
}

/**
 * The capture window, or `null` when the server reports none — no ring, no retention, or no live
 * edge yet. `null` is **unknown**, and must be drawn as unknown: a default span here would be the
 * 48 h constant all over again.
 */
export function captureWindow(r: TimelineResponse | null): CaptureWindow | null {
  const w = r?.window;
  if (!w) return null;
  const { t0_s, t1_s, span_s } = w;
  if (typeof t0_s !== "number" || typeof t1_s !== "number" || typeof span_s !== "number") return null;
  if (!(span_s > 0) || !(t1_s > t0_s)) return null;
  const b = w.buffered;
  if (!(b && typeof b.t0_s === "number" && typeof b.t1_s === "number")) return { t0S: t0_s, t1S: t1_s, spanS: span_s, buffered: null };
  const buffered: NonNullable<CaptureWindow["buffered"]> = { t0S: b.t0_s, t1S: b.t1_s };
  if (Array.isArray(b.drops)) {
    // Kept only while well-formed and in order: a drop list the rule cannot trust is no list.
    const drops = b.drops.filter((d) => typeof d?.at_s === "number" && typeof d?.t0_s === "number")
      .map((d) => ({ atS: d.at_s, t0S: d.t0_s }));
    buffered.drops = drops.every((d, i) => i === 0 || (d.atS >= drops[i - 1].atS && d.t0S >= drops[i - 1].t0S)) ? drops : [];
  }
  return { t0S: t0_s, t1S: t1_s, spanS: span_s, buffered };
}

/** A capture window's length in words. Keeps seconds, because a retention is commonly seconds
 * (`--iq-retention 90s`) and rounding it to "2 min" would misstate the span being claimed. */
export function durationText(s: number): string {
  if (s < 90) return `${Math.round(s)} s`;
  if (s < 3600) return `${Math.round(s / 60)} min`;
  return `${(s / 3600).toFixed(1)} h`;
}

/** The current view span: the live geometry once the surface mirrors it, else a fallback from the
 * device's tuned centre/rate (both from the store, no analysis). */
export function currentSpan(input: {
  live: { loHz: number; hiHz: number } | null;
  device: { centerHz: number | null; sampleRateHz: number | null };
}): { loHz: number; hiHz: number } | null {
  if (input.live) return input.live;
  const { centerHz, sampleRateHz } = input.device;
  if (centerHz === null || sampleRateHz === null) return null;
  return { loHz: centerHz - sampleRateHz / 2, hiHz: centerHz + sampleRateHz / 2 };
}

// ---- the two rules on the canvas's time axis ----

/**
 * Where the two capture-window boundaries sit on the time axis, in capture-clock seconds.
 *
 * - `retentionS` — the **retention bound**: the live edge minus the configured retention
 *   (`span_s`). `GET /api/timeline` defines `t0_s = t1_s − retention_s`; this is that same
 *   subtraction made against the edge the panes are drawn to *this frame*, so the rule advances with
 *   the rows instead of jumping on the poll (the one-shared-time-axis rule). `edgeS` is the capture
 *   clock's edge (the surface's own, fed from `live.edgeTS` / `captureWindow.t1S`), never a
 *   browser clock; `null` falls back to the window's own `t1S`.
 * - `iqS` — the **IQ horizon**: the oldest instant the ring actually holds (`buffered.t0_s`). It
 *   sits *inside* the window and never resizes it: a ring ten seconds into a two-minute retention is
 *   a mostly-empty two-minute window (docs/api.md). It is never older than the retention bound — the
 *   ring's byte quota is derived from retention × rate, so it cannot hold more — which is also what
 *   keeps it moving per frame once the ring is full, instead of stepping on the poll. That clamp only
 *   ever moves the line *newer*: it can under-promise IQ, never promise IQ that is not there.
 *   `null` when the ring holds nothing: **unknown**, not "the ring is empty".
 *
 *   **Whole-slot drops (T-845).** A byte-full ring evicts a whole slot at a time (7.5 s of a
 *   120 s ring), which can put its oldest sample *ahead* of the retention bound, and the window is
 *   only re-polled every `CAPTURE_CLOCK_MS`. So `buffered.t0S` alone, drawn per frame, would claim
 *   up to a slot of IQ the ring dropped since the poll — and a clip or demod asked for there
 *   fails. The server schedules its next drops (`buffered.drops`), and the horizon is also held at
 *   or after the oldest sample every drop scheduled up to [[DROP_LEAD_S]] past the edge leaves —
 *   applied *ahead* of its time, because the ring's writer runs ahead of the edge the panes are
 *   drawn to and the drop's time is a prediction. A filling ring whose first drop is far off shows
 *   exactly what it holds. The rule under-promises by one slot for at most the lead before each
 *   drop; it never promises IQ the ring has dropped (unless the writer leads the drawn edge by
 *   more than the lead).
 *
 * `null` overall when no window has been answered.
 */
export interface RingRules {
  retentionS: number; iqS: number | null; spanS: number;
  /** The oldest sample the scheduled drops applied this frame leave (T-845); `null` when none is. */
  dropT0S?: number | null;
}

/**
 * How far ahead of the edge the panes are drawn to a scheduled ring drop is already honoured, s
 * (T-845). It covers what the drop's `at_s` cannot know: the ring's writer running ahead of the
 * drawn edge (stream and frame latency), and a rate raised since the prediction. It is not the
 * poll cadence and does not need to be — `at_s` is absolute, so an old poll's schedule stays right.
 */
export const DROP_LEAD_S = 2;

export function ringRules(w: CaptureWindow | null, edgeS: number | null): RingRules | null {
  if (!w) return null;
  const edge = edgeS !== null && Number.isFinite(edgeS) && edgeS > 0 ? Math.max(edgeS, w.t1S) : w.t1S;
  const retentionS = edge - w.spanS;
  const b = w.buffered;
  let dropT0S: number | null = null;
  for (const d of b?.drops ?? []) if (d.atS <= edge + DROP_LEAD_S) dropT0S = Math.max(dropT0S ?? d.t0S, d.t0S);
  const iqS = b && b.t1S > b.t0S ? Math.max(b.t0S, retentionS, Math.min(dropT0S ?? -Infinity, b.t1S)) : null;
  return { retentionS, iqS, spanS: w.spanS, dropT0S };
}

/** Whether an instant still has IQ behind it: `"live"` at the live edge, `"ring"` inside what the
 * ring holds, `"recording"` past the ring but inside a persisted IQ recording (T-464), "outside-ring"
 * (renamed in spirit, not in wire value, to "outside every span we know of") past all of them,
 * `"unknown"` when nothing has answered yet. */
export type IqBacking = "live" | "ring" | "recording" | "outside-ring" | "unknown";

/**
 * The ring's own boundary is judged from `rules` alone — worked out fresh every frame from the live
 * edge *this frame* is drawn to (`ringRules`), open at the live end — **never** from `spans`, which
 * is polled at most every `CAPTURE_CLOCK_MS` (5 s). A rolling ring moves both its edges: a snapshot
 * that old can already be behind the live edge (a just-paused pane reads a false `"outside-ring"`
 * until the next poll) or behind the ring's true, newer oldest-sample boundary (an old position
 * reads a false `"ring"` that `/api/playback` would then refuse). CLAUDE.md, "Playback": *"the
 * boundary is not a constant and must not be cached as one."*
 *
 * `spans`, from `GET /api/recordings`'s `iq_available` (T-469), only extends the answer past the
 * ring: a **recording** is immutable once written, so its polled span cannot go stale the way the
 * ring's can, and a position older than the ring but inside one answers `"recording"` rather than
 * the false `"outside-ring"` a ring-only check would give. `null` means "not asked yet"; `[]` is
 * trusted as-is — the server looked and named no recording there, which is different from not having
 * asked.
 */
export function iqBackingAt(
  tS: number,
  live: boolean,
  rules: RingRules | null,
  spans: readonly IqSpan[] | null = null,
): IqBacking {
  if (live) return "live";
  if (rules && rules.iqS !== null) {
    if (tS >= rules.iqS) return "ring";
    if (spans !== null && coveredBy(spans, tS, "recording")) return "recording";
    return "outside-ring";
  }
  // No ring answer yet (unanswered window, or a ring holding nothing so far): a recording, answered
  // separately, can still say so.
  if (spans !== null && coveredBy(spans, tS, "recording")) return "recording";
  return "unknown";
}

/**
 * What a viewport's time position is backed by, in one clause for the canvas readout.
 *
 * Five different answers that must never share a sentence: the IQ is in the ring, it is past the
 * ring but a recording still holds it (T-464), it is past every span raw IQ ever covered (the canvas
 * is spectrum history only — **no audio**), it is the live edge, or nobody has said. Promising audio
 * that cannot be delivered is the same defect as implying resolution never captured (CLAUDE.md,
 * "Playback").
 */
export function iqNote(backing: IqBacking): string {
  switch (backing) {
    case "live": return "following live: IQ is being captured";
    case "ring": return "IQ retained here: demod and decode can re-run";
    case "recording": return "past the ring, but a recording holds this IQ: demod and decode can re-run from it";
    case "outside-ring": return "past every raw-IQ span: spectrum history only, no IQ and no audio";
    default: return "IQ coverage unknown";
  }
}

// ---- how the two rules are drawn ----
//
// Distinct from the coverage grey (which is neutral, r = g = b) by being saturated, and from each
// other twice over — by ink and by pattern — because once the ring is full they sit on the same row
// (the ring holds exactly its retention), and a reader must still be able to see that both are
// there. The retention bound is drawn first, thicker and dashed; the IQ horizon is drawn solid on
// top of it, so a coincident pair reads as a green line with magenta dashes either side.
// Alpha 1: the ink is exactly these values on screen, which is what `ui/e2e` reads back.

/** The IQ horizon: where raw IQ ends. Above it (newer) demod and decode can re-run. */
export const IQ_RULE_INK: readonly [number, number, number, number] = [0.2, 0.95, 0.35, 1];
/** The retention bound: the start of the configured capture window. */
export const RETENTION_RULE_INK: readonly [number, number, number, number] = [1, 0.25, 0.85, 1];

/** Both rules for one pane, through the surface's own stroke geometry. Nothing is drawn for an
 * unanswered window, and nothing for an instant outside the pane. */
export function ringRuleQuads(rules: RingRules | null, paneBox: Box, rect: PaneRect): OverlayQuad[] {
  if (!rules) return [];
  return [
    ...timeRuleQuads(rules.retentionS * 1e9, RETENTION_RULE_INK, "retention", paneBox, rect, { thickPx: 4, dashPx: 10, gapPx: 6 }),
    ...(rules.iqS === null ? [] : timeRuleQuads(rules.iqS * 1e9, IQ_RULE_INK, "iq-horizon", paneBox, rect, { thickPx: 2 })),
  ];
}

// ---- the full IQ-available extent: ring PLUS recordings (T-464) ----
//
// `ringRules`/`iqS` above answers where the *ring's own* window starts, from `GET /api/timeline`,
// recomputed every frame from the live edge that frame is drawn to. Raw IQ also survives in
// persisted recordings well past the ring (CLAUDE.md, "Playback"), and `GET /api/recordings`'s
// `iq_available` (T-469) already answers that wider question exactly: the ring's window and every
// complete IQ recording, as spans on the one shared time axis, **deliberately not merged into one
// envelope** — an envelope over a hole between two spans would promise audio that does not exist
// there. This module trusts that answer rather than computing a second opinion from spectrum
// coverage (a different question, over a much longer horizon: "was this observed", not "does raw IQ
// still back it").
//
// `iqBackingAt` below reads this poll's spans for the `"recording"` half only, never for the ring:
// a recording is immutable once written, so its polled span cannot go stale, but the ring's own span
// here is at most `CAPTURE_CLOCK_MS` (5 s) old and a rolling ring moves both its edges in that time —
// using it for the ring answer let a just-paused pane read "no audio" it still had, and let an old
// position keep reading "ring" after the true horizon had already rolled past it (review fix). The
// ring answer stays `rules`-only, open at the live end, exactly as it was before this poll existed.

/** One span of raw IQ, exactly as `GET /api/recordings`'s `iq_available.spans` reports it. */
export interface IqSpan {
  /** Unix s. */
  t0S: number; t1S: number;
  /** `"ring"` (the rolling buffer — moves as it rolls) or `"recording"` (immutable once written). */
  source: "ring" | "recording";
  /** The recording id, for a `"recording"` span; `null` for `"ring"`. */
  recording: string | null;
}

/** The `GET /api/recordings` fields read here: only `iq_available.spans`, never a recording's other
 * metadata (kind, provenance, state…) — this module answers one question. */
export interface RecordingsIqResponse {
  iq_available?: {
    spans?: readonly { t0?: unknown; t1?: unknown; source?: unknown; recording?: unknown }[] | null;
  } | null;
}

/**
 * The raw-IQ-available spans (ring plus recordings), or `[]` when the server answered but named
 * none. A malformed or missing entry is dropped, never guessed into a span; a response with no
 * `iq_available` at all (an old server, or a failed read) yields `[]` too — the caller tells "not
 * asked yet" from "asked and got nothing" by keeping this `null` until the first successful read
 * (see `state.iqAvailability`), never by inspecting the array's length here.
 */
export function iqAvailability(r: RecordingsIqResponse | null): IqSpan[] {
  const spans = r?.iq_available?.spans;
  if (!Array.isArray(spans)) return [];
  const out: IqSpan[] = [];
  for (const s of spans) {
    const t0 = s?.t0, t1 = s?.t1, source = s?.source;
    if (typeof t0 !== "number" || typeof t1 !== "number" || !(t1 > t0)) continue;
    if (source !== "ring" && source !== "recording") continue;
    const recording = typeof s?.recording === "string" ? s.recording : null;
    out.push({ t0S: t0, t1S: t1, source, recording });
  }
  return out;
}

/** Whether `tS` falls inside any span of `source`. */
function coveredBy(spans: readonly IqSpan[], tS: number, source: "ring" | "recording"): boolean {
  return spans.some((s) => s.source === source && tS >= s.t0S && tS <= s.t1S);
}
