// T-802 (MAP-02): the floating control cluster — Go-to, the layers button and the zoom stack
// (T-882 adds Measure, the viewport menu and the colour scale, rehomed from the retired toolbar
// row) — docked to the canvas's edges in SCREEN space (docs/23 §10.1 band 2), translucent, and
// fading after ~6 s idle (§10.2). Reference layout: `ui/mockups/map-ui-v1.html`'s `#goto`,
// `.topright` and `.rightstack`.
//
// **T-1001 retired the follow-live FAB** (the mockup's `#fab`): with two panes open, one corner
// button acting on the hidden active pane could not say which pane it froze. Live/Freeze is now a
// button inside each pane's own rectangle — `centre/pane-live.ts`. Nothing in this module follows,
// freezes or states a live edge any more.
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
  /** The part of a uniform zoom both axes can take (`PaneModel.lockedZoomFactor`, T-472). */
  lockedZoomFactor(id: string, factor: number): number;
  zoomFreq(id: string, factor: number, anchor?: number): void;
  zoomTime(id: string, factor: number, anchor?: number): void;
  /** Read only to anchor a zoom on the growing edge — follow/freeze itself is `pane-live.ts`'s. */
  isFollowing(id: string): boolean;
}

/** What the Go-to offer shows: the words and acceptability `retune.ts` computed, and the press. */
export interface GotoOffer {
  why: string; enabled: boolean; press(): void;
  /** T-1004: the button's word, when the press is not a plain retune — a frozen pane's offer is
   * taken as "go live at this frequency" (unfreeze, then tune there), and a button still reading
   * "Retune" would name half of what it does. Absent = "Retune". */
  label?: string;
}

/** One row of the layers menu: a base style (radio) or an overlay (checkbox). Display only. */
export interface LayerRow {
  id: string; label: string; hint: string; on: boolean;
  /** A key of the marks this layer draws (T-807's coverage fog), painted by the one cell rule. */
  key?: readonly LegendEntry[];
}

/**
 * What the layers menu shows (T-806 / MAP-06, docs/24 §4): two independent axes for the ACTIVE
 * pane — exactly one base style, any number of overlays in paint order — plus the few switches that
 * are view-wide rather than per pane (the spectrum trace), stated as such. Built fresh from
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
  /** Split the active pane: `columns` (side by side, the default) or `rows` (stacked). T-1005. */
  split(dir?: "columns" | "rows"): void;
  closePane(): void;
  /** T-1005: the orientation of the split holding the active pane (null with one pane), and a flip
   * of it between rows and columns. Layout only: both panes keep their views. */
  splitDir?(): "columns" | "rows" | null;
  flipSplit?(): void;
  wholeSurface(): void;
  /** How many panes exist, so the menu can say the last one never closes. */
  paneCount(): number;
  paneMenuExtras?: HTMLElement[];
  /**
   * **Which front end the active pane's coverage comes from** (T-1006, docs/16 §8), as the rows of a
   * radio group: the union first, then one per attached radio. Optional — a host that knows of no
   * front ends offers no picker.
   *
   * Presentation state, like every other row in these menus: a pane's `device` decides whose grey it
   * draws and which radio its own retune names, and setting it reaches no route (the spy test drives
   * it and asserts the call list stays empty). The strings are the host's; nothing here learns what a
   * `device_id` is.
   */
  deviceMenu?(): PaneDeviceMenu;
  /** Pin the active pane to `id` (`"any"` or a `device_id`). A view change. */
  setPaneDevice?(id: string): void;
  /** Take the "one viewport per front end" offer: as many panes as radios, each pinned to one. Pane
   * arithmetic plus one `setPaneDevice` each — no radio moves. */
  splitPerDevice?(): void;
}

/** The device section of the viewport menu (T-1006). `pane` names the pane it acts on, exactly as
 * [[LayerMenu.pane]] does; `offer` is the "one viewport per front end" split, stated even when it is
 * refused (the [[RowAction]] rule: a control that vanishes teaches nothing). */
export interface PaneDeviceMenu {
  pane: string;
  rows: readonly LayerRow[];
  /** The sentence under the rows — whose coverage this pane draws, and what a retune here would
   * move. Computed by `surface/panedevice.ts`, never here. */
  note: string;
  offer: { label: string; why: string; enabled: boolean };
}

export interface MapControlHost extends LayerMenuHost, PaneMenuHost, SettingsHost {
  zoom(factor: number): void;
  /** Measurement mode (T-822): whether a plain drag measures instead of panning. */
  measuring(): boolean;
  setMeasuring(on: boolean): void;
  /**
   * **Retune mode** (T-1028, the user's 2026-09-25 amendment): while on, a settled pan/zoom commands
   * the radio through the one gated `DeviceAction` path. Off by default, and off is the old rule
   * exactly. `held` is the momentary form — `R` held down for one gesture — which lights the chip
   * without latching it, so the user can always see that their next gesture will tune.
   *
   * This is the ONE control in this cluster whose state changes what a gesture does to the device,
   * so it is the one that must never be ambiguous on screen: `aria-pressed` is the latch, and
   * `data-held` says the key is down. Optional, so a host without the mode shows no chip.
   */
  retuneMode?(): { on: boolean; held: boolean };
  setRetuneMode?(on: boolean): void;
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
  /**
   * T-1028 × T-996: one sentence about a retune that retune mode has pending, settling or in flight
   * for the active viewport (`null` when nothing is). T-1028 said it on the per-viewport row, which
   * T-996 retired from the app, so it is said here, on the capture block beside Retune — the one
   * place on the map where a gesture moving the radio is reported. Asked every frame, like the rest.
   */
  paneStatus?(): string | null;
  /** Tell the rest of the page the view moved (the surface's `mirror`). */
  viewChanged(): void;
  toast(text: string): void;
  /** T-821: the Research slide-in's toggle (open/close a panel — presentation only). */
  research?: { isOpen(): boolean; toggle(): void };
  /** T-1008: the scan plan's small button (in the Go-to cluster — it commands the radio, like the
   * retune offer beside it) and its panel. Built by `app/map/scan-overlay.ts`, whose own code states
   * its routes; absent, the cluster offers no scan. */
  scan?: { button: HTMLElement; panel: HTMLElement };
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

/** T-1028's banner, on its own because retune mode is not a tool mode: it re-binds no gesture — a
 * drag still pans and a wheel still zooms — it changes what the view coming to REST means. Its own
 * line so it can be shown beside a tool mode rather than instead of one. */
/** T-1053: the width at which the mode chips fold into ⋯ — `map-controls.css`'s phone breakpoint. */
export const FOLD_QUERY = "(max-width: 600px)";
/** A folded chip's name, shown only while it sits in the ⋯ menu (the chip row shows the icon). */
const foldWord = (w: string) => h("span", { class: "map-ibtn-word", "aria-hidden": "true" }, w);

const RETUNE_TEXT = "Retune mode: the radio follows this viewport — pan or zoom, and when the view settles it tunes there. Too wide to capture tunes the widest window centred on it. Press R (or the chip) to stop.";

/**
 * The pane arithmetic behind the zoom stack, over a real `PaneModel`. Split out of the mount so
 * the spy test drives the very code the page runs.
 *
 * Zoom anchors frequency at the pane's centre and time at the newest row when the pane follows
 * (so zooming never walks a live pane off the growing edge) and at the middle when it is frozen.
 *
 * T-1001: follow/freeze left this module with the FAB — it is per pane now, in
 * `centre/pane-live.ts`'s `paneLiveActions`, which drives the same `PaneModel` methods. What is
 * left here acts on the active pane and is view arithmetic only: nothing reaches a route.
 */
export function paneActions(panes: PaneControl, activePane: () => string | null) {
  return {
    zoom(factor: number): void {
      const id = activePane();
      if (!id) return;
      const anchorT = panes.isFollowing(id) ? 1 : 0.5;
      // T-995: with the minimap retired (user, 2026-09-25) the whole 1 MHz–6 GHz range is reached by
      // zooming OUT, Google-Maps style — so once one axis cannot take the whole press (the T-472
      // lock comes back short of it: in practice the time axis already holds the whole record, which
      // only grows by the seconds between presses), Zoom-out widens each axis on its own, each to its
      // own bound, so frequency carries on to the device range. Without this a young record capped
      // the zoom-out at tens of MHz, and a phone (no shift-wheel) had no way at all to see the whole
      // spectrum. A discrete press, not the continuous wheel T-472 locked; the wheel, pinch and
      // Zoom-in keep the lock unchanged, and while neither axis is at a bound this IS the lock.
      if (factor > 1 && panes.lockedZoomFactor(id, factor) < factor) {
        panes.zoomFreq(id, factor, 0.5);
        panes.zoomTime(id, factor, anchorT);
        return;
      }
      panes.zoomBoth(id, factor, 0.5, anchorT);
    },
    isFollowing(): boolean {
      const id = activePane();
      return !!id && panes.isFollowing(id);
    },
  };
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
 * Build the cluster. Returns the element (the host appends it over the canvas) and the hooks the
 * host calls: `viewMoved()` when a gesture moved the view (a Go-to offer describes a window the pane
 * has now left, so it is withdrawn), and `tuningChanged()` when the radio's window changed.
 */
export function mountMapControls(host: MapControlHost): {
  el: HTMLElement; viewMoved(): void; syncLayers(): void; syncMeasure(): void; syncResearch(): void;
  /** T-996: re-read the active viewport's Retune / width offers. Called per RENDER FRAME. */
  syncRetune(): void;
  tuningChanged(): void;
  /** T-1028: re-state retune mode's chip and banner (the `R` key, or a retune becoming pending). */
  syncRetuneMode(): void;
  /** T-1000: re-state which pane the per-pane chrome acts on (after an active-pane change, split or close). */
  syncActive(): void;
} {
  const input = h("input", {
    class: "mono", placeholder: "Go to frequency, e.g. 433.92M or 101.3", "aria-label": "Go to frequency",
    inputmode: "decimal", autocomplete: "off", spellcheck: "false",
  }) as HTMLInputElement;
  // T-1000: which pane a Go-to moves, stated beside the entry while there is more than one.
  const gotoPane = h("span", { class: "map-goto-pane", hidden: true });
  const goto = h("form", { class: "map-glass map-goto map-fade", role: "search", autocomplete: "off" },
    svg(["circle", 11, 11, 7], ["path", "M20 20l-3.5-3.5"]), input, gotoPane,
    h("span", { class: "map-hint", "aria-hidden": "true" }, "↵"),
    // T-1008: the Scan button sits in the Go-to glass — small, because it commands the radio.
    host.scan?.button ?? null);

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
  // T-1028: what retune mode is doing to this viewport now. `role="status"` so a screen reader hears
  // a retune the user's own pan asked for; hidden (not emptied) when there is nothing to say.
  const retuneStatus = h("span", { class: "map-retune-status", role: "status", hidden: true });
  const retuneRow = h("div", { class: "map-retune-row" }, retuneGo, retuneWhy);
  const retune = h("div", { class: "map-glass map-retune", role: "group", "aria-label": "Capture this viewport", hidden: true },
    retuneRow, widthRow, retuneStatus);

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
  }, svg(["path", "M3 17l14-14 4 4L7 21H3v-4z"], ["path", "M13 7l2 2M10 10l2 2M7 13l2 2"]), foldWord("Measure")) as HTMLButtonElement;
  // T-820 (MAP-20): Annotate (bare drag = a box, bare click = a text note) and Pin (bare click = a
  // marker; drag still pans), the other two columns of docs/23 §10.4's table, beside Measure. Each
  // asks for a label; the surface mount saves it (an authoring act, never a device route).
  const annotateBtn = h("button", {
    type: "button", class: "map-ibtn map-annotate-btn", "aria-label": "Annotate", "aria-pressed": "false",
    hidden: !host.setAnnotating,
    title: "Annotate: drag on the surface to draw an annotation box, or click to drop a text note; "
      + "you are asked for its label, and it is saved as an annotation (docs/25 §5). Esc exits.",
  }, svg(["path", "M4 4h16v12H8l-4 4z"], ["path", "M8 8h8M8 12h5"]), foldWord("Annotate")) as HTMLButtonElement;
  const pinBtn = h("button", {
    type: "button", class: "map-ibtn map-pin-btn", "aria-label": "Pin", "aria-pressed": "false",
    hidden: !host.setAnnotating,
    title: "Pin: click on the surface to drop a labelled marker there, saved as an annotation. Dragging still pans. Esc exits.",
  }, svg(["path", "M6 21V4"], ["path", "M6 4h12l-3 4 3 4H6"]), foldWord("Pin")) as HTMLButtonElement;
  // T-1028: retune mode's chip, beside the tool-mode buttons — the same shape of control (a mode
  // that changes what a gesture means) and the only one that can reach the front end. The title
  // says both forms, because a mode whose momentary key is undiscoverable is a mode nobody holds.
  const retuneBtn = h("button", {
    type: "button", class: "map-ibtn map-retune-btn", "aria-label": "Retune mode", "aria-pressed": "false",
    hidden: !host.retuneMode,
    title: "Retune mode: while on, panning or zooming tunes the radio to the viewport when the gesture settles "
      + "(a view wider than one capture window tunes the widest window centred on it). Off by default — "
      + "then a pan never commands the radio. Tap R to latch it, or hold R for one gesture.",
  }, svg(["circle", 12, 12, 3], ["path", "M12 2v3M12 19v3M2 12h3M19 12h3"], ["path", "M5.6 5.6l2.1 2.1M16.3 16.3l2.1 2.1M18.4 5.6l-2.1 2.1M7.7 16.3l-2.1 2.1"]), foldWord("Retune mode")) as HTMLButtonElement;
  const paneBtn = h("button", {
    type: "button", class: "map-ibtn map-pane-btn", "aria-label": "Viewport", title: "Viewport: split, close, whole surface, front end, record",
    "aria-pressed": "false", "aria-expanded": "false", "aria-controls": "map-pane-menu",
  }, svg(["path", "M4 5h16v14H4z"], ["path", "M12 5v14"])) as HTMLButtonElement;
  // T-993: the retired top bar's Review button is moved in here (`top-chrome.ts`), beside the other
  // panels' buttons, and Theme into a small ⋯ menu — settings, not a bar button.
  const reviewHome = h("span", { class: "map-review-home" });
  const moreBtn = h("button", {
    type: "button", class: "map-ibtn map-more-btn", "aria-label": "More: settings", title: "More: theme and settings",
    "aria-pressed": "false", "aria-expanded": "false", "aria-controls": "map-more-menu",
  }, svg(["circle", 5, 12, 1.2], ["circle", 12, 12, 1.2], ["circle", 19, 12, 1.2])) as HTMLButtonElement;
  const topright = h("div", { class: "map-chips map-topright map-fade" }, layersBtn, researchBtn, measureBtn, annotateBtn, pinBtn, retuneBtn, paneBtn, reviewHome, moreBtn);
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
  // T-1053: on a phone the four MODE chips (Measure, Annotate, Pin, Retune mode) fold into the ⋯
  // menu, so the top-right row stays five small chips — Layers, Research, Viewport, Review, ⋯ —
  // instead of nine reaching across the pane (T-1025: chips, not a bar) and squeezing Go-to to a
  // sliver. The SAME elements move (`foldForWidth`, below): their handlers, pressed state and the
  // retune chip's held marker go with them, and a mode that is on is still said by its banner.
  const moreTools = h("div", { class: "map-more-tools", role: "group", "aria-label": "Tools", hidden: true });
  const moreMenu = h("div", { class: "map-glass map-pane-menu map-more-menu", id: "map-more-menu", role: "group", "aria-label": "Settings", hidden: true },
    h("div", { class: "map-layers-head" }, h("span", {}, "Settings"), moreClose), moreTools, moreBody, settingsList);
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
  // T-1005: rows ⇄ columns for the split the active pane is in. Hidden without a host that can.
  const flipItem = paneItem("flip", "Stack as rows", "Re-orient the active viewport's split between side by side and stacked. Both viewports keep what they show.", () => host.flipSplit?.());
  flipItem.hidden = !host.flipSplit;
  const paneHead = h("span", {}, "Viewport");
  // T-1006: the front-end picker for THIS viewport, and the "one viewport per front end" split.
  // Rebuilt from `host.deviceMenu()` each time the menu opens (`syncPaneMenu`), hidden entirely when
  // the host offers none — a picker with nothing to pick is chrome that says nothing. Pressing a row
  // is a view change: it chooses whose coverage decides this pane's grey and which radio its own
  // retune will name, and reaches no route.
  const deviceRows = h("div", { class: "map-pane-devices" });
  const deviceNote = h("div", { class: "map-note map-pane-device-note" });
  const perDevice = h("button", { type: "button", class: "map-pane-item", "data-pane-act": "per-device" }) as HTMLButtonElement;
  perDevice.addEventListener("click", () => { if (!perDevice.disabled) { host.splitPerDevice?.(); setPaneOpen(false); } });
  const deviceHead = h("h4", {}, "Front end");
  const deviceSection = h("div", { class: "map-layers-axis map-pane-device", "data-axis": "device", role: "radiogroup", "aria-label": "Front end", hidden: true },
    deviceHead, deviceRows, deviceNote, perDevice);
  const paneMenu = h("div", { class: "map-glass map-pane-menu", id: "map-pane-menu", role: "group", "aria-label": "Viewport", hidden: true },
    h("div", { class: "map-layers-head" }, paneHead, paneClose),
    paneItem("split", "Split ⇔", "Two viewports onto the same surface, side by side. They show the identical box until one is moved. The new one starts with this viewport's layers and diverges as you toggle.", () => host.split("columns")),
    paneItem("split-rows", "Split ⇕", "Two viewports onto the same surface, stacked one above the other. They show the identical box until one is moved.", () => host.split("rows")),
    flipItem,
    closeItem,
    paneItem("whole", "Whole surface", "Zoom the active viewport out to the device-available spectrum over the whole record horizon (never less than the retained capture window).", () => host.wholeSurface()),
    deviceSection,
    ...(host.paneMenuExtras ?? []),
    h("div", { class: "map-note" }, "View only: splitting, closing, zooming out and choosing a front end never command the radio."));
  // The mockup's `#mode` banner: while a tool mode is on (Measure, or T-820's Annotate / Pin), say
  // what a drag will do and how to leave. Never faded.
  const modeBanner = h("div", { class: "map-glass map-mode", role: "status", hidden: true }, MODE_TEXT.measure);
  // T-1028: retune mode's own banner, beside (never instead of) a tool mode's — see `RETUNE_TEXT`.
  // Declared with the rest of the chrome because `el` below composes it; `syncRetuneMode` fills it.
  const retuneBanner = h("div", { class: "map-glass map-mode map-retune-mode", role: "status", hidden: true }, RETUNE_TEXT);
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

  // T-1001: the follow-live FAB is retired. Live/Freeze is a button INSIDE each pane's rectangle
  // (`centre/pane-live.ts`), because one corner button acting on the hidden active pane could not
  // say which of two panes it froze. Nothing here follows, freezes or states the live edge.
  const layersPane = h("span", { class: "map-pane-badge", "aria-hidden": "true", hidden: true });
  layersBtn.append(layersPane);

  // T-996: the left column under the nudges is a STACK, not a set of fixed `top:` constants — the
  // T-997 inventory pills, the persistent capture block, the transient Go-to offer and (below
  // 1180 px) the tool-mode banner are each as tall as their words make them, so a fixed top per
  // block is exactly how the offer came to be drawn over the width presets at 400 px. In flow, each
  // starts where the last ended; the pills lead it, so their one fixed place under Go-to holds.
  // T-1028's retune-mode banner joins it under the tool-mode banner (beside it, never instead).
  const stack = h("div", { class: "map-stack" }, invHome, retune, offer, modeBanner, retuneBanner);
  const el = h("div", { class: "map-ctl", "data-band": "chrome" }, goto, nudgeHome, stack, statusHome, topright, layers, paneMenu, moreMenu, host.scan?.panel ?? null, zoom);

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
    for (const b of [zoomPane, layersPane]) { b.hidden = !name; b.textContent = n; }
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
    paneHead.textContent = name ? `Viewport · ${name.label}` : "Viewport";
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
    offerGo.textContent = shown.label ?? "Retune";
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
    const dir = host.splitDir?.() ?? null;
    flipItem.disabled = dir === null;
    flipItem.textContent = dir === "rows" ? "Side by side ⇔" : "Stack as rows ⇕";
    flipItem.dataset.dir = dir ?? "";
    // T-1006: the front-end picker. Built on open rather than per frame — a menu the user is reading
    // must not have its radio inputs replaced under the pointer — and only when the host offers one.
    const dm = host.deviceMenu?.();
    deviceSection.hidden = !dm;
    if (!dm) return;
    deviceSection.setAttribute("aria-label", `Front end, ${dm.pane}`);
    deviceHead.textContent = `Front end · ${dm.pane} · one at a time`;
    deviceRows.replaceChildren(...dm.rows.map((l) => {
      const input = h("input", {
        type: "radio", name: "map-pane-device", value: l.id, "data-pane-device": l.id,
      }) as HTMLInputElement;
      input.checked = l.on;
      input.addEventListener("change", () => { host.setPaneDevice?.(l.id); syncPaneMenu(); });
      return h("label", { class: "map-row" }, input, l.label, h("small", {}, l.hint));
    }));
    deviceNote.textContent = dm.note;
    perDevice.textContent = dm.offer.label;
    perDevice.title = dm.offer.why;
    perDevice.disabled = !dm.offer.enabled;
    perDevice.setAttribute("aria-disabled", String(!dm.offer.enabled));
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
  // T-1053: a folded mode chip pressed from the ⋯ menu turns its mode on (its own handler) and
  // closes the menu, so the drag that mode is for lands on the map rather than on an open menu.
  moreTools.addEventListener("click", (e) => {
    if ((e.target as Element | null)?.closest?.("button")) setMoreOpen(false);
  });
  // T-1053: the fold follows the width, live — a rotated phone or a resized window re-homes them.
  const folded = [measureBtn, annotateBtn, pinBtn, retuneBtn];
  const narrow = typeof matchMedia === "function" ? matchMedia(FOLD_QUERY) : null;
  const foldForWidth = () => {
    const fold = !!narrow?.matches;
    if (fold && folded[0].parentElement !== moreTools) moreTools.append(...folded);
    else if (!fold && folded[0].parentElement !== topright) paneBtn.before(...folded);
    moreTools.hidden = !fold;
  };
  narrow?.addEventListener?.("change", foldForWidth);
  foldForWidth();
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

  // T-1028: the chip's state, and the banner while the mode is on. Called on every change of the
  // mode AND while a retune is pending, because `held` flips with the key rather than with a press.
  const syncRetuneMode = () => {
    const st = host.retuneMode?.() ?? null;
    if (!st) return;
    retuneBtn.setAttribute("aria-pressed", String(st.on));
    retuneBtn.classList.toggle("is-on", st.on);
    retuneBtn.dataset.held = st.held ? "true" : "false";
    retuneBanner.hidden = !st.on;
  };
  retuneBtn.addEventListener("click", () => {
    const st = host.retuneMode?.();
    host.setRetuneMode?.(!(st?.on ?? false));
    syncRetuneMode();
  });

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
    const said = host.paneStatus?.() ?? null;
    const show = !!a || said !== null;
    if (retune.hidden === show) retune.hidden = !show;
    if (retuneRow.hidden === !!a) retuneRow.hidden = !a;
    if (a) { apply(retuneGo, a); setText(retuneWhy, a.why); }
    if (retuneStatus.hidden !== (said === null)) retuneStatus.hidden = said === null;
    if (said !== null) setText(retuneStatus, said);
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

  syncActive();
  syncMeasure();
  syncRetune();
  syncRetuneMode();
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
   * here; the press still re-derives at commit and refuses if the view moved. (T-1001: the panes'
   * own Live buttons depend on the tuned window too; the surface re-states them beside this call.)
   */
  const tuningChanged = () => {
    if (!shown) return;
    shown = host.gotoOffer();
    if (!shown) hideOffer();
    // T-1004: the BUTTON's word is re-derived too, not only the sentence — a pane that froze or
    // went live since the offer was painted must not keep "Go live here" / "Retune" from before.
    else { offerWhy.textContent = shown.why; offerGo.textContent = shown.label ?? "Retune"; offerGo.disabled = !shown.enabled; }
  };
  // A gesture moved the view: the Go-to offer describes a window the pane has left.
  const viewMoved = () => { hideOffer(); };
  return { el, viewMoved, syncLayers, syncMeasure, syncResearch, syncRetune, syncActive, tuningChanged, syncRetuneMode };
}
