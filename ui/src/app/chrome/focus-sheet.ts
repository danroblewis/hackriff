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
/** Top chrome (the bar + the floating Go-to / top-right cluster) plus the dock the sheet floats
 * above, in CSS px. Must agree with `map-layout.css`'s `--sheet-bottom` and the chrome it keeps
 * clear of. (T-882 retired the surface's toolbar row, `.sf-bar`, which this used to name.) */
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

/** The floating controls the sheet's `full` snap must never cover (T-528's rule, carried to the
 * floating chrome by T-882): Go-to and the top-right cluster (Layers, Measure, Viewport). */
export const TOP_FLOATING = [".map-ctl .map-goto", ".map-ctl .map-topright"];

/** The lowest bottom among `TOP_FLOATING`, or null before the surface has mounted them. */
export function floatingChromeBottom(doc: Pick<Document, "querySelector"> = document): number | null {
  const bottoms = TOP_FLOATING.map((q) => doc.querySelector(q)?.getBoundingClientRect().bottom)
    .filter((b): b is number => typeof b === "number" && Number.isFinite(b));
  return bottoms.length ? Math.max(...bottoms) : null;
}

/** Frames to look for the floating cluster before giving up (Explore mounts before the centre
 * area that builds it, and the cluster mounts after the surface boots; ~10 s at 60 Hz). Without it
 * the sheet keeps its fixed `reservedPx`. */
const TOOLBAR_WAIT_FRAMES = 600;

/** Re-lay the sheet out whenever the floating cluster's box changes size (it tracks the stage),
 * once the surface has built it. No-op where the browser APIs are absent. */
function watchToolbar(relayout: () => void): void {
  if (typeof ResizeObserver === "undefined" || typeof requestAnimationFrame === "undefined") return;
  let frames = 0;
  const look = () => {
    const bar = document.querySelector(".map-ctl");
    if (bar) new ResizeObserver(relayout).observe(bar);
    else if (++frames < TOOLBAR_WAIT_FRAMES) requestAnimationFrame(look);
  };
  look();
}

/** Phone width (T-824): at or under this, Research is a full-height panel (`phone.css`) and the sheet
 * drops to `peek` when it opens (docs/23 §10.3). Must agree with `phone.css`'s breakpoint. */
export const PHONE_MAX_PX = 600;

/** Is the window phone-width? False where the browser API is absent (a headless test). */
export function isPhoneWidth(win: { matchMedia?: (q: string) => { matches: boolean } } | undefined =
  typeof window === "undefined" ? undefined : window): boolean {
  return typeof win?.matchMedia === "function" && win.matchMedia(`(max-width: ${PHONE_MAX_PX}px)`).matches;
}

export const mountFocusSheet: MountFn = (el, ctx) => {
  const sheet = mountSheet(el, {
    storageKey: FOCUS_SHEET_KEY, label: "Selected", reservedPx: FOCUS_SHEET_RESERVED_PX,
    // T-528: nothing may cover a control. Since T-882 the controls float over the canvas, so
    // `full` is bounded by where the floating top chrome actually ends, not by a fixed estimate.
    clearOf: () => floatingChromeBottom(),
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
  // T-824 (docs/23 §10.3): at phone width Research opening takes the whole height, so the sheet drops
  // to its peek strip rather than sitting half-hidden behind it. Not persisted: it is the layout's
  // move, not the viewer's choice, and closing Research leaves the sheet where it now is.
  ctx.store.select((s) => s.research?.open === true, (open) => {
    if (open && isPhoneWidth() && sheet.get() !== "peek") sheet.set("peek", false);
  });
};
