// T-993 (MMAP): the app-shell top bar retires over the map. docs/23 §10 — P1 (the map's pixels are
// the map's: no fixed-height bar above the canvas) and P4 (small controls for big view changes).
//
// The bar's controls are MOVED, not copied: each is one element with one id, wired once by
// `shell.ts` (mode switch, device state, readouts, stream status, Review, Theme) or `centre/nudge.ts`
// (the T-409 tuning nudges), and this module only decides which parent it sits in:
//
//   Explore  → the map's floating chrome (`map-controls.ts` registers its homes):
//                the Explore/Decode/History switch, device state, readouts, recording and stream
//                status  → the `.map-status` pill (top-centre when wide, under the top-right
//                cluster otherwise);
//                the nudges → `.map-nudge`, directly under Go-to — the one place device commands
//                live (the retune offer is there too);
//                Review → an icon button in the top-right cluster;
//                Theme  → the cluster's ⋯ menu.
//   Decode / History → back into the framed shell's `<header class="bar">`, in their original order.
//
// Moving the nodes (rather than building second copies) keeps every id unique and every listener
// where it was, so nothing that already reads `#device-label`, `#review-btn` or the nudge slot
// changes. Presentation only: nothing here reaches the client or a route.

/** Where each bar control lives while the map is shown. Built by `mountMapControls`. */
export interface MapHome {
  /** The mode switch + device/stream state pill. */
  status: HTMLElement;
  /** The tuning nudges' row, under Go-to. */
  nudge: HTMLElement;
  /** Review's place in the top-right cluster (before the ⋯ button). */
  review: HTMLElement;
  /** The ⋯ menu's body, where Theme goes. */
  more: HTMLElement;
}

/** Which bar element goes to which home, in the bar's own order. Each selector is unique on the page. */
export const TOP_CHROME_PARTS: readonly (readonly [string, keyof MapHome])[] = [
  [".modes", "status"],
  ["#device", "status"],
  ["#rec-pill", "status"],
  [".readouts", "status"],
  ["#conn", "status"],
  ["[data-slot=nudge]", "nudge"],
  ["#review-btn", "review"],
  ["#theme-btn", "more"],
];

let home: MapHome | null = null;
let onMap = false;
/** Each moved element's place in the bar: a comment anchor left where it was. */
const anchors = new Map<Element, Comment>();

/** The map's floating chrome has mounted (or remounted): record its homes and re-place. */
export function registerMapHome(h: MapHome): void {
  home = h;
  place();
}

/** The shell's mode changed: true while Explore (the map) is shown. */
export function placeTopChrome(explore: boolean): void {
  onMap = explore;
  place();
}

function place(): void {
  if (typeof document === "undefined" || typeof document.querySelector !== "function") return;
  const bar = document.querySelector(".app > .bar");
  if (!bar) return;
  for (const [sel, where] of TOP_CHROME_PARTS) {
    // A part in a home that has since been detached (the surface remounted) is found through its anchor.
    const el = document.querySelector(sel) ?? [...anchors.keys()].find((e) => e.matches(sel)) ?? null;
    if (!el) continue;
    if (onMap && home) {
      if (!anchors.has(el)) {
        const a = document.createComment(`T-993: ${sel} floats over the map in Explore`);
        el.before(a);
        anchors.set(el, a);
      }
      if (el.parentElement !== home[where]) home[where].append(el);
    } else {
      const a = anchors.get(el);
      if (a && el.previousSibling !== a) a.after(el);
    }
  }
  if (home) for (const k of ["status", "nudge"] as const) home[k].hidden = !onMap || home[k].childElementCount === 0;
}
