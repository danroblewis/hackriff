// T-803 (MAP-03): Explore's bottom sheet, hosting the focus panel ("Selected", docs/23 §10.3) until
// MAP-04 rehomes that panel's content and MAP-14 adds the Explore tab. The sheet is the generic
// component in `./sheet`; this mount only gives it a heading and one rule: a NEW selection raises a
// collapsed sheet to `half`, so what was just selected is visible (the mockup's `showDetail`),
// without overwriting the viewer's stored preference. Clearing the selection leaves the sheet where
// the viewer put it. Presentation only: it reads the store's `focus`, never the client.
import type { MountFn } from "../context";
import { closeCard, type Focus, type InventoryTab } from "../explore/slice";
import { mountSheet, type SheetController, type SheetSnap } from "./sheet";

// T-997: the one handle other chrome has on this sheet. The inventory pills (`inv-pills.ts`) open
// the sheet on a list, and the lists live in its body, so they need to raise it — and nothing more.
// `show` never lowers a taller state the viewer chose, so this cannot shrink the sheet either.
let controller: SheetController | null = null;

/** Put the card on screen at at least `snap`. No-op before it mounts. */
export function revealFocusSheet(snap: SheetSnap): void {
  controller?.show(snap);
}

/** The sheet's persisted-snap key (per viewer; see `sheet.ts`). */
export const FOCUS_SHEET_KEY = "hk-mui-sheet-selected";
/** Top chrome (the floating Go-to / nudges / top-right cluster / status pill) plus the dock the sheet floats
 * above, in CSS px. Must agree with `map-layout.css`'s `--sheet-bottom` and the chrome it keeps
 * clear of. (T-882 retired the surface's toolbar row, `.sf-bar`, which this used to name.) */
export const FOCUS_SHEET_RESERVED_PX = 170;

/**
 * The card's heading — the one line visible while it is collapsed to its strip. T-804: a focused
 * signal whose row is loaded names its served centre, so the collapsed card still says *which*
 * signal is selected (the detail sheet's big frequency, in one line).
 *
 * T-1026: with nothing focused the card is only on screen because a pill asked for a list, so the
 * heading names THAT list. The old wording ("Selected — nothing yet: click a signal or drag a
 * region") was an instruction printed along the bottom edge of a card that is now simply not there
 * until something is selected — the bar the user asked to be rid of, and its text with it.
 */
export function focusSheetTitle(f: Focus, centerHz?: number | null, tab: InventoryTab = "confirmed"): string {
  switch (f.kind) {
    case "signal": return centerHz != null && Number.isFinite(centerHz) ? `Selected signal · ${(centerHz / 1e6).toFixed(4)} MHz` : "Selected signal";
    case "selection": return "Selected region";
    default: return tab === "candidate" ? "Candidate signals in view" : "Confirmed signals in view";
  }
}

/** The floating controls the sheet's `full` snap must never cover (T-528's rule, carried to the
 * floating chrome by T-882): Go-to and the top-right cluster (Layers, Measure, Viewport, Review, ⋯),
 * and — since T-993 retired the top bar — the nudge row and the mode/status pill it left behind.
 * A hidden one (`display: none`) measures a bottom of 0 and so never lowers the bound. */
export const TOP_FLOATING = [".map-ctl .map-goto", ".map-ctl .map-topright", ".map-ctl .map-nudge", ".map-ctl .map-status"];

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
    // T-1026: hidden until something is clicked. The store's `card.open` is the one truth about
    // that, so the ×, Escape and a flick-to-dismiss report back to it rather than leaving the sheet
    // off screen with the store still saying a feature is selected.
    initialOpen: false,
    onClose: () => ctx.store.set(closeCard()),
    // T-528: nothing may cover a control. Since T-882 the controls float over the canvas, so
    // `full` is bounded by where the floating top chrome actually ends, not by a fixed estimate.
    clearOf: () => floatingChromeBottom(),
  });
  controller = sheet;
  watchToolbar(() => sheet.relayout());
  // T-1026: the card is on screen exactly while `card.open` — set by selecting a feature (a box, a
  // pin, a list row) or by an inventory pill, cleared by a click on bare map or by a dismiss.
  ctx.store.select((s) => s.card.open, (open) => { if (open) sheet.show("half"); else sheet.hide(); }, { immediate: true });
  // The heading follows the focused row's served centre (refined when the server has refined it),
  // which can arrive after the focus does; only a change of focus ever raises the sheet.
  const retitle = () => {
    const s = ctx.store.get();
    const r = s.focus.kind === "signal" ? s.inventory.rows[s.focus.id] : undefined;
    sheet.title.textContent = focusSheetTitle(s.focus, r ? r.refined?.center_hz ?? r.f_center_hz : null, s.inventory.tab);
  };
  ctx.store.select((s) => {
    const r = s.focus.kind === "signal" ? s.inventory.rows[s.focus.id] : undefined;
    return (r ? r.refined?.center_hz ?? r.f_center_hz : null) as number | null;
  }, retitle);
  // With nothing focused the heading names the list the card was opened on, so it follows the tab.
  ctx.store.select((s) => s.inventory.tab, retitle);
  ctx.store.select((s) => s.focus, (f, prev) => {
    retitle();
    const changed = prev !== undefined && (f.kind !== prev.kind || (f.kind !== "none" && prev.kind !== "none" && f.id !== prev.id));
    // A NEW selection raises a card the viewer had left collapsed, so what was just clicked is
    // visible; `show` opens it if a subscriber ran before `card.open` reached this mount.
    if (changed && f.kind !== "none") sheet.show("half");
  }, { immediate: true });
  // T-824 (docs/23 §10.3): at phone width Research opening takes the whole height, so the sheet drops
  // to its peek strip rather than sitting half-hidden behind it. Not persisted: it is the layout's
  // move, not the viewer's choice, and closing Research leaves the sheet where it now is.
  ctx.store.select((s) => s.research?.open === true, (open) => {
    if (open && isPhoneWidth() && sheet.get() !== "peek") sheet.set("peek", false);
  });
};
