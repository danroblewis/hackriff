// Review drawer state (ADR-0013 §3.1). Owner: T-155. Top-level key: review (T-155 adds `bookmarks`
// here as a new top-level key, read by T-152 when present; the Device tab's own full control state
// stays local to review/device.ts, never written into this slice or into shell's `device`).
import type { Bookmark } from "../../controls/bookmarks";
import type { AppState } from "../state";

// T-445 removed "history" (the region-over-time spectrum grid): the unified surface is that view.
export type ReviewTab = "alarms" | "report" | "scheduler" | "device" | "bookmarks";

/**
 * T-1007: the drawer's tabs are two GROUPS, and the drawer states which it is showing.
 *
 * User, 2026-09-25: "instead of the Review section a lot of those things could be considered
 * settings." Review is what needs a person — anomalies/alarms and the survey report they are read
 * against — and it keeps its badge. Everything CONFIGURABLE (the device/display/sweep/recording
 * controls, the scheduler, the saved bookmarks) is settings, reached from the map's ⋯ menu, which
 * also holds the small preferences directly (`chrome/settings.ts`). One drawer, two groups: the tab
 * decides which, so nothing new has to be kept in sync with it.
 */
export type DrawerGroup = "review" | "settings";
export const REVIEW_TABS: readonly ReviewTab[] = ["alarms", "report"];
export const SETTINGS_TABS: readonly ReviewTab[] = ["device", "scheduler", "bookmarks"];

/** Which group a tab belongs to. */
export const tabGroup = (tab: ReviewTab): DrawerGroup => (REVIEW_TABS.includes(tab) ? "review" : "settings");
export interface ReviewSlice {
  open: boolean; tab: ReviewTab;
  /** Region the drawer was opened on (e.g. a selection's History action); null = the live view. */
  region: { loHz: number; hiHz: number; t0?: number; t1?: number } | null;
}

/** `/api/bookmarks`, mirrored here so T-152 can draw markers on the live view when this key exists
 * (ADR-0013 §4.3 "Markers / bookmarks"). Written only by review/bookmarks.ts. */
export interface BookmarksSlice { list: readonly Bookmark[]; loadedAtS: number | null; error: string | null }

export interface ReviewState { review: ReviewSlice; bookmarks: BookmarksSlice }

export const reviewInitial = (): ReviewState => ({
  review: { open: false, tab: "alarms", region: null },
  bookmarks: { list: [], loadedAtS: null, error: null },
});

/**
 * The Review button: show the REVIEW group, whatever the drawer last held.
 *
 * T-1007: the drawer is shared with the settings panels, so Review means "show me what needs a
 * person" in three cases, not two — closed (open it on alarms), open on a settings panel (switch to
 * alarms; pressing the badge must never *close* the drawer that is showing something else), open on
 * review (close it, the toggle it has always been). Closing keeps the tab, so close/open returns
 * where you were.
 */
export const toggleReview = (s: AppState): Partial<AppState> => {
  const onReview = tabGroup(s.review.tab) === "review";
  if (s.review.open && !onReview) return { review: { ...s.review, tab: "alarms" } };
  return { review: { ...s.review, open: !s.review.open, tab: onReview ? s.review.tab : "alarms" } };
};
export const openReview = (tab: ReviewTab, region: ReviewSlice["region"] = null) => (): Partial<AppState> => ({ review: { open: true, tab, region } });

export const setBookmarks = (list: readonly Bookmark[]) => (): Partial<AppState> => ({ bookmarks: { list, loadedAtS: Date.now() / 1000, error: null } });
export const bookmarksError = (error: string) => (s: AppState): Partial<AppState> => ({ bookmarks: { ...s.bookmarks, error } });
