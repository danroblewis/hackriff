// Capture-timeline state (ADR-0013 §3.1, §3.3). Owner: T-150. Top-level keys: time, captureWindow.
import type { AppState } from "../state";
// Pure types only (`timeline.ts` imports nothing), so this is not a cycle.
import type { CaptureWindow } from "./timeline";

/**
 * The capture-timeline cursor: following live data, or reviewing a past instant (Unix s).
 *
 * T-340: `spanS` is the time **span** the reviewed view covers, ending at `tS` — what a dragged
 * region on the time navigator zooms the waterfall to. Absent (or null) means *no span was asked
 * for*, and the review render falls back to the rows it holds at their own period; it is never a
 * default duration, because a duration invented here would be the 48 h constant T-338 removed all
 * over again.
 */
export type TimeCursor = { live: true } | { live: false; tS: number; spanS?: number | null };

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
