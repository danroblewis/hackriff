// The preview page's entry point (T-450): DOM assembly, pointer plumbing, and the frame loop.
//
// Thin by construction, in the sense ADR-0013 §1 means: it converts pointer events into drawing
// -buffer coordinates and calls `SurfacePreview`, which calls `PaneModel`. There is no signal logic
// here, no level policy, and no device route — `./retune.ts` and `../app/centre/view.ts` are not
// imported, and `ui/test/surface-preview.test.ts` asserts that against this source.
//
// It is also **separate from the app**: its own HTML, its own bundle, its own page. It shares the
// tab's API token (`sessionStorage`, the one key the app already uses) and nothing else. Nothing
// under `ui/src/app/` reaches this file.

import { ControlClient } from "../controls/client";
import { h } from "../app/dom";
import { takeToken } from "../app/net";
import { fmtShare } from "./bootstrap";
import { legendEntries, swatchPixels } from "./legend";
import { SurfacePreview, isBackpressure, probeSurface, wheelZoom } from "./preview";

const SWATCH_W = 54, SWATCH_H = 22;

function slot(name: string): HTMLElement {
  const el = document.querySelector<HTMLElement>(`[data-slot="${name}"]`);
  if (!el) throw new Error(`missing data-slot="${name}"`);
  return el;
}

const text = (el: HTMLElement, s: string) => { if (el.textContent !== s) el.textContent = s; };

/** A capture instant, as a local wall clock — the axis is absolute capture time throughout. */
const at = (ns: number) => new Date(ns / 1e6).toLocaleString();

function mountLegend(root: HTMLElement): void {
  for (const e of legendEntries()) {
    const canvas = h("canvas", { class: "sp-swatch", width: SWATCH_W, height: SWATCH_H });
    const ctx = canvas.getContext("2d");
    if (ctx) {
      const img = ctx.createImageData(SWATCH_W, SWATCH_H);
      img.data.set(swatchPixels(e, SWATCH_W, SWATCH_H));
      ctx.putImageData(img, 0, 0);
    }
    root.append(h("div", { class: "sp-legend-row", "data-mark": e.key },
      canvas,
      h("div", { class: "sp-legend-text" },
        h("b", {}, e.label),
        h("span", {}, e.note)),
    ));
  }
}

function fail(message: string, detail = ""): void {
  const stage = slot("stage");
  stage.replaceChildren(h("div", { class: "sp-fail" }, h("b", {}, message), detail ? h("p", {}, detail) : ""));
}

async function main(): Promise<void> {
  const token = takeToken();
  if (!token) {
    fail("No API token in this tab.",
      "Open the link `hk serve` printed (it carries `#token=…`), or open the app at / first — this page shares that token and adds `#token=…` to this address.");
    return;
  }
  const client = new ControlClient(token);

  let probe;
  try {
    probe = await probeSurface((path) => client.get(path));
  } catch (e) {
    // Backpressure is not a broken surface, and after the retries it is the only thing left to say:
    // something else — another tab, or this page's own abandoned reads from before a reload — is
    // holding the route's four tile slots. Reloading is the whole remedy.
    if (isBackpressure(e)) {
      fail("The tile route is busy producing for someone else.",
        "GET /api/tiles refused every attempt: tile production takes the history lock, and its slots are shared with every other viewport and tab. Nothing is wrong with the surface — reload in a moment.");
    } else {
      fail("The surface could not be addressed.",
        `${e instanceof Error ? e.message : String(e)} — GET /api/tiles is what states the view lattice, and a client that guessed one would be addressing a pyramid that does not exist.`);
    }
    return;
  }

  // ——— what the user reads before they read the picture ———
  text(slot("note"), probe.note);
  text(slot("edge"), `newest recorded ${at(probe.origin.edgeNs)}`);
  const census = probe.census;
  text(slot("census"), census.total
    ? `coverage map: ${census.observed} observed · ${census.unobserved} unobserved · ${census.unknown} past the horizon (${fmtShare(census.observed, census.total)} sampled)`
    : "coverage map: none returned");
  slot("provenance").replaceChildren(
    h("li", {}, `frequency extent — ${probe.origin.provenance.freq}`),
    h("li", {}, `time extent — ${probe.origin.provenance.time}`),
    h("li", {}, `requests — ${probe.requests.join("  ·  ")}`),
    ...probe.degraded.map((d) => h("li", { class: "sp-degraded" }, d)),
  );
  mountLegend(slot("legend"));

  const canvas = slot("canvas") as HTMLCanvasElement;
  let preview: SurfacePreview;
  try {
    preview = new SurfacePreview({ canvas, probe, token, fetchFn: (u, i) => fetch(u, i), chrome: slot("chrome") });
  } catch (e) {
    fail("WebGL2 is unavailable in this browser.", e instanceof Error ? e.message : String(e));
    return;
  }

  // ——— controls. Every one of these is a view change; none reaches the front end. ———
  const button = (label: string, title: string, fn: () => void) =>
    h("button", { type: "button", class: "sp-btn", title, onclick: fn }, label);
  slot("actions").replaceChildren(
    button("Fit to coverage", "Put the active pane back on the region the backend reported as observed.", () => preview.fitToCoverage()),
    button("Whole surface", "Zoom the active pane out to the device-available spectrum over the whole record horizon.", () => preview.fitToSurface()),
    button("Split ⇔", "Two viewports onto the same surface, side by side. They show the identical box until one is moved.", () => preview.split("columns")),
    button("Split ⇕", "Two viewports onto the same surface, stacked.", () => preview.split("rows")),
    button("Close pane", "Close the active viewport. The last one never closes.", () => preview.closeActive()),
    button("Overlays", "Draw the map's pane rectangles. They are strokes, never washes, so they cannot tint a measurement.", () => { preview.view.overlays = !preview.view.overlays; }),
  );

  // ——— pointer: drawing-buffer coordinates, GL convention (origin bottom-left) ———
  const point = (e: { clientX: number; clientY: number }) => {
    const r = canvas.getBoundingClientRect();
    return {
      x: (e.clientX - r.left) * (canvas.width / Math.max(1, r.width)),
      y: canvas.height - (e.clientY - r.top) * (canvas.height / Math.max(1, r.height)),
    };
  };

  // **Drag pans, on both axes at once (T-456)** — and it has no threshold and no combined travel
  // measure, which is T-407's lesson kept rather than re-learned: that defect was a 6 px threshold
  // that turned a tap into a drag, plus travel measured as `clientX + clientY` so a stroke *across*
  // a bar counted as travel *along* it. Here each move applies its own `dx` and `dy` to its own
  // axis, a zero-pixel move moves the view by zero, and nothing accumulates toward a decision.
  let dragging: { x: number; y: number; map: boolean; pane: string | null } | null = null;
  canvas.addEventListener("pointerdown", (e) => {
    const p = point(e);
    const map = preview.onMap(p);
    const pane = map ? null : preview.paneAt(p);
    if (pane) preview.activePane = pane;
    dragging = { x: e.clientX, y: e.clientY, map, pane };
    canvas.setPointerCapture(e.pointerId);
  });
  canvas.addEventListener("pointermove", (e) => {
    if (!dragging || !e.buttons) return;
    const scale = canvas.width / Math.max(1, canvas.getBoundingClientRect().width);
    // Screen y runs down and the drawing buffer's runs up, so the vertical delta is negated once,
    // here, and every consumer below is in one convention.
    const dx = (e.clientX - dragging.x) * scale, dy = -(e.clientY - dragging.y) * scale;
    dragging.x = e.clientX;
    dragging.y = e.clientY;
    if (dragging.map) preview.dragMap(dx, dy);
    else if (dragging.pane) preview.drag(dragging.pane, dx, dy);
  });
  const endDrag = () => { dragging = null; };
  canvas.addEventListener("pointerup", endDrag);
  canvas.addEventListener("pointercancel", endDrag);

  // **Every wheel over the canvas is the surface's, whatever is held down (T-456).**
  //
  // The listener is `{ passive: false }` precisely so this `preventDefault` binds, and it is
  // unconditional: ctrl+wheel is the browser's page-zoom shortcut and cmd+wheel is Safari's, so a
  // conditional one would let a user who reached for ctrl zoom the *document* on top of the surface.
  // What it cannot do is reach past the browser — macOS's own ctrl+scroll zoom (Accessibility →
  // Zoom) consumes the event before any `wheel` is dispatched, which is why `wheelAxes` puts the
  // time axis on ALT and leaves ctrl to fall through to the uniform gesture (where a trackpad pinch,
  // which Chrome and Safari deliver as ctrl+wheel, also belongs).
  //
  // **What the gesture MEANS is not decided here.** `wheelZoom` reads the modifier bits and the
  // deltas; this host only forwards the event and places the result. The surface has a second host
  // after the cutover, and two hosts each doing their own wheel arithmetic is T-412's wheel-zoom
  // mismatch rebuilt — so the arithmetic lives in `preview.ts` and the listener stays this thin.
  canvas.addEventListener("wheel", (e) => {
    e.preventDefault();
    const p = point(e);
    const { factor, axes } = wheelZoom(e);
    if (preview.onMap(p)) { preview.wheelMap(p, factor, axes); return; }
    const pane = preview.paneAt(p);
    if (pane) { preview.activePane = pane; preview.wheel(pane, p, factor, axes); }
  }, { passive: false });

  // Double-click on the map sends the active pane there. A discrete act rather than a threshold on
  // a pointer stream — T-407's lesson, kept even though nothing here can reach a radio.
  canvas.addEventListener("dblclick", (e) => {
    const p = point(e);
    if (preview.onMap(p)) preview.goToOnMap(p);
  });

  // ——— size, and the loop ———
  const fit = () => {
    const r = canvas.getBoundingClientRect();
    preview.resize(r.width, r.height, window.devicePixelRatio || 1);
  };
  fit();
  window.addEventListener("resize", fit);
  preview.start();

  const status = slot("status");
  window.setInterval(() => {
    const s = preview.view.surface.cache.stats;
    const f = preview.lastFrame;
    text(status, [
      `${preview.view.panes.count} pane${preview.view.panes.count === 1 ? "" : "s"} + map`,
      `${preview.view.surface.cache.residentTiles} tiles resident (${(preview.view.surface.cache.residentBytes / 1048576).toFixed(1)} MB)`,
      // The abandoned count is shown next to the in-flight one because it is the same budget: an
      // aborted request keeps costing the route a slot until its read finishes (T-454).
      `${preview.view.surface.cache.inFlightCount}+${preview.view.surface.cache.abandonedSlots}/${preview.view.surface.cache.inFlightLimit} in flight`,
      `queue ${preview.view.surface.cache.queueDepth}`,
      `~${preview.view.surface.cache.serverEstimateMs.toFixed(0)} ms/tile`,
      `${s.uploads} uploads · ${s.evictions} evicted · ${s.cancelled} cancelled · ${s.abandoned} abandoned · ${s.busyRefusals} backpressure · ${s.failures} failed`,
      f ? `display range ${preview.view.surface.lo.toFixed(1)}…${preview.view.surface.hi.toFixed(1)} dBFS` : "",
    ].filter(Boolean).join("  ·  "));
  }, 500);
}

void main();
