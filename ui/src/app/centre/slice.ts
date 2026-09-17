// Centre live-view state (ADR-0013 §3.1). Owner: T-152. Top-level keys: live, navGrid.
import type { ActiveWindow } from "../../navigators";
import type { NavigationGrid } from "../../navigation";

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
  /**
   * The capture time of the newest spectrum row the stream has delivered (T-379) — the live edge,
   * on the **capture clock**, arriving with every row rather than on a poll.
   *
   * The spectrum record carries its own absolute time (docs/api.md), so this is served, never
   * measured here. `null` means no timed row has arrived yet, and a surface then falls back to the
   * capture window's `t1S` — and to *unknown* when there is no window either. Nothing substitutes
   * `Date.now()`: a replay's clock and the browser's are unrelated.
   */
  edgeTS: number | null;
  /** The view a retune asked for (a pan past the band edge), applied by the next header whose
   * geometry differs; null otherwise. */
  pendingView: { loHz: number; hiHz: number } | null;
  /** A retune the user has not asked for yet (T-343); null when nothing is offered. */
  retuneOffer: RetuneOffer | null;
}

/**
 * What `GET /api/navigation` reported, for the edge navigators (T-340).
 *
 * `windows` is the **reported** list of currently-active capture windows, never derived from
 * `grid.frequency.current`: the frequency navigator lights one segment per entry, so a list
 * invented from a singleton would draw a window count nothing measured. `loaded` distinguishes
 * *not asked yet* from *nothing reported*.
 */
export interface NavGridSlice {
  grid: NavigationGrid | null;
  windows: ActiveWindow[];
  loaded: boolean;
}

export interface CentreState { live: LiveSlice; navGrid: NavGridSlice }

export const centreInitial = (): CentreState => ({
  live: {
    streamId: null, centerHz: null, bandwidthHz: null, bins: null, rowRateHz: null, view: null,
    edgeTS: null, pendingView: null, retuneOffer: null,
  },
  navGrid: { grid: null, windows: [], loaded: false },
});

/** Records a `GET /api/navigation` answer (T-340). A store write only: nothing here moves a device. */
export const setNavigation = (grid: NavigationGrid | null, windows: ActiveWindow[]) => (): { navGrid: NavGridSlice } =>
  ({ navGrid: { grid, windows, loaded: true } });

/** Seconds the stream's live edge must advance before it is written back (T-379). Rows arrive at
 * about 25/s and nothing re-renders on the edge alone, so this only keeps the store from churning;
 * the inventory's window is read at call time, and a quarter-second is far inside its 5 s poll. */
export const EDGE_WRITE_S = 0.25;

/** Records the newest spectrum row's capture time, if it advanced enough to be worth a write
 * ([[EDGE_WRITE_S]]). A non-finite or going-backwards time is ignored — a re-plumbed stream's first
 * rows can repeat, and a live edge must never move backwards under the lists that read it. */
export const setLiveEdge = (tS: number) => (s: { live: LiveSlice }): { live?: LiveSlice } => {
  if (!Number.isFinite(tS)) return {};
  const cur = s.live.edgeTS;
  if (cur !== null && tS - cur < EDGE_WRITE_S) return {};
  return { live: { ...s.live, edgeTS: tS } };
};
