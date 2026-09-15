// Review drawer state (ADR-0013 §3.1). Owner: T-155. Top-level key: review (T-155 adds e.g.
// `bookmarks` here as a new top-level key; the Device tab's full control state lives here too).
import type { AppState } from "../state";

export type ReviewTab = "alarms" | "report" | "history" | "scheduler" | "device" | "bookmarks";
export interface ReviewSlice {
  open: boolean; tab: ReviewTab;
  /** Region the drawer was opened on (e.g. a selection's History action); null = the live view. */
  region: { loHz: number; hiHz: number; t0?: number; t1?: number } | null;
}

export interface ReviewState { review: ReviewSlice }

export const reviewInitial = (): ReviewState => ({ review: { open: false, tab: "alarms", region: null } });

export const toggleReview = (s: AppState): Partial<AppState> => ({ review: { ...s.review, open: !s.review.open } });
export const openReview = (tab: ReviewTab, region: ReviewSlice["region"] = null) => (): Partial<AppState> => ({ review: { open: true, tab, region } });
