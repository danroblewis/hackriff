// The analyze job the output panel is watching (T-862, ADR-0015 §5, MAUTO M-11). One key: the id
// of the `AnalyzeJob` (`a<n>`) to follow, set when the user starts Analyze from the context menu.
import type { AppState } from "../state";

export interface AnalyzeSlice { jobId: string | null }
export interface AnalyzeState { analyze: AnalyzeSlice }

export const analyzeInitial = (): AnalyzeState => ({ analyze: { jobId: null } });
export const watchAnalyzeJob = (jobId: string | null) => (): Partial<AppState> => ({ analyze: { jobId } });

/** Whether a job is being watched — the focus panel must then stay mounted (and shown) so its section renders. */
export const analyzeWatched = (jobId: string | null): boolean => jobId !== null;
