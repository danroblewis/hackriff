// T-802 (MAP-02): the floating control cluster — Go-to, the layers button, the follow-live
// ("my location") FAB and the zoom stack (T-882 adds Measure, the viewport menu and the colour scale,
// rehomed from the retired toolbar row) — docked to the canvas's edges in SCREEN space (docs/23
// §10.1 band 2), translucent, and fading after ~6 s idle (§10.2). Reference layout:
// `ui/mockups/map-ui-v1.html`'s `#goto`, `.topright`, `.rightstack` and `#fab`.
//
// ## The view/device line (docs/23 §4, §10.4)
//
// Every control here is VIEW ARITHMETIC over the active pane — zoom, follow, go to, open a menu —
// and reaches no route. The single exception is the one the whole product already has: a Go-to
// that lands on spectrum no tuned window covers shows the retune OFFER (T-444/T-476's plan, the
// sentence it would paint on the pane's own row), and only an explicit press of that offer's
// button commands the radio, through the host's `pressOffer` → `acceptPaneRetune` → T-343's one
// gated `DeviceAction` path, which re-derives the offer at commit and refuses if the view moved.
// `ui/test/app-map-controls.test.ts` drives every control against a `fetch` spy and a spy client
// and asserts the call list stays empty.
//
// Thin client: nothing here knows what a signal, a tuning step or a capture window is. Frequency
// parsing is `controls/freq.ts`'s input formatting; the offer's words and its acceptability are
// computed by `surface/retune.ts` and handed in as strings.
import { parseFrequency } from "../../controls/freq";
import { swatchPixels, type LegendEntry } from "../../surface/legend";
import { h } from "../dom";
import { trackOverlay } from "./dismiss";
import { renderSettings, type SettingsHost } from "./settings";
import type { RowAction, WidthAction } from "../../surface/chrome";
import { registerMapHome } from "./top-chrome";
import { registerMapInvHome } from "./inv-home";

/** One zoom-button press scales both axes' spans by this (in) or its inverse (out) — the mockup's
 * step. The pane's own `zoomBoth` holds the aspect lock and the bounds (T-472), so a press at a
 * bound does nothing rather than distorting the view. */
export const ZOOM_STEP = 0.6;
/** Chrome fades after this long with no pointer, key, wheel or focus event (docs/23 §10.2). */
export const IDLE_MS = 6000;
/** The class on <body> while the chrome is idle-faded (T-824). The one idle signal the rest of the
 * floating chrome and the HUD read; set only by the cluster's [[IdleFade]]. */
export const IDLE_CLASS = "chrome-idle";

/** The subset of `surface/panes.ts`'s `PaneModel` the cluster drives. All of it is view state. */
export interface PaneControl {
  zoomBoth(id: string, factor: number, anchorF?: number, anchorT?: number): void;
  isFollowing(id: string): boolean;
  setFollowing(id: string, on: boolean): void;
  /** T-955: following in time AND showing the tuned window (`PaneModel.atTunedLiveEdge`). */
  atTunedLiveEdge(id: string, tuned: TunedWindow | null): boolean;
  /** T-955: follow, and move onto the tuned window only if not overlapping it (`PaneModel.followTuned`). */
  followTuned(id: string, tuned: TunedWindow | null): void;
}

/** The front end's own current centre/span (`frequency.current`), or null when none is reported. */
export interface TunedWindow { centerHz: number; spanHz: number }

/** What the Go-to offer shows: the words and acceptability `retune.ts` computed, and the press. */
export interface GotoOffer { why: string; enabled: boolean; press(): void }

/** One row of the layers menu: a base style (radio) or an overlay (checkbox). Display only. */
export interface LayerRow {
  id: string; label: string; hint: string; on: boolean;
  /** A key of the marks this layer draws (T-807's coverage fog), painted by the one cell rule. */
  key?: readonly LegendEntry[];
}

/**
 * What the layers menu shows (T-806 / MAP-06, docs/24 §4): two independent axes for the ACTIVE
 * pane — exactly one base style, any number of overlays in paint order — plus the few switches that
 * are view-wide rather than per pane (the spectrum-trace strip), stated as such. Built fresh from
 * the registry each time the menu renders; the rows are data, and every press goes back through
 * the host, which writes presentation state and reaches no route.
 */
export interface LayerMenu {
  /** How the menu names the pane it acts on ("pane 2 of 3"). */
  pane: string;
  bases: LayerRow[];
  /** The `data`-plane layers (T-807: the coverage fog) — flags on the one cell rule, painted below
   * every overlay. Optional so a host with none offers no section. */
  data?: LayerRow[];
  overlays: LayerRow[];
  viewWide: LayerRow[];
}

/** The menu's writes. Every one is presentation state; none can reach a route. */
export interface LayerMenuHost {
  layerMenu(): LayerMenu;
  setBase(id: string): void;
  /** Toggle one of the active pane's layers — an overlay, or a `data` row (the coverage fog). */
  toggleOverlay(id: string): void;
  toggleViewWide(id: string): void;
}

/**
 * The viewport menu's actions (T-882): pane management — Split, Close, Whole surface — which the
 * retired toolbar row carried and docs/26 gave no other home. All of it is pane arithmetic on the
 * view; `extras` are host-built items (Record IQ) appended below, whose own code states their route.
 */
export interface PaneMenuHost {
  split(): void;
  closePane(): void;
  wholeSurface(): void;
  /** How many panes exist, so the menu can say the last one never closes. */
  paneCount(): number;
  paneMenuExtras?: HTMLElement[];
}

export interface MapControlHost extends LayerMenuHost, PaneMenuHost, SettingsHost {
  zoom(factor: number): void;
  followLive(): void;
  /** T-955: is the active pane at the live edge of the TUNED window (time and frequency)? The FAB
   * freezes only then; anywhere else its press is `followLive`. Optional: absent = `isFollowing`. */
  atLiveEdge?(): boolean;
  /** Freeze the active pane on the window it shows — the FAB's other half (the retired `Live`
   * button's pause). A coordinate change on the pane; capture, the ring and detection never stop. */
  pauseLive(): void;
  isFollowing(): boolean;
  /** Measurement mode (T-822): whether a plain drag measures instead of panning. */
  measuring(): boolean;
  setMeasuring(on: boolean): void;
  /** T-820 (MAP-20): the Annotate / Pin tool modes — `null` when neither is on. Mutually exclusive
   * with Measure: the host keeps one tool mode, so a bare drag has one meaning. Optional, so a host
   * without annotation authoring shows neither button. */
  annotating?(): "annotate" | "pin" | null;
  setAnnotating?(mode: "annotate" | "pin" | null): void;
  /** Centre the active pane on `hz`, keeping its spans. A view move; never a retune. */
  goTo(hz: number): void;
  /** The tuned centre, for `+`/`-` relative entries (`parseFrequency`'s contract), or null. */
  centreHz(): number | null;
  /** The retune offer for the active pane when no tuned window covers it, else null. */
  gotoOffer(): GotoOffer | null;
  /**
   * T-996: the ACTIVE viewport's own persistent Retune (T-476) and capture-width presets (T-496),
   * rehomed here from the retired per-viewport panel — "the Retune button stays where Go-to lives"
   * (user, 2026-09-25). Strings and a bit, exactly as `RowAction`/`WidthAction` always were: this
   * cluster still learns nothing about tuning, and the press is a callback the host routes through
   * its own gated `DeviceAction` path. Asked EVERY FRAME (`syncRetune`), because the sentence names
   * the window the viewport is showing now — a label from a 1 s poll names somewhere it is not.
   */
  paneRetune?(): RowAction | null;
  pressPaneRetune?(): void;
  paneWidths?(): readonly WidthAction[];
  pressPaneWidth?(key: string): void;
  /** Tell the rest of the page the view moved (the surface's `mirror`). */
  viewChanged(): void;
  toast(text: string): void;
  /** T-821: the Research slide-in's toggle (open/close a panel — presentation only). */
  research?: { isOpen(): boolean; toggle(): void };
  /**
   * T-1000 (docs/23 §10.7): which pane the per-pane chrome acts on, as the user names it — its
   * position in layout order ("pane 2 of 3") — or `null` when there is one pane and so nothing to
   * disambiguate. Go-to, zoom, the layers button, the follow-live FAB and the viewport menu each
   * state it; the colour scale and the outputs are global and do not. Optional: a host with one
   * pane only names none.
   */
  activeName?(): { n: number; count: number; label: string } | null;
}

/** The tool-mode banner's words (the mockup's `#mode`), per mode (docs/23 §10.4). */
const MODE_TEXT = {
  measure: "Measure: drag on the surface to read Δf · Δt. Release to keep it. Esc exits.",
  annotate: "Annotate: drag to draw a box, click to drop a text note. Shift+drag still marks a region. Esc exits.",
  pin: "Pin: click to drop a marker. Drag still pans. Esc exits.",
} as const;

/**
 * The pane arithmetic behind the zoom stack and the FAB, over a real `PaneModel`. Split out of the
 * mount so the spy test drives the very code the page runs.
 *
 * Zoom anchors frequency at the pane's centre and time at the newest row when the pane follows
 * (so zooming never walks a live pane off the growing edge) and at the middle when it is frozen.
 *
 * `tunedWindow` (T-955) is the front end's OWN current centre/span (`frequency.current`, off the
 * navigation poll the host already runs), read only on an explicit follow-live press and for the
 * FAB's state — never at open (see `surface/bootstrap.ts`'s recency-narrowed opening) and never as a
 * background correction. The control's states are `PaneModel`'s: **at the tuned live edge**
 * (following in time and overlapping the tuned window) a press freezes; **anywhere else** — frozen,
 * or following the live edge of spectrum the radio has left — a press brings the pane to the tuned
 * window's live edge (`followTuned`), so it can never freeze a pane that was not at the edge (the
 * explorer's 0430 shot: "LIVE" at 162.2 MHz after a retune to 144.6, pressed, frozen at -14 s).
 * View arithmetic only — nothing here reaches a route. T-1001 moves this control into each pane and
 * reuses the same two `PaneModel` methods.
 */
export function paneActions(
  panes: PaneControl,
  activePane: () => string | null,
  onFollow?: (on: boolean) => void,
  tunedWindow?: () => TunedWindow | null,
) {
  const tuned = (): TunedWindow | null => tunedWindow?.() ?? null;
  const atLiveEdge = (): boolean => {
    const id = activePane();
    return !!id && panes.atTunedLiveEdge(id, tuned());
  };
  return {
    zoom(factor: number): void {
      const id = activePane();
      if (!id) return;
      panes.zoomBoth(id, factor, 0.5, panes.isFollowing(id) ? 1 : 0.5);
    },
    followLive(): void {
      const id = activePane();
      if (!id) return;
      // T-442: following is a coordinate change on the pane, nothing more — the SDR, the ring and
      // detection never paused, so there is nothing to resume anywhere but the screen.
      // T-955: and to the TUNED window's live edge — frequency too, when the pane has drifted off it.
      panes.followTuned(id, tuned());
      onFollow?.(true);
    },
    pauseLive(): void {
      const id = activePane();
      if (!id) return;
      // T-442: freezing writes down the window the pane was already showing — the frame you pause
      // on is identical to the one before it. The view stops; the capture does not.
      panes.setFollowing(id, false);
      onFollow?.(false);
    },
    isFollowing(): boolean {
      const id = activePane();
      return !!id && panes.isFollowing(id);
    },
    atLiveEdge,
  };
}

/**
 * **The FAB's press** (T-955): freeze only a pane AT the tuned live edge; a frozen pane, or one
 * following the live edge of spectrum the radio has left, is brought to the tuned window's live
 * edge instead. The explorer's 0430 shot is what the old `isFollowing ? pause : follow` did to a
 * pane reading "LIVE" at 162.2 MHz after a retune to 144.6: it froze it at -14 s.
 */
export function fabPress(host: Pick<MapControlHost, "atLiveEdge" | "isFollowing" | "pauseLive" | "followLive">): void {
  const atEdge = host.atLiveEdge ? host.atLiveEdge() : host.isFollowing();
  if (atEdge) host.pauseLive(); else host.followLive();
}

/** The FAB's words for the active pane's state — what the VIEW is doing, never the radio. */
export function fabState(following: boolean, atTuned = following): { cls: "following" | "frozen"; offTuned: boolean; title: string } {
  if (following && !atTuned) {
    // T-955: following in time over spectrum the radio has left — the press brings it to the tuned
    // window, it does not freeze, and the button must say so rather than read as plain "following".
    return { cls: "following", offTuned: true, title: "Following live time, but this viewport is off the tuned window — press to bring it to the tuned window's live edge." };
  }
  return following
    ? { cls: "following", offTuned: false, title: "Following the live edge — press to freeze this viewport on what it shows. Capture never stops, paused or not." }
    : { cls: "frozen", offTuned: false, title: "This viewport is frozen on a past window (capture continues) — press to follow the tuned window's live edge again." };
}

/** Parse a Go-to entry: a frequency (`433.92M`, `101.3`, `+200k` relative to the tuned centre). */
export function parseGoto(text: string, centreHz: number | null): { hz: number } | { error: string } {
  const hz = parseFrequency(text, centreHz ?? undefined);
  return hz === null ? { error: "Enter a frequency, e.g. 433.92M or 101.3." } : { hz };
}

/**
 * The idle fade, as a state machine over an injected timer so it is testable without a DOM.
 *
 * `poke()` on any activity; `hold(key, on)` pins the chrome visible while something that must never
 * fade is up (an open menu, a focused control — docs/23 §10.2). Idle is reported only on change.
 */
export class IdleFade {
  private timer: unknown = null;
  private idle = false;
  private readonly holds = new Set<string>();
  constructor(
    private readonly onChange: (idle: boolean) => void,
    private readonly ms = IDLE_MS,
    private readonly timers: { set(fn: () => void, ms: number): unknown; clear(t: unknown): void } = {
      set: (fn, ms) => setTimeout(fn, ms),
      clear: (t) => clearTimeout(t as ReturnType<typeof setTimeout>),
    },
  ) { this.arm(); }

  get isIdle(): boolean { return this.idle; }

  poke(): void {
    this.set(false);
    this.arm();
  }

  hold(key: string, on: boolean): void {
    if (on) this.holds.add(key); else this.holds.delete(key);
    this.poke();
  }

  dispose(): void { if (this.timer !== null) this.timers.clear(this.timer); this.timer = null; }

  private arm(): void {
    if (this.timer !== null) this.timers.clear(this.timer);
    this.timer = this.holds.size > 0 ? null : this.timers.set(() => { this.timer = null; this.set(true); }, this.ms);
  }

  private set(idle: boolean): void {
    if (this.idle === idle) return;
    this.idle = idle;
    this.onChange(idle);
  }
}

type Shape = ["path", string] | ["circle", number, number, number];
/** An icon from author-written shapes, built with `createElementNS` (never `innerHTML` — dom.ts's rule). */
const svg = (...shapes: Shape[]): SVGSVGElement => {
  const NS = "http://www.w3.org/2000/svg";
  const s = document.createElementNS(NS, "svg");
  s.setAttribute("viewBox", "0 0 24 24");
  s.setAttribute("aria-hidden", "true");
  for (const sh of shapes) {
    const e = document.createElementNS(NS, sh[0]);
    if (sh[0] === "path") e.setAttribute("d", sh[1]);
    else { e.setAttribute("cx", String(sh[1])); e.setAttribute("cy", String(sh[2])); e.setAttribute("r", String(sh[3])); }
    s.append(e);
  }
  return s;
};

/** Swatch size for a layer's key, CSS px. */
const KEY_W = 22, KEY_H = 12;

/**
 * A layer's key: one small swatch per mark, each painted by `swatchPixels` — the same `cellPixel`
 * rule the shader is generated from, so the menu cannot show a mark the canvas does not draw.
 */
function layerKey(entries: readonly LegendEntry[]): HTMLElement {
  return h("ul", { class: "map-layer-key", "aria-label": "What each mark means" }, ...entries.map((e) => {
    const c = h("canvas", { class: "map-key-swatch", width: KEY_W, height: KEY_H, "aria-hidden": "true" }) as HTMLCanvasElement;
    const ctx = typeof c.getContext === "function" ? c.getContext("2d") : null;
    if (ctx) {
      const img = ctx.createImageData(KEY_W, KEY_H);
      img.data.set(swatchPixels(e, KEY_W, KEY_H));
      ctx.putImageData(img, 0, 0);
    }
    return h("li", { "data-mark": e.key, title: e.note }, c, e.label);
  }));
}

/**
 * Build the cluster. Returns the element (the host appends it over the canvas) and two hooks the
 * host calls: `viewMoved()` when a gesture moved the view (a Go-to offer describes a window the pane
 * has now left, so it is withdrawn), and `syncFollow()` when the follow state may have changed.
 */
export function mountMapControls(host: MapControlHost): {
  el: HTMLElement; viewMoved(): void; syncFollow(): void; syncLayers(): void; syncMeasure(): void;
  syncResearch(): void;
  /** T-996: re-read the active viewport's Retune / width offers. Called per RENDER FRAME. */
  syncRetune(): void;
  tuningChanged(): void;
  /** T-1000: re-state which pane the per-pane chrome acts on (after an active-pane change, split or close). */
  syncActive(): void;
  /** T-1000: the FAB's press, for the `L` key — freeze a following active pane, re-pin a frozen one. */
  toggleFollow(): void;
} {
  const input = h("input", {
    class: "mono", placeholder: "Go to frequency, e.g. 433.92M or 101.3", "aria-label": "Go to frequency",
    inputmode: "decimal", autocomplete: "off", spellcheck: "false",
  }) as HTMLInputElement;
  // T-1000: which pane a Go-to moves, stated beside the entry while there is more than one.
  const gotoPane = h("span", { class: "map-goto-pane", hidden: true });
  const goto = h("form", { class: "map-glass map-goto map-fade", role: "search", autocomplete: "off" },
    svg(["circle", 11, 11, 7], ["path", "M20 20l-3.5-3.5"]), input, gotoPane,
    h("span", { class: "map-hint", "aria-hidden": "true" }, "↵"));

  // The retune offer never fades (docs/23 §10.2), so it carries no `map-fade`.
  const offerWhy = h("span", { class: "map-offer-why" });
  const offerGo = h("button", { type: "button", class: "map-offer-go" }, "Retune") as HTMLButtonElement;
  const offerX = h("button", { type: "button", class: "map-offer-x", "aria-label": "Dismiss the retune offer" }, "✕");
  const offer = h("div", { class: "map-glass map-offer", role: "status", hidden: true }, offerWhy, offerGo, offerX);
  let shown: GotoOffer | null = null;
  // T-900: the offer is a transient on the one overlay stack — Escape dismisses it when topmost.
  const offerOverlay = trackOverlay("retune-offer", () => hideOffer());
  const hideOffer = () => { shown = null; offer.hidden = true; offerOverlay.open(false); };

  // T-996: the ACTIVE viewport's persistent capture controls, under Go-to — "the Retune button
  // stays where Go-to lives", beside the nudges and the Go-to offer, which is where every device
  // command on this map already is. Small and never faded (it is a device command with a stated
  // destination, docs/23 §10.2), and shown only while the host offers one for this viewport.
  const retuneWhy = h("span", { class: "map-retune-why" });
  const retuneGo = h("button", { type: "button", class: "map-retune-go" }, "Retune") as HTMLButtonElement;
  const widthRow = h("div", { class: "map-widths", role: "group", "aria-label": "Capture width" });
  const retune = h("div", { class: "map-glass map-retune", role: "group", "aria-label": "Capture this viewport", hidden: true },
    h("div", { class: "map-retune-row" }, retuneGo, retuneWhy), widthRow);

  const layersBtn = h("button", {
    type: "button", class: "map-ibtn map-layers-btn", "aria-label": "Layers", title: "Layers",
    "aria-pressed": "false", "aria-expanded": "false", "aria-controls": "map-layers",
  }, svg(["path", "M12 3l9 5-9 5-9-5 9-5z"], ["path", "M3 12l9 5 9-5"], ["path", "M3 16l9 5 9-5"])) as HTMLButtonElement;
  // T-821 (MAP-21): the Research slide-in — collections, and every mark as a row.
  const researchBtn = h("button", {
    type: "button", class: "map-ibtn map-research-btn", "aria-label": "Research", title: "Research: collections and their marks",
    "aria-pressed": "false", hidden: !host.research,
  }, svg(["path", "M4 5h16M4 12h16M4 19h16"], ["path", "M8 3v18"])) as HTMLButtonElement;
  // T-882: Measure (T-822) and the viewport menu join Layers top-right, as in the mockup's
  // `.topright` (`#measure-btn`). Fixed positions, not draggable (the coordinator's note on T-882).
  const measureBtn = h("button", {
    type: "button", class: "map-ibtn map-measure-btn", "aria-label": "Measure", "aria-pressed": "false",
    title: "Measure: drag on the surface to read Δf/Δt between two points and save it. Esc exits.",
  }, svg(["path", "M3 17l14-14 4 4L7 21H3v-4z"], ["path", "M13 7l2 2M10 10l2 2M7 13l2 2"])) as HTMLButtonElement;
  // T-820 (MAP-20): Annotate (bare drag = a box, bare click = a text note) and Pin (bare click = a
  // marker; drag still pans), the other two columns of docs/23 §10.4's table, beside Measure. Each
  // asks for a label; the surface mount saves it (an authoring act, never a device route).
  const annotateBtn = h("button", {
    type: "button", class: "map-ibtn map-annotate-btn", "aria-label": "Annotate", "aria-pressed": "false",
    hidden: !host.setAnnotating,
    title: "Annotate: drag on the surface to draw an annotation box, or click to drop a text note; "
      + "you are asked for its label, and it is saved as an annotation (docs/25 §5). Esc exits.",
  }, svg(["path", "M4 4h16v12H8l-4 4z"], ["path", "M8 8h8M8 12h5"])) as HTMLButtonElement;
  const pinBtn = h("button", {
    type: "button", class: "map-ibtn map-pin-btn", "aria-label": "Pin", "aria-pressed": "false",
    hidden: !host.setAnnotating,
    title: "Pin: click on the surface to drop a labelled marker there, saved as an annotation. Dragging still pans. Esc exits.",
  }, svg(["path", "M6 21V4"], ["path", "M6 4h12l-3 4 3 4H6"])) as HTMLButtonElement;
  const paneBtn = h("button", {
    type: "button", class: "map-ibtn map-pane-btn", "aria-label": "Viewport", title: "Viewport: split, close, whole surface, record",
    "aria-pressed": "false", "aria-expanded": "false", "aria-controls": "map-pane-menu",
  }, svg(["path", "M4 5h16v14H4z"], ["path", "M12 5v14"])) as HTMLButtonElement;
  // T-993: the retired top bar's Review button is moved in here (`top-chrome.ts`), beside the other
  // panels' buttons, and Theme into a small ⋯ menu — settings, not a bar button.
  const reviewHome = h("span", { class: "map-review-home" });
  const moreBtn = h("button", {
    type: "button", class: "map-ibtn map-more-btn", "aria-label": "More: settings", title: "More: theme and settings",
    "aria-pressed": "false", "aria-expanded": "false", "aria-controls": "map-more-menu",
  }, svg(["circle", 5, 12, 1.2], ["circle", 12, 12, 1.2], ["circle", 19, 12, 1.2])) as HTMLButtonElement;
  const topright = h("div", { class: "map-chips map-topright map-fade" }, layersBtn, researchBtn, measureBtn, annotateBtn, pinBtn, paneBtn, reviewHome, moreBtn);
  const moreClose = h("button", {
    type: "button", class: "map-layers-close map-more-close", "aria-label": "Close the settings menu — back to the map", title: "Close (Esc)",
  }, "×") as HTMLButtonElement;
  // T-1007: the ⋯ menu is THE settings menu — Theme (the node T-993 moved here) plus every
  // preference that used to sit in Review: the colour scale (and with it auto-contrast), the time
  // ruler's labels (T-998's seconds-ago / local-clock preference, `surface/hud.ts`'s
  // `TimeLabelMode`, offered here as one radio group rather than a second toggle), the front ends
  // and which viewport's grey reads which, and the capture window the backend reports. `./settings.ts` renders the groups; each press is presentation state.
  const moreBody = h("div", { class: "map-more-body" });
  const settingsList = h("div", { class: "map-settings-rows" });
  const moreMenu = h("div", { class: "map-glass map-pane-menu map-more-menu", id: "map-more-menu", role: "group", "aria-label": "Settings", hidden: true },
    h("div", { class: "map-layers-head" }, h("span", {}, "Settings"), moreClose), moreBody, settingsList);
  // T-993: the retired bar's other homes. T-1025: the mode switch and the device/stream state are
  // SEPARATE chips in a `map-chips` row that paints nothing of its own and takes no pointer, so the
  // canvas shows (and drags) between them; the tuning nudges (T-409, device commands through the one gated DeviceAction path) sit
  // under Go-to, where the retune offer — the other device command on the map — already lives.
  const statusHome = h("div", { class: "map-chips map-status map-fade", role: "group", "aria-label": "View and device", hidden: true });
  const nudgeHome = h("div", { class: "map-glass map-nudge map-fade", hidden: true });
  // T-997: the inventory pills' row, under Go-to and the nudges in the left stack — the place the
  // mid-height chip over the time ruler was retired from. Filled by `chrome/inv-pills.ts` (which may
  // mount before or after this cluster), and hidden until it is: an empty glass box is chrome that
  // says nothing.
  const invHome = h("div", { class: "map-glass map-inv map-fade", role: "group", "aria-label": "Signal lists", hidden: true });
  const paneItem = (act: string, label: string, title: string, run: () => void) => {
    const b = h("button", { type: "button", class: "map-pane-item", "data-pane-act": act, title }, label) as HTMLButtonElement;
    // A menu item acts and closes the menu, like any menu; Record IQ (a host extra) keeps it open so
    // its Stop is where the start was.
    b.addEventListener("click", () => { run(); setPaneOpen(false); });
    return b;
  };
  // T-900 (P1): the viewport menu is an overlay too — a visible dismiss, and Esc via the one stack.
  const paneClose = h("button", {
    type: "button", class: "map-layers-close map-pane-close", "aria-label": "Close the viewport menu — back to the map", title: "Close (Esc)",
  }, "×") as HTMLButtonElement;
  const closeItem = paneItem("close", "Close viewport", "Close the active viewport. The last one never closes.", () => host.closePane());
  const paneHead = h("span", {}, "Viewport");
  const paneMenu = h("div", { class: "map-glass map-pane-menu", id: "map-pane-menu", role: "group", "aria-label": "Viewport", hidden: true },
    h("div", { class: "map-layers-head" }, paneHead, paneClose),
    paneItem("split", "Split ⇔", "Two viewports onto the same surface, side by side. They show the identical box until one is moved. The new one starts with this viewport's layers and diverges as you toggle.", () => host.split()),
    closeItem,
    paneItem("whole", "Whole surface", "Zoom the active viewport out to the device-available spectrum over the whole record horizon (never less than the retained capture window).", () => host.wholeSurface()),
    ...(host.paneMenuExtras ?? []),
    h("div", { class: "map-note" }, "View only: splitting, closing and zooming out never command the radio."));
  // The mockup's `#mode` banner: while a tool mode is on (Measure, or T-820's Annotate / Pin), say
  // what a drag will do and how to leave. Never faded.
  const modeBanner = h("div", { class: "map-glass map-mode", role: "status", hidden: true }, MODE_TEXT.measure);
  const layersList = h("div", { class: "map-layers-rows" });
  // T-900 (docs/23 §10.6 P1): an open menu is an overlay, so it has a visible dismiss, not just a fade.
  const layersClose = h("button", {
    type: "button", class: "map-layers-close", "aria-label": "Close layers — back to the map", title: "Close (Esc)",
  }, "×") as HTMLButtonElement;
  const layersHead = h("span", {}, "Layers");
  const layers = h("div", { class: "map-glass map-layers", id: "map-layers", role: "group", "aria-label": "Layers", hidden: true },
    h("div", { class: "map-layers-head" }, layersHead, layersClose),
    layersList,
    h("div", { class: "map-note" }, "Display only: a layer changes what is drawn, never what is measured or detected."));

  const zoomIn = h("button", { type: "button", class: "map-ibtn map-zoom-in", "aria-label": "Zoom in", title: "Zoom in (both axes)" },
    svg(["path", "M12 5v14M5 12h14"]));
  const zoomOut = h("button", { type: "button", class: "map-ibtn map-zoom-out", "aria-label": "Zoom out", title: "Zoom out (both axes)" },
    svg(["path", "M5 12h14"]));
  // T-1000: the number of the pane the zoom stack acts on, while there is more than one.
  const zoomPane = h("div", { class: "map-pane-badge map-zoom-pane", "aria-hidden": "true", hidden: true });
  const zoom = h("div", { class: "map-glass map-zoom map-fade", role: "group", "aria-label": "Zoom" },
    zoomPane, zoomIn, h("div", { class: "map-sep", "aria-hidden": "true" }), zoomOut);

  const fabPane = h("span", { class: "map-pane-badge", "aria-hidden": "true", hidden: true });
  const fab = h("button", { type: "button", class: "map-fab map-fade", "aria-label": "Follow live" },
    svg(["circle", 12, 12, 3], ["path", "M12 2v4M12 18v4M2 12h4M18 12h4"], ["circle", 12, 12, 8]), fabPane) as HTMLButtonElement;
  const layersPane = h("span", { class: "map-pane-badge", "aria-hidden": "true", hidden: true });
  layersBtn.append(layersPane);

  // T-996: the left column under the nudges is a STACK, not a set of fixed `top:` constants — the
  // T-997 inventory pills, the persistent capture block, the transient Go-to offer and (below
  // 1180 px) the tool-mode banner are each as tall as their words make them, so a fixed top per
  // block is exactly how the offer came to be drawn over the width presets at 400 px. In flow, each
  // starts where the last ended; the pills lead it, so their one fixed place under Go-to holds.
  const stack = h("div", { class: "map-stack" }, invHome, retune, offer, modeBanner);
  const el = h("div", { class: "map-ctl", "data-band": "chrome" }, goto, nudgeHome, stack, statusHome, topright, layers, paneMenu, moreMenu, zoom, fab);

  // T-824 (MAP-24): the idle state is also stated once on <body> (`chrome-idle`), so every other
  // piece of floating chrome — the top bar, the dock, the lists' chip (`chrome/phone.css`) and the
  // HUD rulers (`centre/surface.ts`) — fades and returns with this cluster on the same timer. One
  // timer, one class: two would let the bar come back while the zoom stack stayed faded.
  const fade = new IdleFade((idle) => {
    el.classList.toggle("is-idle", idle);
    if (typeof document !== "undefined") document.body?.classList.toggle(IDLE_CLASS, idle);
  });
  const poke = () => fade.poke();
  for (const ev of ["pointermove", "pointerdown", "keydown", "wheel", "touchstart", "focusin"]) {
    window.addEventListener(ev, poke, { passive: true, capture: true });
  }
  // A focused control never fades (§10.2): hold while focus is inside the cluster.
  el.addEventListener("focusin", () => fade.hold("focus", true));
  el.addEventListener("focusout", (e) => {
    if (!el.contains((e as FocusEvent).relatedTarget as Node | null)) fade.hold("focus", false);
  });

  const syncFollow = () => {
    const following = host.isFollowing();
    const st = fabState(following, host.atLiveEdge ? host.atLiveEdge() : following);
    fab.classList.toggle("following", st.cls === "following");
    fab.classList.toggle("frozen", st.cls === "frozen");
    fab.classList.toggle("off-tuned", st.offTuned);
    fab.setAttribute("aria-pressed", String(st.cls === "following" && !st.offTuned));
    const name = host.activeName?.() ?? null;
    fab.title = name ? `${st.title} (Acts on ${name.label}; L toggles it.)` : st.title;
  };

  // T-1000 (docs/23 §10.7): every control that acts on ONE pane says which, while there are two or
  // more; with one pane it says nothing extra. The words are the host's (`activeName`), so the
  // chrome and the outline on the canvas cannot name different panes. Set-if-changed: cheap enough
  // to call on every active-pane change, split and close.
  let namedAs = "";
  const syncActive = () => {
    const name = host.activeName?.() ?? null;
    const key = name ? name.label : "";
    if (key === namedAs) return;
    namedAs = key;
    const n = name ? String(name.n) : "";
    const suffix = name ? ` · ${name.label}` : "";
    for (const b of [zoomPane, fabPane, layersPane]) { b.hidden = !name; b.textContent = n; }
    gotoPane.hidden = !name;
    // "pane N", with the word droppable at a phone width (CSS) so the entry keeps its room.
    gotoPane.replaceChildren(...(name ? [h("span", { class: "map-goto-word" }, "pane "), n] : []));
    input.setAttribute("aria-label", `Go to frequency${suffix}`);
    goto.dataset.pane = n;
    layersBtn.setAttribute("aria-label", `Layers${suffix}`);
    layersBtn.title = `Layers${suffix}`;
    layersHead.textContent = name ? `Layers · ${name.label}` : "Layers";
    zoom.setAttribute("aria-label", `Zoom${suffix}`);
    zoom.dataset.pane = n;
    zoomIn.setAttribute("title", `Zoom in (both axes)${suffix}`);
    zoomOut.setAttribute("title", `Zoom out (both axes)${suffix}`);
    fab.setAttribute("aria-label", `Follow live${suffix}`);
    fab.dataset.pane = n;
    paneHead.textContent = name ? `Viewport · ${name.label}` : "Viewport";
    syncFollow();
  };

  goto.addEventListener("submit", (e) => {
    e.preventDefault();
    const r = parseGoto(input.value, host.centreHz());
    if ("error" in r) { host.toast(r.error); return; }
    host.goTo(r.hz);
    host.viewChanged();
    // After the move: the offer for where the pane now IS, and only if no tuned window covers it.
    shown = host.gotoOffer();
    if (!shown) { hideOffer(); return; }
    offerWhy.textContent = shown.why;
    offerGo.disabled = !shown.enabled;
    offer.hidden = false;
    offerOverlay.open(true);
  });
  offerGo.addEventListener("click", () => {
    const o = shown;
    hideOffer();
    // THE explicit press: the offer object that was painted is the one pressed (T-476's rule).
    if (o && o.enabled) o.press();
  });
  offerX.addEventListener("click", hideOffer);

  const renderLayers = () => {
    const m = host.layerMenu();
    // A re-render replaces the inputs; keep keyboard focus on the one that was pressed.
    const act = document.activeElement as HTMLElement | null;
    const refocus = act && layersList.contains(act)
      ? (act.dataset.base ? `[data-base="${act.dataset.base}"]` : act.dataset.layer ? `[data-layer="${act.dataset.layer}"]`
        : act.dataset.viewLayer ? `[data-view-layer="${act.dataset.viewLayer}"]` : null)
      : null;
    const row = (l: LayerRow, input: HTMLInputElement, press: () => void) => {
      input.checked = l.on;
      input.addEventListener("change", () => { press(); renderLayers(); });
      const label = h("label", { class: "map-row" }, input, l.label, h("small", {}, l.hint));
      return l.key?.length ? h("div", {}, label, layerKey(l.key)) : label;
    };
    layersList.replaceChildren(
      h("div", { class: "map-layers-axis", "data-axis": "base", role: "radiogroup", "aria-label": `Base style, ${m.pane}` },
        h("h4", {}, `Base style · ${m.pane} · one at a time`),
        ...m.bases.map((l) => row(l,
          h("input", { type: "radio", name: "map-base", value: l.id, "data-base": l.id }) as HTMLInputElement,
          () => host.setBase(l.id)))),
      ...(m.data?.length ? [h("div", { class: "map-layers-axis", "data-axis": "data", role: "group", "aria-label": `Coverage, ${m.pane}` },
        h("h4", {}, `Coverage · ${m.pane} · under every overlay`),
        ...m.data.map((l) => row(l,
          h("input", { type: "checkbox", "data-layer": l.id }) as HTMLInputElement,
          () => host.toggleOverlay(l.id))))] : []),
      h("div", { class: "map-layers-axis", "data-axis": "overlays", role: "group", "aria-label": `Overlays, ${m.pane}` },
        h("h4", {}, `Overlays · ${m.pane} · paint order ↓`),
        ...m.overlays.map((l) => row(l,
          h("input", { type: "checkbox", "data-layer": l.id }) as HTMLInputElement,
          () => host.toggleOverlay(l.id)))),
      ...(m.viewWide.length ? [h("div", { class: "map-layers-axis", "data-axis": "view-wide", role: "group", "aria-label": "Every pane" },
        h("h4", {}, "Every pane"),
        ...m.viewWide.map((l) => row(l,
          h("input", { type: "checkbox", "data-view-layer": l.id }) as HTMLInputElement,
          () => host.toggleViewWide(l.id))))] : []),
      h("div", { class: "map-layers-note" }, "Unknowns are never hidden by default. Toggling a layer changes this pane's picture only — never what is captured or detected."),
    );
    if (refocus) (layersList.querySelector(refocus) as HTMLElement | null)?.focus();
  };
  let layersOpen = false;
  const layersOverlay = trackOverlay("layers", () => { setLayersOpen(false); layersBtn.focus(); });
  const paneOverlay = trackOverlay("pane-menu", () => { setPaneOpen(false); paneBtn.focus(); });
  let paneOpen = false;
  const syncPaneMenu = () => {
    const last = host.paneCount() <= 1;
    closeItem.disabled = last;
    closeItem.title = last ? "The last viewport never closes." : "Close the active viewport. The last one never closes.";
  };
  const setPaneOpen = (open: boolean) => {
    paneOpen = open;
    paneOverlay.open(open);
    paneMenu.hidden = !open;
    paneBtn.setAttribute("aria-pressed", String(open));
    paneBtn.setAttribute("aria-expanded", String(open));
    if (open) { syncPaneMenu(); if (layersOpen) setLayersOpen(false); if (moreOpen) setMoreOpen(false); }
    fade.hold("pane-menu", open); // an open menu never fades
  };
  let moreOpen = false;
  const moreOverlay = trackOverlay("more-menu", () => { setMoreOpen(false); moreBtn.focus(); });
  const setMoreOpen = (open: boolean) => {
    moreOpen = open;
    moreOverlay.open(open);
    moreMenu.hidden = !open;
    moreBtn.setAttribute("aria-pressed", String(open));
    moreBtn.setAttribute("aria-expanded", String(open));
    if (open) { renderSettings(settingsList, host, () => setMoreOpen(false)); if (layersOpen) setLayersOpen(false); if (paneOpen) setPaneOpen(false); }
    fade.hold("more-menu", open); // an open menu never fades
  };
  moreBtn.addEventListener("click", () => setMoreOpen(!moreOpen));
  moreClose.addEventListener("click", () => { setMoreOpen(false); moreBtn.focus(); });
  const setLayersOpen = (open: boolean) => {
    layersOpen = open;
    layersOverlay.open(open);
    if (open && paneOpen) setPaneOpen(false);
    if (open && moreOpen) setMoreOpen(false);
    layers.hidden = !open;
    layersBtn.setAttribute("aria-pressed", String(open));
    layersBtn.setAttribute("aria-expanded", String(open));
    if (open) renderLayers();
    fade.hold("layers", open); // an open menu never fades
  };
  layersBtn.addEventListener("click", () => setLayersOpen(!layersOpen));
  layersClose.addEventListener("click", () => { setLayersOpen(false); layersBtn.focus(); });
  paneBtn.addEventListener("click", () => setPaneOpen(!paneOpen));
  paneClose.addEventListener("click", () => { setPaneOpen(false); paneBtn.focus(); });

  const syncMeasure = () => {
    const on = host.measuring();
    const anno = host.annotating?.() ?? null;
    measureBtn.setAttribute("aria-pressed", String(on));
    annotateBtn.setAttribute("aria-pressed", String(anno === "annotate"));
    pinBtn.setAttribute("aria-pressed", String(anno === "pin"));
    const mode = on ? "measure" : anno;
    modeBanner.hidden = mode === null;
    if (mode !== null && modeBanner.textContent !== MODE_TEXT[mode]) modeBanner.textContent = MODE_TEXT[mode];
  };
  measureBtn.addEventListener("click", () => { host.setMeasuring(!host.measuring()); syncMeasure(); });
  const toggleAnno = (mode: "annotate" | "pin") => {
    host.setAnnotating?.(host.annotating?.() === mode ? null : mode);
    syncMeasure();
  };
  annotateBtn.addEventListener("click", () => toggleAnno("annotate"));
  pinBtn.addEventListener("click", () => toggleAnno("pin"));

  const zoomBy = (k: number) => { host.zoom(k); hideOffer(); host.viewChanged(); };
  zoomIn.addEventListener("click", () => zoomBy(ZOOM_STEP));
  zoomOut.addEventListener("click", () => zoomBy(1 / ZOOM_STEP));
  // The FAB is the retired `Live` button too (T-882): following → press freezes the view on what it
  // shows; frozen → press re-pins it to the growing edge. Either way only the screen changes.
  // The one toggle, shared by the FAB's press and the `L` key (T-1000), so the two cannot differ.
  // (T-955: "following" here means at the TUNED live edge - see `paneActions`/`fabPress`.)
  const toggleFollow = () => {
    fabPress(host);
    // T-955: follow-live can move the pane's frequency too, so a painted Go-to offer now describes
    // a window the pane has left - withdrawn, exactly as a zoom withdraws it.
    hideOffer();
    host.viewChanged();
    syncFollow();
  };
  fab.addEventListener("click", toggleFollow);

  // ---- T-996: the active viewport's Retune and width presets, re-asked every frame ----
  //
  // Same discipline the per-viewport row had (T-476/T-496/T-407): buttons are created once and only
  // ever UPDATED — a button rebuilt each frame is a button that cannot be pressed, because the
  // element under the finger between pointerdown and pointerup would be a different one — and each
  // width button reads its `key` fresh from its own record, never a value closed over at mint time,
  // so the list can change WHICH preset a position stands for without the listener naming a stale
  // one. A refusal is STATED (disabled, with `why` beside it and as the title), never hidden: a
  // control that vanishes teaches nothing, which is the complaint T-476 exists for.
  const widthBtns: { el: HTMLButtonElement; key: string }[] = [];
  const setText = (e: HTMLElement, t: string) => { if (e.textContent !== t) e.textContent = t; };
  const apply = (b: HTMLButtonElement, a: RowAction) => {
    setText(b, a.label);
    if (b.title !== a.why) b.title = a.why;
    b.disabled = !a.enabled;
    b.setAttribute("aria-disabled", a.enabled ? "false" : "true");
  };
  const syncRetune = () => {
    const a = host.paneRetune?.() ?? null;
    if (retune.hidden === !!a) retune.hidden = !a;
    if (a) { apply(retuneGo, a); setText(retuneWhy, a.why); }
    const ws = host.paneWidths?.() ?? [];
    while (widthBtns.length < ws.length) {
      const rec = { el: h("button", { type: "button", class: "map-width" }) as HTMLButtonElement, key: "" };
      rec.el.addEventListener("click", () => { if (!rec.el.disabled) host.pressPaneWidth?.(rec.key); });
      widthBtns.push(rec);
      widthRow.append(rec.el);
    }
    while (widthBtns.length > ws.length) widthBtns.pop()!.el.remove();
    if (widthRow.hidden !== (ws.length === 0)) widthRow.hidden = ws.length === 0;
    ws.forEach((w, i) => { widthBtns[i].key = w.key; apply(widthBtns[i].el, w); });
  };
  retuneGo.addEventListener("click", () => { if (!retuneGo.disabled) host.pressPaneRetune?.(); });

  const syncResearch = () => researchBtn.setAttribute("aria-pressed", String(!!host.research?.isOpen()));
  researchBtn.addEventListener("click", () => { host.research?.toggle(); syncResearch(); });
  syncResearch();

  syncFollow();
  syncActive();
  syncMeasure();
  syncRetune();
  registerMapHome({ status: statusHome, nudge: nudgeHome, review: reviewHome, more: moreBody });
  registerMapInvHome(invHome);
  /** Re-render an open menu — the active pane changed, or a toggle elsewhere changed a layer. The
   * settings menu re-states too: its range sentence follows auto-contrast, and its front-end list
   * follows the control-state poll. */
  const syncLayers = () => {
    if (layersOpen) renderLayers();
    if (paneOpen) syncPaneMenu();
    if (moreOpen) renderSettings(settingsList, host, () => setMoreOpen(false));
  };
  /**
   * T-955: **the front end's tuned window changed** (a retune by this page, another client or the
   * API). A painted Go-to offer was computed against the OLD tuning — "Retune to 162.2 MHz" still on
   * screen after the radio went to 162.2 and then 144.6 (explorer 0428) — so it is re-derived against
   * the new one: withdrawn if a tuned window now covers the pane, re-worded if not. Never pressed
   * here; the press still re-derives at commit and refuses if the view moved. The FAB's state
   * depends on the tuned window too, so it is re-stated.
   */
  const tuningChanged = () => {
    if (shown) {
      shown = host.gotoOffer();
      if (!shown) hideOffer();
      else { offerWhy.textContent = shown.why; offerGo.disabled = !shown.enabled; }
    }
    syncFollow();
  };
  // A gesture moved the view: the Go-to offer describes a window the pane has left, and whether
  // the pane still shows the tuned window (the FAB's state, T-955) may have changed with it.
  const viewMoved = () => { hideOffer(); syncFollow(); };
  return { el, viewMoved, syncFollow, syncLayers, syncMeasure, syncResearch, syncRetune, syncActive, toggleFollow, tuningChanged };
}
