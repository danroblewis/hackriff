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
import { attachSurfaceInput } from "./input";
import { legendEntries, swatchPixels } from "./legend";
import { SurfacePreview, isBackpressure, probeSurface } from "./preview";

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

  // ——— pointer ———
  //
  // **T-445 moved the handlers to `./input.ts`, and T-456's semantics went with them.** The cutover
  // gives this surface a *second* host — the app's Explore centre — and two hosts each doing their
  // own wheel and drag arithmetic is T-412's wheel-zoom mismatch rebuilt. So the listeners are
  // registered in one file, the meaning of a wheel is `preview.ts`'s `wheelZoom`, and this page and
  // the app cannot disagree about either.
  attachSurfaceInput(canvas, preview);

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
