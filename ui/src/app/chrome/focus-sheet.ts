// T-803 (MAP-03): Explore's bottom sheet, hosting the focus panel ("Selected", docs/23 §10.3) until
// MAP-04 rehomes that panel's content and MAP-14 adds the Explore tab. The sheet is the generic
// component in `./sheet`; this mount only gives it a heading and one rule: a NEW selection raises a
// collapsed sheet to `half`, so what was just selected is visible (the mockup's `showDetail`),
// without overwriting the viewer's stored preference. Clearing the selection leaves the sheet where
// the viewer put it. Presentation only: it reads the store's `focus`, never the client.
import type { MountFn } from "../context";
import type { Focus } from "../explore/slice";
import { mountSheet } from "./sheet";

/** The sheet's persisted-snap key (per viewer; see `sheet.ts`). */
export const FOCUS_SHEET_KEY = "hk-mui-sheet-selected";
/** Top chrome (bar + the surface's toolbar) plus the dock the sheet floats above, in CSS px. Must
 * agree with `map-layout.css`'s `--sheet-bottom` and the `.bar` / `.sf-bar` it keeps clear of. */
export const FOCUS_SHEET_RESERVED_PX = 170;

/** The peek strip's heading for a focus state — the one line visible while collapsed. T-804: a
 * focused signal whose row is loaded names its served centre, so the collapsed sheet still says
 * *which* signal is selected (the detail sheet's big frequency, in one line). */
export function focusSheetTitle(f: Focus, centerHz?: number | null): string {
  switch (f.kind) {
    case "signal": return centerHz != null && Number.isFinite(centerHz) ? `Selected signal · ${(centerHz / 1e6).toFixed(4)} MHz` : "Selected signal";
    case "selection": return "Selected region";
    default: return "Selected — nothing yet: click a signal or drag a region";
  }
}

/** Frames to look for the surface's toolbar before giving up (Explore mounts before the centre
 * area that builds it; ~10 s at 60 Hz). Without it the sheet keeps its fixed `reservedPx`. */
const TOOLBAR_WAIT_FRAMES = 600;

/** Re-lay the sheet out whenever `.sf-bar` changes size (it wraps to more rows as it narrows or as a
 * control is added), once the surface has built it. No-op where the browser APIs are absent. */
function watchToolbar(relayout: () => void): void {
  if (typeof ResizeObserver === "undefined" || typeof requestAnimationFrame === "undefined") return;
  let frames = 0;
  const look = () => {
    const bar = document.querySelector(".sf-bar");
    if (bar) new ResizeObserver(relayout).observe(bar);
    else if (++frames < TOOLBAR_WAIT_FRAMES) requestAnimationFrame(look);
  };
  look();
}

export const mountFocusSheet: MountFn = (el, ctx) => {
  const sheet = mountSheet(el, {
    storageKey: FOCUS_SHEET_KEY, label: "Selected", reservedPx: FOCUS_SHEET_RESERVED_PX,
    // T-528: nothing may cover a toolbar button. `.sf-bar` wraps to more rows on a narrow window,
    // so `full` is bounded by where the toolbar actually ends, not by a fixed estimate.
    clearOf: () => document.querySelector(".sf-bar")?.getBoundingClientRect().bottom ?? null,
  });
  watchToolbar(() => sheet.relayout());
  // The heading follows the focused row's served centre (refined when the server has refined it),
  // which can arrive after the focus does; only a change of focus ever raises the sheet.
  ctx.store.select((s) => {
    const r = s.focus.kind === "signal" ? s.inventory.rows[s.focus.id] : undefined;
    return (r ? r.refined?.center_hz ?? r.f_center_hz : null) as number | null;
  }, (hz) => { sheet.title.textContent = focusSheetTitle(ctx.store.get().focus, hz); });
  ctx.store.select((s) => s.focus, (f, prev) => {
    const r = f.kind === "signal" ? ctx.store.get().inventory.rows[f.id] : undefined;
    sheet.title.textContent = focusSheetTitle(f, r ? r.refined?.center_hz ?? r.f_center_hz : null);
    const changed = prev !== undefined && (f.kind !== prev.kind || (f.kind !== "none" && prev.kind !== "none" && f.id !== prev.id));
    if (changed && f.kind !== "none") sheet.reveal("half");
  }, { immediate: true });
};
