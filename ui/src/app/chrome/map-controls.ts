// T-802 (MAP-02): the floating control cluster — Go-to, the layers button, the follow-live
// ("my location") FAB and the zoom stack — docked to the canvas's edges in SCREEN space (docs/23
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
import { h } from "../dom";
import { trackOverlay } from "./dismiss";

/** One zoom-button press scales both axes' spans by this (in) or its inverse (out) — the mockup's
 * step. The pane's own `zoomBoth` holds the aspect lock and the bounds (T-472), so a press at a
 * bound does nothing rather than distorting the view. */
export const ZOOM_STEP = 0.6;
/** Chrome fades after this long with no pointer, key, wheel or focus event (docs/23 §10.2). */
export const IDLE_MS = 6000;

/** The subset of `surface/panes.ts`'s `PaneModel` the cluster drives. All of it is view state. */
export interface PaneControl {
  zoomBoth(id: string, factor: number, anchorF?: number, anchorT?: number): void;
  isFollowing(id: string): boolean;
  setFollowing(id: string, on: boolean): void;
}

/** What the Go-to offer shows: the words and acceptability `retune.ts` computed, and the press. */
export interface GotoOffer { why: string; enabled: boolean; press(): void }

/** One row of the layers menu: a base style (radio) or an overlay (checkbox). Display only. */
export interface LayerRow { id: string; label: string; hint: string; on: boolean }

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
  overlays: LayerRow[];
  viewWide: LayerRow[];
}

/** The menu's writes. Every one is presentation state; none can reach a route. */
export interface LayerMenuHost {
  layerMenu(): LayerMenu;
  setBase(id: string): void;
  toggleOverlay(id: string): void;
  toggleViewWide(id: string): void;
}

export interface MapControlHost extends LayerMenuHost {
  zoom(factor: number): void;
  followLive(): void;
  isFollowing(): boolean;
  /** Centre the active pane on `hz`, keeping its spans. A view move; never a retune. */
  goTo(hz: number): void;
  /** The tuned centre, for `+`/`-` relative entries (`parseFrequency`'s contract), or null. */
  centreHz(): number | null;
  /** The retune offer for the active pane when no tuned window covers it, else null. */
  gotoOffer(): GotoOffer | null;
  /** Tell the rest of the page the view moved (the surface's `mirror`). */
  viewChanged(): void;
  toast(text: string): void;
}

/**
 * The pane arithmetic behind the zoom stack and the FAB, over a real `PaneModel`. Split out of the
 * mount so the spy test drives the very code the page runs.
 *
 * Zoom anchors frequency at the pane's centre and time at the newest row when the pane follows
 * (so zooming never walks a live pane off the growing edge) and at the middle when it is frozen.
 */
export function paneActions(panes: PaneControl, activePane: () => string | null, onFollow?: (on: boolean) => void) {
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
      onFollow?.(true);
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
    ? { cls: "following", title: "Following the live edge. Capture never stops, paused or not." }
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

/**
 * Build the cluster. Returns the element (the host appends it over the canvas) and two hooks the
 * host calls: `viewMoved()` when a gesture moved the view (a Go-to offer describes a window the pane
 * has now left, so it is withdrawn), and `syncFollow()` when the follow state may have changed.
 */
export function mountMapControls(host: MapControlHost): { el: HTMLElement; viewMoved(): void; syncFollow(): void; syncLayers(): void } {
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
  const topright = h("div", { class: "map-glass map-topright map-fade" }, layersBtn);
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

  const el = h("div", { class: "map-ctl", "data-band": "chrome" }, goto, offer, topright, layers, zoom, fab);

  const fade = new IdleFade((idle) => el.classList.toggle("is-idle", idle));
  const poke = () => fade.poke();
  for (const ev of ["pointermove", "pointerdown", "keydown", "wheel", "touchstart"]) {
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
        : act.dataset.viewLayer ? `[data-view-layer="${act.dataset.viewLayer}"]` : null)
      : null;
    const row = (l: LayerRow, input: HTMLInputElement, press: () => void) => {
      input.checked = l.on;
      input.addEventListener("change", () => { press(); renderLayers(); });
      return h("label", { class: "map-row" }, input, l.label, h("small", {}, l.hint));
    };
    layersList.replaceChildren(
      h("div", { class: "map-layers-axis", "data-axis": "base", role: "radiogroup", "aria-label": `Base style, ${m.pane}` },
        h("h4", {}, `Base style · ${m.pane} · one at a time`),
        ...m.bases.map((l) => row(l,
          h("input", { type: "radio", name: "map-base", value: l.id, "data-base": l.id }) as HTMLInputElement,
          () => host.setBase(l.id)))),
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
  const setLayersOpen = (open: boolean) => {
    layersOpen = open;
    layersOverlay.open(open);
    layers.hidden = !open;
    layersBtn.setAttribute("aria-pressed", String(open));
    layersBtn.setAttribute("aria-expanded", String(open));
    if (open) renderLayers();
    fade.hold("layers", open); // an open menu never fades
  };
  layersBtn.addEventListener("click", () => setLayersOpen(!layersOpen));
  layersClose.addEventListener("click", () => { setLayersOpen(false); layersBtn.focus(); });

  const zoomBy = (k: number) => { host.zoom(k); hideOffer(); host.viewChanged(); };
  zoomIn.addEventListener("click", () => zoomBy(ZOOM_STEP));
  zoomOut.addEventListener("click", () => zoomBy(1 / ZOOM_STEP));
  fab.addEventListener("click", () => { host.followLive(); host.viewChanged(); syncFollow(); });

  syncFollow();
  /** Re-render an open menu — the active pane changed, or a toolbar button toggled a layer. */
  const syncLayers = () => { if (layersOpen) renderLayers(); };
  return { el, viewMoved: hideOffer, syncFollow, syncLayers };
}
