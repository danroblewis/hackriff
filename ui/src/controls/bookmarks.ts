// Markers and bookmarks (T-051) over `/api/bookmarks` (T-050): user metadata only (name, centre,
// optional bandwidth and note), stored server-side. Add from a click or a selection, list, jump,
// delete. Text via textContent.
import * as ax from "../axis";
import { formatFrequency } from "./freq";

export interface Bookmark {
  id: string;
  kind: "marker" | "bookmark";
  name: string;
  f_center_hz: number;
  bandwidth_hz: number | null;
  note: string | null;
  created_s: number;
  updated_s: number;
}

export interface NewBookmark { kind: "marker" | "bookmark"; name: string; f_center_hz: number; bandwidth_hz?: number }

/** hk-model `BOOKMARK_NAME_MAX` (characters). */
export const BOOKMARK_NAME_MAX = 120;

const clip = (s: string) => Array.from(s.trim().replace(/\s+/g, " ")).slice(0, BOOKMARK_NAME_MAX).join("");

/** A marker at a clicked frequency (named after it unless a name is given). */
export function bookmarkFromClick(hz: number, name = ""): NewBookmark {
  return { kind: "marker", name: clip(name) || formatFrequency(hz), f_center_hz: hz };
}

/** A bookmark covering a selection's band. */
export function bookmarkFromSelection(s: { name: string; f_lo: number; f_hi: number }): NewBookmark {
  return { kind: "bookmark", name: clip(s.name) || "Selection", f_center_hz: (s.f_lo + s.f_hi) / 2, bandwidth_hz: s.f_hi - s.f_lo };
}

export type Jump = { kind: "zoom"; loHz: number; hiHz: number } | { kind: "retune"; centerHz: number } | { kind: "outside" };

/**
 * Where "jump" goes: inside the displayed band, zoom to the bookmark (its bandwidth ×3, at least
 * ±25 kHz); outside it, retune when the device can (an explicit action), otherwise nothing.
 */
export function jumpPlan(g: ax.Geometry | null, b: Pick<Bookmark, "f_center_hz" | "bandwidth_hz">, canRetune: boolean): Jump {
  const full = g ? ax.fullView(g) : null;
  if (full && b.f_center_hz >= full.loHz && b.f_center_hz <= full.hiHz) {
    const half = Math.max(25e3, 1.5 * (b.bandwidth_hz ?? 0));
    return { kind: "zoom", loHz: b.f_center_hz - half, hiHz: b.f_center_hz + half };
  }
  return canRetune ? { kind: "retune", centerHz: b.f_center_hz } : { kind: "outside" };
}
