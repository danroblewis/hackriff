// T-803 (MAP-03): the reusable bottom sheet — docs/23 §5 and §10.3, ui/mockups/map-ui-v1.html.
//
// T-1026 (user 2026-09-25): the sheet is also CLOSED-BY-DEFAULT now, which is a state of its own and
// not a fourth snap. Openness answers "is this panel on screen at all" (the Google-Maps place card:
// nothing at the bottom edge until a feature is clicked, gone again on a click on the back of the
// map); the snap answers "how tall is it while it is". So the two are separate: `show`/`hide` put
// the host on and off the screen (`hidden`, so it takes no pixels and no hit test), while `set`,
// `reveal` and the drag size it, and the size is the only half a viewer's `localStorage` remembers —
// openness belongs to the current selection, never to the browser profile.
//
// A non-modal panel docked to the bottom of the full-bleed surface with three snap states, `peek`
// (a title strip), `half` (~45 % of the viewport) and `full` (as tall as the chrome allows). It is
// dragged by its grab handle (with flick), clicked to cycle, and driven from the keyboard
// (↑/↓ step, Home/End jump). It is NEVER modal: no backdrop, no focus trap, nothing laid over the
// canvas outside the sheet's own box, so the surface beneath stays live, pannable and zoomable and a
// pointer that starts outside the sheet reaches the canvas. The snap state is a per-viewer
// preference kept in `localStorage` behind try/catch; with storage unavailable the sheet still
// renders (at its default) and still moves — it just forgets across reloads.
//
// Presentation only (CLAUDE.md thin client): this module takes no client and imports nothing that
// talks to the backend, so dragging, clicking or keying the sheet cannot reach a route — least of all
// a device route. `ui/test/app-sheet.test.ts` asserts that on the source and on a spy.
//
// Split in two: the pure model (snap arithmetic, keys, persistence — node-testable, no DOM) and
// `mountSheet`, which wires it to an element that already contains the sheet's body. The mount
// never replaces the host's children, because a body can itself be another area's `data-slot`
// (Explore's focus panel is), and that area owns its subtree.

import { trackOverlay } from "./dismiss";

export type SheetSnap = "peek" | "half" | "full";
export const SNAPS: readonly SheetSnap[] = ["peek", "half", "full"];

/** The title strip's height: the grab handle plus one line of heading (docs/23 §10.3: ~56 px). */
export const PEEK_PX = 56;
/** `half` as a fraction of the viewport height (docs/23 §10.3: ~45 vh). */
export const HALF_FRAC = 0.45;
/** A flick faster than this (px/ms, ~ one screen per second on a phone) moves one snap state in its
 * direction even if the finger stopped short of the midpoint — the Material/NNg bottom-sheet feel. */
export const FLICK_PX_PER_MS = 0.5;
/** A press that moves less than this is a click (cycle), not a drag. */
export const DRAG_SLOP_PX = 4;

/** The three snap heights for a viewport `viewportH` tall with `reservedPx` of chrome the sheet must
 * never cover (the top bar and the floating top chrome above it, the dock below it). Monotone
 * non-decreasing whatever the viewport, so the model's ordering holds on a tiny window too. */
export function snapHeights(viewportH: number, reservedPx: number): Record<SheetSnap, number> {
  const full = Math.max(PEEK_PX, Math.round(viewportH - reservedPx));
  const half = Math.min(full, Math.max(PEEK_PX, Math.round(viewportH * HALF_FRAC)));
  return { peek: PEEK_PX, half, full };
}

/** The snap state nearest a free drag height. Ties go to the smaller state (less covered canvas). */
export function nearestSnap(heightPx: number, heights: Record<SheetSnap, number>): SheetSnap {
  let best: SheetSnap = "peek";
  for (const s of SNAPS) if (Math.abs(heightPx - heights[s]) < Math.abs(heightPx - heights[best])) best = s;
  return best;
}

/** One step up (+1, taller) or down (-1), clamped at the ends. */
export function stepSnap(s: SheetSnap, dir: 1 | -1): SheetSnap {
  const i = SNAPS.indexOf(s) + dir;
  return SNAPS[Math.max(0, Math.min(SNAPS.length - 1, i))];
}

/** Clicking the grab handle cycles peek → half → full → peek (the mockup's behaviour). */
export function cycleSnap(s: SheetSnap): SheetSnap {
  return SNAPS[(SNAPS.indexOf(s) + 1) % SNAPS.length];
}

/**
 * Where a released drag lands. `velocityPxPerMs` is positive when the sheet was growing (finger
 * moving up). A flick moves at least one state in its direction from where the drag started;
 * otherwise the nearest snap to the released height wins.
 */
export function releaseSnap(
  from: SheetSnap, heightPx: number, velocityPxPerMs: number, heights: Record<SheetSnap, number>,
): SheetSnap {
  const near = nearestSnap(heightPx, heights);
  if (Math.abs(velocityPxPerMs) < FLICK_PX_PER_MS) return near;
  const dir: 1 | -1 = velocityPxPerMs > 0 ? 1 : -1;
  const stepped = stepSnap(from, dir);
  // A long, fast drag may already be past the next state; never land *behind* the finger.
  return dir > 0
    ? (SNAPS.indexOf(near) > SNAPS.indexOf(stepped) ? near : stepped)
    : (SNAPS.indexOf(near) < SNAPS.indexOf(stepped) ? near : stepped);
}

/** The keyboard equivalents of the drag: the new state for `key`, or `null` if the key is not ours
 * (Enter/Space are the button's own click, which cycles). */
export function keySnap(key: string, s: SheetSnap): SheetSnap | null {
  switch (key) {
    case "ArrowUp": return stepSnap(s, 1);
    case "ArrowDown": return stepSnap(s, -1);
    case "Home": return "full";
    case "End": return "peek";
    default: return null;
  }
}

/** What a gesture asks for: a snap height, or `"close"` — off the screen altogether (T-1026). */
export type SheetMove = SheetSnap | "close";

/**
 * The keyboard move, with the dismiss on the end of it (T-1026): shrinking a sheet that is already
 * at its smallest closes it, so the same key that made it smaller can give the last 56 px back to
 * the map. Every other key is [[keySnap]] unchanged.
 */
export function keyMove(key: string, s: SheetSnap): SheetMove | null {
  const next = keySnap(key, s);
  if (next === null) return null;
  const shrinking = key === "ArrowDown" || key === "End";
  return next === s && shrinking ? "close" : next;
}

/**
 * Where a released drag lands, with the dismiss (T-1026): a downward flick from `peek` — the sheet
 * is already as small as a sheet gets, and the finger is still going down — closes it, the Material
 * "swipe to dismiss" the bottom-sheet grammar ends in. Every other release is [[releaseSnap]].
 */
export function releaseMove(
  from: SheetSnap, heightPx: number, velocityPxPerMs: number, heights: Record<SheetSnap, number>,
): SheetMove {
  const next = releaseSnap(from, heightPx, velocityPxPerMs, heights);
  return from === "peek" && next === "peek" && velocityPxPerMs <= -FLICK_PX_PER_MS ? "close" : next;
}

/** The storage surface the sheet needs — `localStorage`, or a test double, or `null` (unavailable). */
export type SnapStorage = Pick<Storage, "getItem" | "setItem"> | null;

/** The persisted snap state, or `fallback` for absent, unknown or throwing storage. */
export function readSnap(storage: SnapStorage, key: string, fallback: SheetSnap): SheetSnap {
  try {
    const v = storage?.getItem(key);
    return v && (SNAPS as readonly string[]).includes(v) ? (v as SheetSnap) : fallback;
  } catch {
    return fallback;
  }
}

/** Persist the snap state; a throwing or absent storage is ignored (per-viewer convenience only). */
export function writeSnap(storage: SnapStorage, key: string, s: SheetSnap): void {
  try { storage?.setItem(key, s); } catch { /* storage unavailable */ }
}

/** `localStorage` if this page may touch it, else `null` (some browsers throw on the getter). */
export function pageStorage(): SnapStorage {
  try { return typeof localStorage === "undefined" ? null : localStorage; } catch { return null; }
}

// ---- DOM ----

export interface SheetOptions {
  /** `localStorage` key for this sheet's snap state (one per sheet). */
  storageKey: string;
  /** Accessible name of the grab handle's sheet, e.g. "Selected". */
  label: string;
  /** State when nothing is stored. */
  initial?: SheetSnap;
  /** Whether the sheet is on screen when it mounts. Default **false** (T-1026: the detail card is
   * hidden until a feature is clicked); openness is never read from or written to storage. */
  initialOpen?: boolean;
  /** Run when the VIEWER dismissed the sheet — the ×, Escape, a keyboard shrink past `peek` or a
   * downward flick — so the owner can clear what the card was about. Not called by [[hide]], which
   * is the owner closing it itself. */
  onClose?: () => void;
  /** Viewport pixels the sheet must leave uncovered (top chrome + dock). */
  reservedPx?: number;
  /** Test/embedding seam; defaults to `localStorage`. */
  storage?: SnapStorage;
  /** The viewport y of the lowest chrome edge the sheet's top must stay below (e.g. the
   * floating top chrome, T-882), read at every layout. `null` = none. The
   * sheet reserves whichever is more: `reservedPx`, or what this edge needs. */
  clearOf?: () => number | null;
}

/** Gap kept between the sheet's top at `full` and the chrome it clears (`clearOf`). */
export const CLEAR_GAP_PX = 8;

/** The reserved height when the sheet's top must clear a chrome edge at viewport y `clearY`, given
 * the sheet's bottom edge sits `bottomGapPx` above the viewport's bottom. Never less than `minPx`. */
export function reservedFor(minPx: number, clearY: number | null, bottomGapPx: number): number {
  if (clearY === null || !Number.isFinite(clearY) || !Number.isFinite(bottomGapPx)) return minPx;
  return Math.max(minPx, Math.ceil(clearY + CLEAR_GAP_PX + bottomGapPx));
}

export interface SheetController {
  get(): SheetSnap;
  /** Is the sheet on screen at all (T-1026)? */
  isOpen(): boolean;
  /** Put it on screen, at `min` or the taller state the viewer last sized it to. */
  show(min?: SheetSnap): void;
  /** Take it off screen: no strip, no pixels at the bottom edge, nothing to hit-test. */
  hide(): void;
  /** Move to `s`. `persist` (default true) records it as the viewer's choice. */
  set(s: SheetSnap, persist?: boolean): void;
  /** Raise to at least `s` without persisting — for content that needs room (a new selection).
   * Never lowers a taller state the viewer chose. */
  reveal(s: SheetSnap): void;
  /** Re-apply the current state's height — when the chrome it clears (`clearOf`) changed size. */
  relayout(): void;
  /** The element the caller fills with the peek-strip heading. */
  readonly title: HTMLElement;
}

const SNAP_LABEL: Record<SheetSnap, string> = { peek: "collapsed", half: "half height", full: "full height" };

/**
 * Turn `host` (which already holds the sheet's content in a `.sheet-body` child) into a bottom
 * sheet: prepends the grab handle and the peek title strip, applies the stored snap state, and
 * wires drag / click / keyboard. `host`'s other children are left exactly where they are.
 */
export function mountSheet(host: HTMLElement, opts: SheetOptions): SheetController {
  const storage = opts.storage === undefined ? pageStorage() : opts.storage;
  const reserved = opts.reservedPx ?? 150;
  const body = host.querySelector<HTMLElement>(":scope > .sheet-body");
  const grab = document.createElement("button");
  grab.type = "button";
  grab.className = "sheet-grab";
  grab.append(document.createElement("i"));
  const title = document.createElement("div");
  title.className = "sheet-title";
  const head = document.createElement("div");
  head.className = "sheet-head";
  // docs/23 §10.6 P1: an overlay exists to be closed — a visible dismiss that hands ALL of the
  // sheet's pixels back to the map (T-1026: off the screen, not down to a 56 px strip along the
  // bottom edge, which is the bar the user asked to be rid of).
  const close = document.createElement("button");
  close.type = "button";
  close.className = "sheet-close";
  close.textContent = "×";
  close.setAttribute("aria-label", `Close ${opts.label} sheet`);
  close.setAttribute("title", "Close");
  head.append(title, close);
  host.prepend(grab, head);
  host.classList.add("sheet");
  host.setAttribute("aria-label", opts.label);

  // T-1026: Escape closes the card, exactly as the × does — the same `dismiss` both, so the two
  // dismissals can never mean two different things.
  const overlay = trackOverlay(`sheet:${opts.storageKey}`, () => dismiss());
  let snap = readSnap(storage, opts.storageKey, opts.initial ?? "peek");
  let open = opts.initialOpen ?? false;
  const heights = () => {
    const vh = window.innerHeight;
    // The dock offset is CSS (`--sheet-bottom`); the host's bottom edge does not move with height.
    const bottomGap = vh - host.getBoundingClientRect().bottom;
    return snapHeights(vh, reservedFor(reserved, opts.clearOf?.() ?? null, bottomGap));
  };

  function apply() {
    // T-1026: closed means OFF THE SCREEN — `hidden`, so it takes no pixels, no hit test and no tab
    // stop, and `data-open` states which it is for the CSS and for a test that reads the DOM.
    host.hidden = !open;
    host.dataset.open = String(open);
    host.dataset.snap = snap;
    host.style.height = `${heights()[snap]}px`;
    grab.setAttribute("aria-label", `${opts.label} sheet, ${SNAP_LABEL[snap]}. Drag, click or use arrow keys to resize.`);
    grab.setAttribute("aria-expanded", String(snap !== "peek"));
    // Collapsed content is not on screen, so it must not be in the tab order either.
    if (body) body.inert = !open || snap === "peek";
    // The dismiss is offered whenever the sheet is on screen — including at `peek`, whose strip is
    // exactly the chrome that has to be closeable (it used to be hidden there, so the strip was the
    // one state with no way out but a drag).
    close.hidden = !open;
    // T-900: an open sheet is an overlay on the one stack, so Escape closes it when it is topmost.
    overlay.open(open);
  }

  /** The viewer dismissed it (×, Escape, a shrink past `peek`, a downward flick). */
  function dismiss() {
    if (!open) return;
    ctl.hide();
    opts.onClose?.();
  }

  let drag: { y0: number; h0: number; moved: boolean; lastY: number; lastT: number; v: number } | null = null;
  const ctl: SheetController = {
    get: () => snap,
    isOpen: () => open,
    show(min = "half") {
      // Opening never *shrinks* what the viewer sized the sheet to, and the raise it does make is
      // not persisted: the height a selection needed is the selection's, not the profile's.
      if (!open) { open = true; if (SNAPS.indexOf(min) > SNAPS.indexOf(snap)) snap = min; apply(); return; }
      ctl.reveal(min);
    },
    hide() {
      if (!open) return;
      open = false;
      apply();
    },
    set(s, persist = true) {
      snap = s;
      if (persist) writeSnap(storage, opts.storageKey, s);
      apply();
    },
    reveal(s) {
      if (SNAPS.indexOf(s) > SNAPS.indexOf(snap)) ctl.set(s, false);
    },
    relayout: () => { if (!drag) apply(); },
    title,
  };

  // Drag. Pointer capture keeps the gesture on the handle, so it never reaches the canvas; a drag
  // that STARTS outside the sheet was never ours to begin with (non-modal).
  let swallowClick = false;
  grab.addEventListener("pointerdown", (ev) => {
    if (ev.button !== 0) return;
    swallowClick = false;
    drag = { y0: ev.clientY, h0: host.getBoundingClientRect().height, moved: false, lastY: ev.clientY, lastT: ev.timeStamp, v: 0 };
    grab.setPointerCapture(ev.pointerId);
    host.classList.add("dragging");
  });
  grab.addEventListener("pointermove", (ev) => {
    if (!drag) return;
    const hs = heights();
    const dy = ev.clientY - drag.y0;
    if (Math.abs(dy) > DRAG_SLOP_PX) drag.moved = true;
    const dt = ev.timeStamp - drag.lastT;
    if (dt > 0) drag.v = (drag.lastY - ev.clientY) / dt;
    drag.lastY = ev.clientY;
    drag.lastT = ev.timeStamp;
    host.style.height = `${Math.max(hs.peek, Math.min(hs.full, drag.h0 - dy))}px`;
  });
  const end = () => {
    if (!drag) return;
    const d = drag;
    drag = null;
    host.classList.remove("dragging");
    if (!d.moved) { apply(); return; }
    swallowClick = true;
    const move = releaseMove(snap, host.getBoundingClientRect().height, d.v, heights());
    if (move === "close") dismiss(); else ctl.set(move);
  };
  grab.addEventListener("pointerup", end);
  grab.addEventListener("pointercancel", end);
  grab.addEventListener("click", () => {
    if (swallowClick) { swallowClick = false; return; }
    ctl.set(cycleSnap(snap));
  });
  grab.addEventListener("keydown", (ev) => {
    const next = keyMove(ev.key, snap);
    if (next === null) return;
    ev.preventDefault();
    if (next === "close") dismiss(); else ctl.set(next);
  });
  close.addEventListener("click", (ev) => {
    // Not the head's click below, which would re-size a collapsed sheet.
    ev.stopPropagation();
    dismiss();
  });
  // The title strip is a second, larger target: clicking it while collapsed opens the sheet.
  head.addEventListener("click", () => { if (snap === "peek") ctl.set("half"); });
  window.addEventListener("resize", apply);

  apply();
  return ctl;
}
