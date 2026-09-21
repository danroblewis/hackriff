// Time-cursor and capture-clock state (ADR-0013 §3.1, §3.3). Owner: T-150; moved from
// `app/capture/slice.ts` by T-506 when the Capture panel was folded into the canvas. Top-level
// keys: time, captureWindow.
import type { AppState } from "../state";
// Pure types only (`capture-window.ts` imports nothing), so this is not a cycle.
import type { CaptureWindow } from "./capture-window";

/**
 * The capture-timeline cursor: following live data, or reviewing a past instant (Unix s).
 *
 * `spanS` is the time **span** the view covers, ending at `tS` (T-340). Absent (or null) means *no
 * span was asked for*, and the reader falls back to its own extent; it is never a default duration,
 * because a duration invented here would be the 48 h constant T-338 removed all over again.
 *
 * T-445: `spanS` is carried on **both** arms. It used to be frozen-only, because the only thing
 * that could name a span was a dragged region on the retired time navigator, and a following view's
 * span was implicitly the waterfall's ring height over its row rate. The unified surface's time
 * axis is zoomable while following (a pane can follow the growing edge over 20 s or over 10
 * minutes), so a live cursor with no span would leave `viewWindow` computing one window while the
 * canvas drew another — the two-windows defect the whole-UI window rule names, reintroduced by
 * omission. Absent (or null) still means *no span was asked for*, never a default duration.
 */
export type TimeCursor =
  | { live: true; spanS?: number | null }
  | { live: false; tS: number; spanS?: number | null };

/**
 * Cursor equality for `store.select` (a new `{live: true}` object is the same cursor).
 *
 * Moved here by T-445 from the retired `centre/review-render.ts`. It is not review-render's
 * property — it is the cursor's — and leaving it in the module that happened to need it first is
 * how `explore/index.ts` ended up importing the *history renderer* to compare two time cursors.
 *
 * The live arm compares `spanS` too, now that a following view can name one: a pane that zoomed its
 * time axis while still following has genuinely changed window, and a comparison that called those
 * two cursors equal would leave every subscriber answering about the old span.
 */
export const sameCursor = (a: TimeCursor, b: TimeCursor): boolean =>
  (a.live
    ? b.live && (a.spanS ?? null) === (b.spanS ?? null)
    : !b.live && a.tS === b.tS && (a.spanS ?? null) === (b.spanS ?? null));

export interface CaptureState {
  time: TimeCursor;
  /**
   * The capture window `GET /api/timeline` reported, shared by every surface (T-379).
   *
   * **This is the UI's one live edge, and it is the capture clock's.** A replay, the mock SDR on a
   * time-compressed scene, or any device whose stamps differ from the host clock all run on a time
   * of their own, and `Date.now()` is not on it — the fixture behind this task's evidence was
   * 3.5 days from wall time. A surface that windows on the browser's clock therefore asks about a
   * range the capture never covered and renders empty *while holding the data*, which is the
   * failure the whole-UI window rule names. Read the live edge from here, never from a clock in the
   * browser.
   *
   * `null` is **unknown** — not answered yet, or no capture window on this server — and a surface
   * must say so rather than substitute a window of its own.
   */
  captureWindow: CaptureWindow | null;
}

export const captureInitial = (): CaptureState => ({ time: { live: true }, captureWindow: null });

/** Records the capture window `GET /api/timeline` served (T-379); `null` when it reports none. */
export const setCaptureWindow = (w: CaptureWindow | null) => (s: AppState): Partial<AppState> => {
  const cur = s.captureWindow;
  const same = cur === w || (!!cur && !!w && cur.t0S === w.t0S && cur.t1S === w.t1S && cur.spanS === w.spanS
    && (cur.buffered?.t0S ?? null) === (w.buffered?.t0S ?? null) && (cur.buffered?.t1S ?? null) === (w.buffered?.t1S ?? null));
  return same ? {} : { captureWindow: w };
};

export const goLive = (): Partial<AppState> => ({ time: { live: true } });

/** Review the instant `tS`, optionally over an explicit span ending there (T-340). */
export const reviewAt = (tS: number, spanS: number | null = null) => (): Partial<AppState> =>
  (Number.isFinite(tS) ? { time: { live: false, tS, spanS: spanS !== null && spanS > 0 ? spanS : null } } : {});
