// Centre live-view state (ADR-0013 §3.1). Owner: T-152. Top-level key: live.

/**
 * A retune a pan to the band edge has **offered** but not performed (T-343).
 *
 * Moving the front end is a device action: it can stop and re-plumb the running capture, and it
 * takes the one radio. A drag therefore leaves this behind for the user to accept or ignore; the
 * device is reached only from `view.ts`'s `applyDeviceAction`, never from the gesture.
 */
export interface RetuneOffer { centerHz: number; view: { loHz: number; hiHz: number } }

/** The live spectrum stream's geometry (header) and the client-side zoom. */
export interface LiveSlice {
  streamId: string | null; centerHz: number | null; bandwidthHz: number | null; bins: number | null;
  rowRateHz: number | null; view: { loHz: number; hiHz: number } | null;
  /** The view a retune asked for (a pan past the band edge), applied by the next header whose
   * geometry differs; null otherwise. */
  pendingView: { loHz: number; hiHz: number } | null;
  /** A retune the user has not asked for yet (T-343); null when nothing is offered. */
  retuneOffer: RetuneOffer | null;
}

export interface CentreState { live: LiveSlice }

export const centreInitial = (): CentreState => ({
  live: {
    streamId: null, centerHz: null, bandwidthHz: null, bins: null, rowRateHz: null, view: null,
    pendingView: null, retuneOffer: null,
  },
});
