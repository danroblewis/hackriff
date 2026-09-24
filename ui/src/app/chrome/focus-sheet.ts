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

/** The peek strip's heading for a focus state — the one line visible while collapsed. */
export function focusSheetTitle(f: Focus): string {
  switch (f.kind) {
    case "signal": return "Selected signal";
    case "selection": return "Selected region";
    default: return "Selected — nothing yet: click a signal or drag a region";
  }
}

export const mountFocusSheet: MountFn = (el, ctx) => {
  const sheet = mountSheet(el, { storageKey: FOCUS_SHEET_KEY, label: "Selected", reservedPx: FOCUS_SHEET_RESERVED_PX });
  ctx.store.select((s) => s.focus, (f, prev) => {
    sheet.title.textContent = focusSheetTitle(f);
    const changed = prev !== undefined && (f.kind !== prev.kind || (f.kind !== "none" && prev.kind !== "none" && f.id !== prev.id));
    if (changed && f.kind !== "none") sheet.reveal("half");
  }, { immediate: true });
};
