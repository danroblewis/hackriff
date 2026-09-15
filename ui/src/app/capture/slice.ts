// Capture-timeline state (ADR-0013 §3.1, §3.3). Owner: T-150. Top-level key: time.
import type { AppState } from "../state";

/** The capture-timeline cursor: following live data, or reviewing a past instant (Unix s). */
export type TimeCursor = { live: true } | { live: false; tS: number };

export interface CaptureState { time: TimeCursor }

export const captureInitial = (): CaptureState => ({ time: { live: true } });

export const goLive = (): Partial<AppState> => ({ time: { live: true } });
export const reviewAt = (tS: number) => (): Partial<AppState> => (Number.isFinite(tS) ? { time: { live: false, tS } } : {});
