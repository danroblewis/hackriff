// T-1001 (MMAP split view): **each pane's own Live/Freeze button, inside its own rectangle.**
//
// The user's shape for split view (2026-09-25): "A common use would be to look at one signal from
// the past and the current waterfall, and each split needs its own 'Live' button." Until this
// ticket there was ONE follow-live FAB in the floating cluster (`chrome/map-controls.ts`'s
// `.map-fab`), and it acted on the hidden `SurfacePreview.activePane` — the pane last pressed or
// wheeled. With two panes open, the button that says "LIVE" said it about whichever pane the
// pointer happened to touch last, and pressing it froze a pane the user was not looking at. The
// FAB is retired; this module is what replaced it.
//
// The rules it keeps:
//
//   * **A button belongs to a pane, not to the chrome.** `press(id)` and `state(id)` name the pane
//     they act on, and no call here reads the active pane. Pressing pane 1's button therefore
//     cannot change pane 2 — the acceptance for the whole split-view set.
//   * **A pane's pause IS its time window** (T-347): freezing writes the window the pane already
//     showed, and nothing anywhere is flagged "paused". Capture, the IQ ring and detection are
//     always-on; this is a coordinate change on the view and reaches **no route** — the pane-live
//     unit test drives it against a `fetch` spy and a spy client with an empty call list.
//   * **T-955's three states, per pane**: at the tuned window's live edge a press freezes; frozen,
//     or following the live edge of spectrum the radio has left ("off-window"), a press brings the
//     pane to the tuned window's live edge. So a press can never freeze a pane that was not at the
//     edge (the explorer's 0430 shot: "LIVE" at 162.2 MHz after a retune to 144.6, pressed, frozen
//     at -14 s). The words moved here verbatim from the retired `fabState`/`fabPress`.
//   * **Placed every render frame from the pane rectangle the frame was drawn with** — the same
//     `outlineBox` conversion the active-pane outline makes (drawing-buffer GL px → CSS px), never
//     on a data poll, so a button cannot drift off the pane it belongs to (docs/16 §8's one shared
//     mapping). A press re-states every button in the same call, so no state on screen waits for
//     the next frame.

import type { PaneRect } from "../../surface/surface";
import { h } from "../dom";
import { outlineBox } from "./active-pane";

/** The front end's own current centre/span (`frequency.current`), or null when none is reported. */
export interface TunedWindow { centerHz: number; spanHz: number }

/** The subset of `surface/panes.ts`'s `PaneModel` a Live button drives. All of it is view state. */
export interface PaneLiveControl {
  isFollowing(id: string): boolean;
  setFollowing(id: string, on: boolean): void;
  /** T-955: following in time AND showing the tuned window (`PaneModel.atTunedLiveEdge`). */
  atTunedLiveEdge(id: string, tuned: TunedWindow | null): boolean;
  /** T-955: follow, and move onto the tuned window only if not overlapping it (`PaneModel.followTuned`). */
  followTuned(id: string, tuned: TunedWindow | null): void;
}

/** What one pane's button says: its class, its word, and the sentence behind it. */
export interface LiveState {
  cls: "following" | "frozen";
  /** T-955: following live TIME over spectrum the radio has left — a press moves it, not freezes it. */
  offTuned: boolean;
  /** The word on the button. */
  label: string;
  title: string;
}

/**
 * One pane's Live/Freeze state — **what the VIEW is doing, never the radio.** Ported from the
 * retired FAB's `fabState`, with a word added because the per-pane control is a labelled button
 * inside the picture rather than one unlabelled circle in the corner.
 */
export function liveState(following: boolean, atTuned = following): LiveState {
  if (following && !atTuned) {
    return {
      cls: "following", offTuned: true, label: "Off-window",
      title: "Following live time, but this viewport is off the tuned window — press to bring it to the tuned window's live edge.",
    };
  }
  return following
    ? {
      cls: "following", offTuned: false, label: "Live",
      title: "Following the live edge — press to freeze this viewport on what it shows. Capture never stops, paused or not.",
    }
    : {
      cls: "frozen", offTuned: false, label: "Frozen",
      title: "This viewport is frozen on a past window (capture continues) — press to follow the tuned window's live edge again.",
    };
}

/**
 * The follow/freeze arithmetic, **per pane id**, over a real `PaneModel`. Nothing here reads which
 * pane is active: every method is told which pane it acts on.
 *
 * `tunedWindow(id)` is THAT PANE's front end's own current centre/span (T-1006: the pinned radio's
 * `windows` entry, or `frequency.current` for a pane on `any`, off the navigation poll the host
 * already runs), read only for a button's state and on an explicit press — never at open
 * (see `surface/bootstrap.ts`'s recency-narrowed opening) and never as a background correction.
 * `onFollow` is told when a pane was moved to or off the live edge, so the map strip follows with
 * it, exactly as the FAB's `paneActions` did.
 */
export function paneLiveActions(
  panes: PaneLiveControl,
  tunedWindow?: (id: string) => TunedWindow | null,
  onFollow?: (on: boolean) => void,
) {
  // T-1006: the window is asked for BY PANE — each pane may be pinned to its own front end.
  const tuned = (id: string): TunedWindow | null => tunedWindow?.(id) ?? null;
  const atLiveEdge = (id: string): boolean => panes.atTunedLiveEdge(id, tuned(id));
  const followLive = (id: string): void => {
    // T-442: following is a coordinate change on the pane, nothing more — the SDR, the ring and
    // detection never paused, so there is nothing to resume anywhere but the screen.
    // T-955: and to the TUNED window's live edge — frequency too, when the pane has drifted off it.
    panes.followTuned(id, tuned(id));
    onFollow?.(true);
  };
  const pauseLive = (id: string): void => {
    // T-442/T-347: freezing writes down the window the pane was already showing — the frame you
    // pause on is identical to the one before it. The view stops; the capture does not.
    panes.setFollowing(id, false);
    onFollow?.(false);
  };
  return {
    isFollowing: (id: string) => panes.isFollowing(id),
    atLiveEdge,
    followLive,
    pauseLive,
    /** The button's press: freeze only a pane AT the tuned live edge; anywhere else, bring it there. */
    press(id: string): void {
      if (atLiveEdge(id)) pauseLive(id); else followLive(id);
    },
    /** What this pane's button must say, right now. */
    state: (id: string): LiveState => liveState(panes.isFollowing(id), atLiveEdge(id)),
  };
}

/** What the layer needs of a pane the frame drew: its id and its rectangle. */
export interface PaneLiveView { readonly id: string; readonly rect: PaneRect }

/** Where a pane's button sits inside its own rectangle: inset from the pane's top-right corner. */
export const LIVE_INSET_PX = 8;
/** The widest a button gets ("Off-window · pane 2"), used to ask what chrome is over that corner
 * before it is laid out — an over-estimate only moves the button further clear. */
export const BTN_MAX_W = 180;

/** A chrome box, in CSS px from the **canvas's** top-left — what [[chromeClearance]] measures. */
export interface ChromeRect { left: number; right: number; top: number; bottom: number; width: number; height: number }

/**
 * How far the floating chrome that sits over the **top** of the canvas reaches down, within the
 * horizontal range `[x0, x1]` — so a pane can place its Live button below whatever is actually
 * above it rather than under the Go-to row, the top-right cluster or the device chips.
 *
 * Measured from the cluster's own boxes, never assumed: which rows exist and how deep they stack
 * is CSS's answer and it changes with width (at 400 px the status chips wrap onto a second row —
 * the constant an earlier version of this used put the button under them). Only boxes in the top
 * third of the canvas count: the zoom stack at mid-height and the readouts at the bottom are not
 * "above" anything. Same spirit as `surface.ts`'s `chromeReserve` for the HUD's time labels.
 */
export function chromeClearance(rects: readonly ChromeRect[], x0: number, x1: number, canvasHpx: number): number {
  let band = 0;
  const limit = canvasHpx / 3;
  for (const r of rects) {
    if (r.width <= 0 || r.height <= 0) continue;
    if (r.top >= limit) continue;
    if (r.right <= x0 || r.left >= x1) continue;
    band = Math.max(band, r.bottom);
  }
  return band;
}

/** What the layer asks of its host each frame. No method takes "the active pane". */
export interface PaneLiveHost {
  state(id: string): LiveState;
  /** Toggle this pane's follow/freeze. The host also re-publishes the view (`viewChanged`). */
  press(id: string): void;
  /** The pane's 1-based position in layout order while there are two or more, else null — the same
   * name the outline and the rest of the per-pane chrome use (`active-pane.ts`'s `activePaneName`). */
  paneNumber(id: string): number | null;
}

/**
 * **The buttons themselves**: one per pane, pooled by pane id, placed inside the pane's rectangle
 * every render frame. The root is `pointer-events: none` like every other DOM layer over the canvas
 * (a gesture that starts on the picture still pans and zooms); the buttons themselves take the
 * pointer, because a button nobody can press is not a control.
 */
export class PaneLiveLayer {
  private readonly buttons = new Map<string, HTMLButtonElement>();
  /** The panes of the last `update`, so a press can re-state every button in the same call. */
  private shown: readonly string[] = [];

  constructor(private readonly root: HTMLElement, private readonly host: PaneLiveHost) {}

  /**
   * Place and state one button per pane, from THIS frame's rectangles. `chrome` is the floating
   * cluster's boxes in CSS px from the canvas's top-left, so a pane at the top of the canvas puts
   * its button below whatever chrome is above it ([[chromeClearance]]) rather than under it.
   */
  update(panes: readonly PaneLiveView[], canvasHpx: number, dpr: number, chrome: readonly ChromeRect[] = []): void {
    const ids = new Set(panes.map((p) => p.id));
    for (const [id, el] of this.buttons) {
      if (ids.has(id)) continue;
      el.remove();
      this.buttons.delete(id);
    }
    this.shown = panes.map((p) => p.id);
    for (const pane of panes) {
      const el = this.button(pane.id);
      const box = outlineBox(pane.rect, canvasHpx, dpr);
      // Top-right INSIDE the pane: the trace strip is taken off the pane's rectangle (view.ts), so
      // this corner is picture, not chrome. Pushed below whatever floating chrome is over that
      // corner. Placed with `transform`, like every other DOM layer.
      const right = box.left + box.width - LIVE_INSET_PX;
      const clear = chromeClearance(chrome, right - BTN_MAX_W, right, canvasHpx / (dpr > 0 ? dpr : 1));
      const top = Math.max(box.top + LIVE_INSET_PX, clear + LIVE_INSET_PX);
      el.style.transform = `translate(${right.toFixed(1)}px, ${top.toFixed(1)}px) translateX(-100%)`;
      this.state(pane.id, el);
    }
  }

  /** Re-state every button on screen without moving one — the tuned window changed, or a press. */
  sync(): void {
    for (const id of this.shown) {
      const el = this.buttons.get(id);
      if (el) this.state(id, el);
    }
  }

  /** The button of one pane, for a test or a key press that needs the very element the user sees. */
  buttonFor(id: string): HTMLButtonElement | null { return this.buttons.get(id) ?? null; }

  private button(id: string): HTMLButtonElement {
    const found = this.buttons.get(id);
    if (found) return found;
    const word = h("span", { class: "sf-pane-live-word" });
    // A small explicit <button> — the only place a view jump may live (docs/23 §10.6 rule 4).
    // Acts on ITS pane, whatever is active, and re-states every button in the same call so no
    // button on screen is one frame stale.
    const el = h("button", {
      type: "button", class: "sf-pane-live-btn", "data-pane-id": id,
      onclick: () => { this.host.press(id); this.sync(); },
    }, word) as HTMLButtonElement;
    this.root.appendChild(el);
    this.buttons.set(id, el);
    return el;
  }

  private state(id: string, el: HTMLButtonElement): void {
    const st = this.host.state(id);
    const n = this.host.paneNumber(id);
    const suffix = n === null ? "" : ` · pane ${n}`;
    el.classList.toggle("following", st.cls === "following");
    el.classList.toggle("frozen", st.cls === "frozen");
    el.classList.toggle("off-tuned", st.offTuned);
    el.dataset.state = st.offTuned ? "off-tuned" : st.cls;
    el.dataset.pane = n === null ? "" : String(n);
    el.setAttribute("aria-pressed", String(st.cls === "following" && !st.offTuned));
    const label = `${st.label}${suffix}`;
    const word = el.firstChild as HTMLElement;
    if (word.textContent !== label) word.textContent = label;
    if (el.title !== st.title) {
      el.title = st.title;
      el.setAttribute("aria-label", `Follow live${suffix}. ${st.title}`);
    }
  }
}
