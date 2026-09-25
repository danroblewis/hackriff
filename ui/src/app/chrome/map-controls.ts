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
  /** Move the pane's frequency window (view arithmetic; never a device route). Used by
   * `followLive` (T-955) to bring a pane's FREQUENCY back to the tuned window too, not only its
   * time, when the caller supplies one. */
  setFreq(id: string, centerHz: number, spanHz: number): void;
}

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
  /** The colour scale (T-470/T-528, rehomed from the toolbar by T-882): exactly one of the surface's
   * range modes, view-wide, plus the sentence stating the range it produces. Absent = not offered. */
  scale?: { rows: LayerRow[]; note: string };
}

/** The menu's writes. Every one is presentation state; none can reach a route. */
export interface LayerMenuHost {
  layerMenu(): LayerMenu;
  setBase(id: string): void;
  /** Toggle one of the active pane's layers — an overlay, or a `data` row (the coverage fog). */
  toggleOverlay(id: string): void;
  toggleViewWide(id: string): void;
  /** Choose the colour-scale mode (a display range, never a gain). Optional: a host with no scale. */
  setScale?(id: string): void;
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

export interface MapControlHost extends LayerMenuHost, PaneMenuHost {
  zoom(factor: number): void;
  followLive(): void;
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
  /** Tell the rest of the page the view moved (the surface's `mirror`). */
  viewChanged(): void;
  toast(text: string): void;
  /** T-821: the Research slide-in's toggle (open/close a panel — presentation only). */
  research?: { isOpen(): boolean; toggle(): void };
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
 * navigation poll the host already runs), asked only here and only on an explicit follow-live press — never at open
 * (see `surface/bootstrap.ts`'s recency-narrowed opening) and never as a background correction. A
 * pane can drift to spectrum the radio is no longer tuned to (a stale reload, a pan, a retune
 * elsewhere) while still reading as "following" in TIME; pressing follow-live is the explicit ask to
 * return to what the front end is doing now, in both axes, so it also resets the pane's frequency
 * window when the caller has one to give. View arithmetic only — `setFreq` reaches no route.
 */
export function paneActions(
  panes: PaneControl,
  activePane: () => string | null,
  onFollow?: (on: boolean) => void,
  tunedWindow?: () => { centerHz: number; spanHz: number } | null,
) {
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
      panes.setFollowing(id, true);
      const w = tunedWindow?.();
      if (w && Number.isFinite(w.centerHz) && Number.isFinite(w.spanHz) && w.spanHz > 0) {
        panes.setFreq(id, w.centerHz, w.spanHz);
      }
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
  };
}

/** The FAB's words for the active pane's state — what the VIEW is doing, never the radio. */
export function fabState(following: boolean): { cls: "following" | "frozen"; title: string } {
  return following
    ? { cls: "following", title: "Following the live edge — press to freeze this viewport on what it shows. Capture never stops, paused or not." }
    : { cls: "frozen", title: "This viewport is frozen on a past window (capture continues) — press to follow the live edge again." };
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
  el: HTMLElement; viewMoved(): void; syncFollow(): void; syncLayers(): void; syncMeasure(): void; syncResearch(): void;
} {
  const input = h("input", {
    class: "mono", placeholder: "Go to frequency, e.g. 433.92M or 101.3", "aria-label": "Go to frequency",
    inputmode: "decimal", autocomplete: "off", spellcheck: "false",
  }) as HTMLInputElement;
  const goto = h("form", { class: "map-glass map-goto map-fade", role: "search", autocomplete: "off" },
    svg(["circle", 11, 11, 7], ["path", "M20 20l-3.5-3.5"]), input,
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
  const topright = h("div", { class: "map-glass map-topright map-fade" }, layersBtn, researchBtn, measureBtn, annotateBtn, pinBtn, paneBtn);
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
  const paneMenu = h("div", { class: "map-glass map-pane-menu", id: "map-pane-menu", role: "group", "aria-label": "Viewport", hidden: true },
    h("div", { class: "map-layers-head" }, h("span", {}, "Viewport"), paneClose),
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
  const layers = h("div", { class: "map-glass map-layers", id: "map-layers", role: "group", "aria-label": "Layers", hidden: true },
    h("div", { class: "map-layers-head" }, h("span", {}, "Layers"), layersClose),
    layersList,
    h("div", { class: "map-note" }, "Display only: a layer changes what is drawn, never what is measured or detected."));

  const zoomIn = h("button", { type: "button", class: "map-ibtn map-zoom-in", "aria-label": "Zoom in", title: "Zoom in (both axes)" },
    svg(["path", "M12 5v14M5 12h14"]));
  const zoomOut = h("button", { type: "button", class: "map-ibtn map-zoom-out", "aria-label": "Zoom out", title: "Zoom out (both axes)" },
    svg(["path", "M5 12h14"]));
  const zoom = h("div", { class: "map-glass map-zoom map-fade", role: "group", "aria-label": "Zoom" },
    zoomIn, h("div", { class: "map-sep", "aria-hidden": "true" }), zoomOut);

  const fab = h("button", { type: "button", class: "map-fab map-fade", "aria-label": "Follow live" },
    svg(["circle", 12, 12, 3], ["path", "M12 2v4M12 18v4M2 12h4M18 12h4"], ["circle", 12, 12, 8])) as HTMLButtonElement;

  const el = h("div", { class: "map-ctl", "data-band": "chrome" }, goto, offer, modeBanner, topright, layers, paneMenu, zoom, fab);

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
    const st = fabState(host.isFollowing());
    fab.classList.toggle("following", st.cls === "following");
    fab.classList.toggle("frozen", st.cls === "frozen");
    fab.setAttribute("aria-pressed", String(st.cls === "following"));
    fab.title = st.title;
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
        : act.dataset.viewLayer ? `[data-view-layer="${act.dataset.viewLayer}"]`
          : act.dataset.scale ? `[data-scale="${act.dataset.scale}"]` : null)
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
      ...(m.scale && host.setScale ? [h("div", { class: "map-layers-axis", "data-axis": "scale", role: "radiogroup", "aria-label": "Colour scale, every pane" },
        h("h4", {}, "Colour scale · every pane · one at a time"),
        ...m.scale.rows.map((l) => row(l,
          h("input", { type: "radio", name: "map-scale", value: l.id, "data-scale": l.id }) as HTMLInputElement,
          () => host.setScale!(l.id))),
        h("div", { class: "map-layers-note sf-range-note" }, m.scale.note))] : []),
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
    if (open) { syncPaneMenu(); if (layersOpen) setLayersOpen(false); }
    fade.hold("pane-menu", open); // an open menu never fades
  };
  const setLayersOpen = (open: boolean) => {
    layersOpen = open;
    layersOverlay.open(open);
    if (open && paneOpen) setPaneOpen(false);
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
  fab.addEventListener("click", () => {
    if (host.isFollowing()) host.pauseLive(); else host.followLive();
    host.viewChanged();
    syncFollow();
  });

  const syncResearch = () => researchBtn.setAttribute("aria-pressed", String(!!host.research?.isOpen()));
  researchBtn.addEventListener("click", () => { host.research?.toggle(); syncResearch(); });
  syncResearch();

  syncFollow();
  syncMeasure();
  /** Re-render an open menu — the active pane changed, or a toggle elsewhere changed a layer. */
  const syncLayers = () => { if (layersOpen) renderLayers(); if (paneOpen) syncPaneMenu(); };
  return { el, viewMoved: hideOffer, syncFollow, syncLayers, syncMeasure, syncResearch };
}
