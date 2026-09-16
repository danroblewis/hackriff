// Capture-timeline state (ADR-0013 §3.1, §3.3). Owner: T-150. Top-level key: time.
import type { AppState } from "../state";

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

export interface CaptureState { time: TimeCursor }

export const captureInitial = (): CaptureState => ({ time: { live: true } });

export const goLive = (): Partial<AppState> => ({ time: { live: true } });

/** Review the instant `tS`, optionally over an explicit span ending there (T-340). */
export const reviewAt = (tS: number, spanS: number | null = null) => (): Partial<AppState> =>
  (Number.isFinite(tS) ? { time: { live: false, tS, spanS: spanS !== null && spanS > 0 ? spanS : null } } : {});
