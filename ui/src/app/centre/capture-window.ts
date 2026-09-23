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
  /** What the ring currently holds, inside the window; `null` when it holds nothing. */
  buffered: { t0S: number; t1S: number } | null;
}

/** The `GET /api/timeline` response fields read here: the window, and nothing about a band. */
export interface TimelineResponse {
  window?: {
    enabled?: boolean;
    retention_s?: number | null;
    t0_s?: number | null; t1_s?: number | null; span_s?: number | null;
    buffered?: { t0_s: number; t1_s: number } | null;
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
  return {
    t0S: t0_s, t1S: t1_s, spanS: span_s,
    buffered: b && typeof b.t0_s === "number" && typeof b.t1_s === "number" ? { t0S: b.t0_s, t1S: b.t1_s } : null,
  };
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
 * `null` overall when no window has been answered.
 */
export interface RingRules { retentionS: number; iqS: number | null; spanS: number }

export function ringRules(w: CaptureWindow | null, edgeS: number | null): RingRules | null {
  if (!w) return null;
  const edge = edgeS !== null && Number.isFinite(edgeS) && edgeS > 0 ? Math.max(edgeS, w.t1S) : w.t1S;
  const retentionS = edge - w.spanS;
  const b = w.buffered;
  const iqS = b && b.t1S > b.t0S ? Math.max(b.t0S, retentionS) : null;
  return { retentionS, iqS, spanS: w.spanS };
}

/** Whether an instant still has IQ behind it: `"live"` at the live edge, `"ring"` inside what the
 * ring holds, `"recording"` past the ring but inside a persisted IQ recording (T-464), "outside-ring"
 * (renamed in spirit, not in wire value, to "outside every span we know of") past all of them,
 * `"unknown"` when nothing has answered yet. */
export type IqBacking = "live" | "ring" | "recording" | "outside-ring" | "unknown";

/**
 * `spans`, from `GET /api/recordings`'s `iq_available` (T-469), is the authority: ring **and**
 * recordings, so a position past the ring but inside a recording answers `"recording"` rather than
 * the false `"outside-ring"` a ring-only check would give. `null` means "not asked yet" and falls
 * back to `rules` (the ring-only answer `ringRules` already gives from `GET /api/timeline`) so a
 * caller with no recordings poll running still gets the ring's own honest boundary; an empty answered
 * array (`[]`) is trusted as-is — the server looked and found nothing extending the horizon, which is
 * different from not having asked.
 */
export function iqBackingAt(
  tS: number,
  live: boolean,
  rules: RingRules | null,
  spans: readonly IqSpan[] | null = null,
): IqBacking {
  if (live) return "live";
  if (spans !== null) {
    if (coveredBy(spans, tS, "ring")) return "ring";
    if (coveredBy(spans, tS, "recording")) return "recording";
    return "outside-ring";
  }
  if (!rules || rules.iqS === null) return "unknown";
  return tS >= rules.iqS ? "ring" : "outside-ring";
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
// `ringRules`/`iqS` above answers a narrower question — where the *ring's own* window starts — from
// `GET /api/timeline`. Raw IQ also survives in persisted recordings well past the ring (CLAUDE.md,
// "Playback"), and `GET /api/recordings`'s `iq_available` (T-469) already answers the wider question
// exactly: the ring's window and every complete IQ recording, as spans on the one shared time axis,
// **deliberately not merged into one envelope** — an envelope over a hole between two spans would
// promise audio that does not exist there. This module trusts that answer rather than computing a
// second opinion from spectrum coverage (a different question, over a much longer horizon: "was this
// observed", not "does raw IQ still back it").

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
