// Centre live-view state (ADR-0013 §3.1). Owner: T-152. Top-level key: live.

/** The live spectrum stream's geometry (header) and the client-side zoom. */
export interface LiveSlice {
  streamId: string | null; centerHz: number | null; bandwidthHz: number | null; bins: number | null;
  rowRateHz: number | null; view: { loHz: number; hiHz: number } | null;
  /** The view a retune asked for (a pan past the band edge), applied by the next header whose
   * geometry differs; null otherwise. */
  pendingView: { loHz: number; hiHz: number } | null;
}

export interface CentreState { live: LiveSlice }

export const centreInitial = (): CentreState => ({
  live: { streamId: null, centerHz: null, bandwidthHz: null, bins: null, rowRateHz: null, view: null, pendingView: null },
});
