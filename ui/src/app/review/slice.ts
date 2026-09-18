// Review drawer state (ADR-0013 §3.1). Owner: T-155. Top-level key: review (T-155 adds `bookmarks`
// here as a new top-level key, read by T-152 when present; the Device tab's own full control state
// stays local to review/device.ts, never written into this slice or into shell's `device`).
import type { Bookmark } from "../../controls/bookmarks";
import type { AppState } from "../state";

// T-445 removed "history" (the region-over-time spectrum grid): the unified surface is that view.
export type ReviewTab = "alarms" | "report" | "scheduler" | "device" | "bookmarks";
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

export const toggleReview = (s: AppState): Partial<AppState> => ({ review: { ...s.review, open: !s.review.open } });
export const openReview = (tab: ReviewTab, region: ReviewSlice["region"] = null) => (): Partial<AppState> => ({ review: { open: true, tab, region } });

export const setBookmarks = (list: readonly Bookmark[]) => (): Partial<AppState> => ({ bookmarks: { list, loadedAtS: Date.now() / 1000, error: null } });
export const bookmarksError = (error: string) => (s: AppState): Partial<AppState> => ({ bookmarks: { ...s.bookmarks, error } });
