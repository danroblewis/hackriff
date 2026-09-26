// T-802 (MAP-02): the floating control cluster — Go-to, layers, zoom (T-1001 retired the FAB; the
// per-pane Live button is `ui/test/app-pane-live.test.ts`'s).
//
// The claims, each against what would make a degenerate implementation pass:
//  1. **Every control is view arithmetic and reaches no route** (docs/23 §4/§10.4, the spy-client
//     empty-call-list rule). Driven over a REAL `PaneModel` through the very `paneActions` the page
//     mounts, with `fetch` and the app client both spied — and with the control that the pane
//     actually moved, so "the buttons do nothing" cannot pass.
//  2. **Zoom keeps a following pane on the growing edge.**
//  3. **The fade is idle-driven and never fades while held** (an open menu, a focused control).
//  4. **The only device path is the retune OFFER's explicit press**, and it is the pane row's gate:
//     the cluster module itself names no route, no client and no `DeviceAction`.
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { PaneModel } from "../src/surface/panes";
import type { Lattice } from "../src/surface/lattice";
import { IDLE_MS, IdleFade, ZOOM_STEP, mountMapControls, paneActions, parseGoto } from "../src/app/chrome/map-controls";
import { createStore } from "../src/app/store";
import { initialState, requestGoto } from "../src/app/state";
import type { AppContext } from "../src/app/context";

const S = 1e9;
const T0 = 1_700_000_000 * S;
const BOUNDS = { f0Hz: 1e6, f1Hz: 6e9, t0Ns: T0 - 600 * S, t1Ns: T0 };
const LAT: Lattice = { scheme: "view", cells: 256, f0Hz: 6250, t0Ns: 1e9, levelsF: 20, levelsT: 15 };
const model = () => new PaneModel({
  bounds: BOUNDS, lattice: LAT, width: 1200, height: 800,
  freq: { centerHz: 100.8e6, spanHz: 1e6 }, spanNs: 20 * S,
});

function spyCtx() {
  const calls: unknown[] = [];
  const store = createStore(initialState());
  const client = {
    post: (path: string) => { calls.push(["POST", path]); return Promise.resolve({}); },
    get: (path: string) => { calls.push(["GET", path]); return Promise.resolve({}); },
  } as unknown as AppContext["client"];
  return { ctx: { store, client, token: "t" } as AppContext, calls };
}

test("MAP-02: zoom, go-to and layer toggles change the view and reach NO route", () => {
  const fetched: unknown[] = [];
  const g = globalThis as { fetch?: unknown };
  const real = g.fetch;
  g.fetch = (...a: unknown[]) => { fetched.push(a); return Promise.reject(new Error("a view control reached the network")); };
  const { ctx, calls } = spyCtx();
  try {
    const m = model();
    const id = m.list()[0].id;
    // The page's own wiring: go-to is `requestGoto` in the store, which the surface turns into
    // `setFreq` on the active pane (surface.ts's `nav.gotoHz` subscriber) — reproduced here.
    ctx.store.select((s) => s.nav.gotoHz, (hz) => { if (hz !== null) m.setFreq(id, hz, m.get(id)!.freq.spanHz); });
    // T-995: no map strip to keep in step — the minimap is retired; T-1001: follow/freeze is each
    // pane's own button, so `paneActions` is the zoom arithmetic and nothing else.
    const acts = paneActions(m, () => id);
    const span0 = m.get(id)!.freq.spanHz;

    for (let i = 0; i < 4; i++) acts.zoom(ZOOM_STEP);
    const zoomedIn = m.get(id)!.freq.spanHz;
    assert.ok(zoomedIn < span0, `zoom-in did not narrow the pane: ${span0} -> ${zoomedIn}`);
    for (let i = 0; i < 12; i++) acts.zoom(1 / ZOOM_STEP);
    assert.ok(m.get(id)!.freq.spanHz > zoomedIn, "zoom-out did not widen the pane");

    ctx.store.set(requestGoto(433.92e6));
    assert.equal(m.get(id)!.freq.centerHz, 433.92e6, "go-to did not move the pane");

    // A layer toggle is a closure over a display flag; nothing here can be reached through it.
    let shown = true;
    const layer = { id: "signals", label: "Found signals", on: () => shown, toggle: () => { shown = !shown; } };
    layer.toggle(); layer.toggle();
    assert.equal(shown, true);
  } finally { if (real) g.fetch = real; else delete g.fetch; }
  assert.deepEqual(fetched, [], "a floating control reached fetch");
  assert.deepEqual(calls, [], "a floating control reached the control client");
});

test("MAP-02: zooming a following pane keeps it on the growing edge", () => {
  const m = model();
  const id = m.list()[0].id;
  const acts = paneActions(m, () => id);
  assert.equal(acts.isFollowing(), true);
  acts.zoom(ZOOM_STEP);
  acts.zoom(1 / ZOOM_STEP);
  assert.equal(acts.isFollowing(), true, "a zoom button walked a live pane off the edge");
  assert.equal(m.get(id)!.time.live, true);
});

// T-1001: follow/freeze moved OFF this cluster and into each pane's own Live button — its states
// (T-955's three), its press and its independence per pane are `ui/test/app-pane-live.test.ts`'s.

test("MAP-02: no active pane means every control is a no-op, not a throw", () => {
  const m = model();
  const acts = paneActions(m, () => null);
  acts.zoom(ZOOM_STEP);
  assert.equal(acts.isFollowing(), false);
});

test("MAP-02: the cluster fades after idle, returns on activity, and never fades while held", () => {
  let pending: (() => void) | null = null;
  const timers = { set: (fn: () => void) => { pending = fn; return 1; }, clear: () => { pending = null; } };
  const seen: boolean[] = [];
  const f = new IdleFade((idle) => seen.push(idle), IDLE_MS, timers);
  assert.ok(pending, "the idle timer was not armed at mount");
  pending!();
  assert.equal(f.isIdle, true);
  f.poke();
  assert.equal(f.isIdle, false);
  assert.deepEqual(seen, [true, false]);
  // Held (an open layers menu): no timer runs, so it cannot fade.
  f.hold("layers", true);
  assert.equal(pending, null, "a held cluster still armed the idle timer");
  f.hold("layers", false);
  assert.ok(pending, "releasing the hold did not re-arm the fade");
  assert.equal(IDLE_MS, 6000, "docs/23 §10.2: ~6 s idle");
});

test("MAP-02: go-to parsing", () => {
  assert.deepEqual(parseGoto("433.92M", null), { hz: 433.92e6 });
  assert.deepEqual(parseGoto("101.3", null), { hz: 101.3e6 });
  assert.deepEqual(parseGoto("+200k", 100e6), { hz: 100.2e6 });
  assert.ok("error" in parseGoto("+200k", null), "a relative entry with no tuned centre must refuse, not guess");
  assert.ok("error" in parseGoto("fm", 100e6));
});

test("MAP-02: the cluster names no route; its one device path is the painted retune offer", () => {
  const src = readFileSync("src/app/chrome/map-controls.ts", "utf8").replace(/\/\/.*$/gm, "");
  for (const banned of ["fetch(", "/api/", "client.", "applyDeviceAction", "acceptPaneRetune", "acceptPaneWidth", "retunePlan"]) {
    assert.ok(!src.includes(banned), `map-controls.ts reaches for ${banned}: the cluster is view-only`);
  }
  const host = readFileSync("src/app/centre/surface.ts", "utf8");
  // The go-to offer is shown only where no tuned window covers the pane, and — since T-947 — pressed
  // through `acceptPaneWidth`, planning the device's OWN current span (or a caller default), never
  // the pane's viewport: the pane-row Retune (`pressOffer` → `acceptPaneRetune`) is untouched.
  // T-955: "covered" is whether a tuned window — the active windows OR `frequency.current`, so a
  // retune by anyone counts the moment the poll reports it — holds the pane's CENTRE (a Go-to names
  // a centre), not whether it holds the whole viewport.
  assert.match(host, /const heldNow = !!pane && \(coveringWindow\(windows, c, c, pane\.device\) !== null\s*\|\| \(!!cur && Math\.abs\(c - cur\.center_hz\) <= cur\.span_hz \/ 2\)\);/);
  assert.match(host, /if \(!o \|\| heldNow\) \{ lastPaintedGoto = null; return null; \}/);
  assert.match(host, /press: pressGotoOffer/);
  assert.match(host, /const pressGotoOffer = [^]*?acceptPaneWidth\(ctx, \{/);
  assert.match(host, /const gotoSpanHz = \(\): number => goToSpanHz\(/);
  assert.match(host, /const pressRetune = [^]*?if \(o\) pressOffer\(o\);/);
});

test("MAP-02: fade rules — idle chrome dims, the offer and focused controls never do", () => {
  const css = readFileSync("src/app/chrome/map-controls.css", "utf8");
  assert.match(readFileSync("src/app/app.css", "utf8"), /map-controls\.css/);
  assert.match(css, /\.map-ctl\.is-idle \.map-fade:not\(:focus-within\) \{ opacity: \.35; \}/);
  assert.match(css, /\.map-ctl \{[^}]*pointer-events: none/, "the cluster's box must not swallow canvas gestures");
  const ts = readFileSync("src/app/chrome/map-controls.ts", "utf8");
  assert.match(ts, /class: "map-glass map-offer", role/, "the retune offer must not carry map-fade");
  assert.match(ts, /fade\.hold\("layers", open\)/);
});

// T-882's claim that the FAB carried the retired toolbar `Live` button's press is now
// T-1001's: the press lives on each pane's own button (`ui/test/app-pane-live.test.ts`), which
// asserts the freeze, the re-pin, T-955's states and the empty call list — per pane, so a press
// on pane 1 cannot reach pane 2.

test("T-882: the rehomed controls live in the cluster — Measure, the viewport menu, the colour scale — and no toolbar row remains", () => {
  const ts = readFileSync("src/app/chrome/map-controls.ts", "utf8");
  assert.match(ts, /class: "map-ibtn map-measure-btn"/);
  assert.match(ts, /class: "map-ibtn map-pane-btn"/);
  for (const act of ["split", "close", "whole"]) assert.match(ts, new RegExp(`paneItem\\("${act}"`));
  // T-1007 moved the colour scale one menu across — out of Layers (a per-pane picture) and into the
  // ⋯ settings menu, which is where the view-wide preferences now live. Still in the cluster, still
  // one radio group over the one range mode: the assertion follows the control, it is not dropped.
  const settings = readFileSync("src/app/chrome/settings.ts", "utf8");
  assert.match(settings, /group\("scale", "Colour scale[^"]*", "Colour scale, every pane", "radiogroup"/);
  assert.doesNotMatch(ts, /"data-axis": "scale"/, "the colour scale is still offered by the layers menu too");
  assert.match(ts, /fade\.hold\("pane-menu", open\)/, "an open viewport menu must not fade");
  const host = readFileSync("src/app/centre/surface.ts", "utf8");
  assert.doesNotMatch(host, /sf-bar|sf-actions|sf-live|sf-tracebtn|sf-contrast|sf-vscale|sf-signalsbtn|sf-measurebtn/,
    "a retired toolbar control is still built by the surface mount");
  // T-918: the stage is the surface's ONLY child — full-bleed, no bar above it and no row below it
  // (docs/23 §10.1: no chrome subtracts from the canvas). The statuses that were rows under the
  // stage float over it in one bottom-left stack, as screen-space chrome, with the readout.
  assert.match(host, /el\.replaceChildren\(stage\);/, "the stage is the surface's only row: full-bleed, nothing above or below it");
  // T-996: the statuses float over the stage as ONE LINE — no white per-viewport panel, no `More`.
  assert.match(host, /h\("div", \{ class: "sf-status", "data-band": "chrome" \}, fogEl, priorsEl, statusLine, said\)/,
    "the statuses do not float over the stage as one line with its conditional statements");
  assert.match(host, /paneMenuExtras: \[recordBtn\]/, "Record IQ has a home in the viewport menu");
});

// T-996 (user, 2026-09-25, map-UI nit-picks via the supervisor): the white per-viewport panel
// ("100.980 MHz ± 937 kHz · LIVE") and the `More` expansion behind it are RETIRED. What is on the
// picture instead: a Google-Maps-style scale bar bottom-right of every pane, and one line
// bottom-left. What is compiled in here is the composition; that the bar's pixels match the pane's
// own Hz/px and s/px at three zooms, and that it all fits at 400 px, is `ui/e2e/app-status.e2e.mjs`.
test("T-996: no white centre/span panel and no `More` expansion — a scale bar per pane and one status line", () => {
  const host = readFileSync("src/app/centre/surface.ts", "utf8");
  // Gone: the panel, its toggle, its dismiss, and the per-viewport rows the app used to mount.
  for (const [re, why] of [
    [/sf-status-toggle/, "the `More` toggle is still built"],
    [/sf-status-body/, "the expansion body is still built"],
    [/sf-status-close/, "the expansion's dismiss is still built"],
    [/class: "sf-chrome"/, "the app still mounts the white per-viewport panel"],
    [/setStatusOpen/, "the expand/collapse machinery is still here"],
  ] as const) assert.doesNotMatch(host, re, why);
  // The app no longer hands `SurfaceChrome` an element at all (the developer preview still does).
  assert.doesNotMatch(host, /\bchrome, minimapPx|chromeAction, onChromeAction/,
    "the surface still mounts the per-viewport chrome rows");

  // There: the per-pane scale layer, placed inside the render frame from that frame's own statuses.
  assert.match(host, /class: "sf-scales"/, "no scale-bar layer over the canvas");
  assert.match(host, /new ScaleBars\(scaleEl\)/);
  assert.match(host, /const scaleFrame = \([^]*?statuses: readonly PaneStatus\[\],[^]*?\) => \{/,
    "the scale bars are not built from the frame's own per-viewport statuses");
  assert.match(host, /scaleFrame\(panes, hPx, dpr, statuses\);/,
    "the scale bars are not laid out in the render frame (a poll would state last second's zoom)");
  assert.match(host, /scaleMarkOf\(/);
  // The KEPT readout: where the viewport is looking, and peak dB only under the pointer.
  assert.match(host, /class: "sf-where"/, "the centre/span/LIVE readout was dropped, not kept");
  assert.match(host, /setText\(whereEl, \[st\.freqLabel, st\.timeLabel/,
    "the kept readout is not written from the frame's own status");
  assert.match(host, /hoverPeak = slicePk \?/, "peak dB is not carried to the hover readout");
  assert.match(host, /const peak = hoverPeak && hit && hit\.pane\.id === preview\?\.activePane/,
    "peak dB is not scoped to the pane under the pointer");

  // T-476/T-496's device controls kept their arithmetic and moved to where Go-to lives.
  assert.match(host, /paneRetune: \(\) => chromeAction\(pv\.activePane\)/);
  assert.match(host, /pressPaneRetune: \(\) => pressRetune\(pv\.activePane\)/);
  assert.match(host, /paneWidths: \(\) => widthActions\(pv\.activePane\)/);
  assert.match(host, /renderRetune\(\);/, "the cluster's Retune is not re-derived per render frame");
  const ts = readFileSync("src/app/chrome/map-controls.ts", "utf8");
  assert.match(ts, /class: "map-glass map-retune"/, "the capture controls have no home under Go-to");
  assert.match(ts, /retuneGo\.addEventListener\("click", \(\) => \{ if \(!retuneGo\.disabled\) host\.pressPaneRetune\?\.\(\); \}\)/,
    "the Retune press is not a plain click on a persistent button (T-407: never a gesture threshold)");
  // A width preset crosses as an OPAQUE key: the cluster routes the press back by the key it was
  // handed and never parses what it means (the host does, on the other side of the boundary).
  assert.match(ts, /host\.pressPaneWidth\?\.\(rec\.key\)/);
  assert.doesNotMatch(ts, /WIDTH_PRESETS|Number\(rec\.key\)/, "the cluster learned what a width preset means");

  const css = readFileSync("src/app/centre/centre.css", "utf8");
  assert.match(css, /\.sf-status \{[\s\S]*?max-width: min\(560px, calc\(100% - 80px\)\)/,
    "the status has no width bound, or runs under the right-edge cluster");
  assert.match(css, /\.sf-scale \{[\s\S]*?translate: calc\(-100% - 56px\) calc\(-100% - 12px\)/,
    "the scale block is not anchored inside its pane's bottom-right corner");
  assert.match(css, /\.sf-scales \{[^}]*pointer-events: none/,
    "the scale layer can take a pan from the surface under it");
  assert.doesNotMatch(css, /\.sf-status:not\(\[data-open="true"\]\)/, "the collapse rules survive the retired panel");
});

test("T-995: with no minimap, the Zoom-out press reaches the WHOLE device range — the lock holds until time is saturated", () => {
  const m = model();
  const id = m.list()[0].id;
  const acts = paneActions(m, () => id);
  const f0 = m.get(id)!.freq.spanHz, t0 = m.get(id)!.time.spanNs;
  acts.zoom(1 / ZOOM_STEP);
  // While time can still widen, the press is T-472's uniform zoom: both spans by the same factor.
  const kF = m.get(id)!.freq.spanHz / f0, kT = m.get(id)!.time.spanNs / t0;
  assert.ok(kF > 1 && Math.abs(kF - kT) < 1e-9, `the aspect lock broke before time saturated: ${kF} vs ${kT}`);
  for (let i = 0; i < 60; i++) acts.zoom(1 / ZOOM_STEP);
  const p = m.get(id)!;
  assert.equal(p.freq.spanHz, BOUNDS.f1Hz - BOUNDS.f0Hz,
    `zooming out stopped at ${p.freq.spanHz} Hz: the whole spectrum is unreachable without a minimap`);
  assert.equal(acts.isFollowing(), true, "zooming out walked a live pane off the growing edge");
});

// ——— T-955: a painted Go-to offer is re-derived when the radio retunes (by anyone) ———

type Handler = (e: unknown) => void;
/** A lenient element: enough of the DOM for `mountMapControls` to build and for a test to press. */
class FakeEl {
  attrs: Record<string, string> = {};
  children: FakeEl[] = [];
  handlers: Record<string, Handler[]> = {};
  classes = new Set<string>();
  hidden = false;
  disabled = false;
  textContent = "";
  value = "";
  title = "";
  style = { setProperty() {}, removeProperty() {} };
  /** T-1028: `data-*` state a control writes (the retune chip's held marker). */
  dataset: Record<string, string> = {};
  classList = {
    add: (...c: string[]) => c.forEach((x) => this.classes.add(x)),
    remove: (...c: string[]) => c.forEach((x) => this.classes.delete(x)),
    toggle: (c: string, on?: boolean) => { const v = on ?? !this.classes.has(c); if (v) this.classes.add(c); else this.classes.delete(c); return v; },
    contains: (c: string) => this.classes.has(c),
  };
  constructor(readonly tag: string) {}
  setAttribute(k: string, v: string) { this.attrs[k] = v; if (k === "class") v.split(/\s+/).forEach((c) => c && this.classes.add(c)); if (k === "hidden") this.hidden = true; }
  getAttribute(k: string) { return this.attrs[k] ?? null; }
  removeAttribute(k: string) { delete this.attrs[k]; }
  /** T-1053: a node has one parent — appending it elsewhere MOVES it, as the DOM does. */
  parentElement: FakeEl | null = null;
  private adopt(x: FakeEl) {
    const old = x.parentElement;
    if (old) { const i = old.children.indexOf(x); if (i >= 0) old.children.splice(i, 1); }
    x.parentElement = this;
  }
  append(...c: (FakeEl | string)[]) { for (const x of c) if (typeof x !== "string") { this.adopt(x); this.children.push(x); } else this.textContent += x; }
  before(...c: FakeEl[]) {
    const p = this.parentElement!;
    for (const x of c) { p.adopt(x); p.children.splice(p.children.indexOf(this), 0, x); }
  }
  replaceChildren(...c: FakeEl[]) { this.children = []; this.append(...c); }
  addEventListener(t: string, fn: Handler) { (this.handlers[t] ??= []).push(fn); }
  removeEventListener() {}
  fire(t: string, e: unknown = { preventDefault() {} }) { for (const fn of this.handlers[t] ?? []) fn(e); }
  focus() {}
  contains() { return false; }
  querySelector() { return null; }
  querySelectorAll() { return []; }
  getBoundingClientRect() { return { width: 0, height: 0, top: 0, bottom: 0, left: 0, right: 0 }; }
  find(cls: string): FakeEl | null {
    if (this.classes.has(cls)) return this;
    for (const c of this.children) { const f = c.find(cls); if (f) return f; }
    return null;
  }
}

test("T-955: a painted Go-to offer is withdrawn when the radio retunes onto the pane, and re-worded when it retunes elsewhere", () => {
  const g = globalThis as Record<string, unknown>;
  const saved = { document: g.document, window: g.window };
  g.document = {
    createElement: (t: string) => new FakeEl(t),
    createElementNS: (_ns: string, t: string) => new FakeEl(t),
    createComment: () => new FakeEl("#comment"),
    querySelector: () => null,
    body: new FakeEl("body"),
    activeElement: null,
  };
  g.window = { addEventListener() {}, removeEventListener() {} };
  try {
    const m = model();
    const id = m.list()[0].id;
    // The host's offer is `offerNow`'s: null while a tuned window covers the pane, else a Retune
    // naming the span the radio would be given. `tuned` is what `frequency.current` says NOW.
    let tuned: { centerHz: number; spanHz: number } = { centerHz: 100.9e6, spanHz: 2.4e6 };
    const offerFor = () => {
      const p = m.get(id)!;
      if (m.overlapsFreq(id, tuned.centerHz, tuned.spanHz) && Math.abs(p.freq.centerHz - tuned.centerHz) < tuned.spanHz / 2) return null;
      return { why: `Retune to ${(p.freq.centerHz / 1e6).toFixed(4)} MHz at ${(tuned.spanHz / 1e6).toFixed(3)} MHz span`, enabled: true, press() {} };
    };
    const acts = paneActions(m, () => id, undefined, () => tuned);
    const host = {
      ...acts,
      measuring: () => false, setMeasuring() {}, goTo: (hz: number) => m.setFreq(id, hz, m.get(id)!.freq.spanHz),
      centreHz: () => tuned.centerHz, gotoOffer: offerFor, viewChanged() {}, toast() {},
      split() {}, closePane() {}, wholeSurface() {}, paneCount: () => 1,
      layerMenu: () => ({ pane: "this pane", bases: [], data: [], overlays: [], viewWide: [], scale: { rows: [], note: "" } }),
      setBase() {}, toggleOverlay() {}, toggleViewWide() {}, setScale() {},
    } as unknown as Parameters<typeof mountMapControls>[0];
    const c = mountMapControls(host);
    const root = c.el as unknown as FakeEl;
    const form = root.find("map-goto")!, offer = root.find("map-offer")!, why = root.find("map-offer-why")!;
    const input = form.children.find((x) => x.tag === "input")!;
    input.value = "162.2M";
    form.fire("submit");
    assert.equal(offer.hidden, false, "the Go-to offer is painted");
    assert.match(why.textContent, /162\.2000 MHz/);

    // The radio is retuned to 162.2 MHz (by the API, another client, or this page): the offer is
    // now for a window the radio already holds — it must not outlive the retune (explorer 0416→0428).
    tuned = { centerHz: 162.2e6, spanHz: 2.4e6 };
    c.tuningChanged();
    assert.equal(offer.hidden, true, "the stale 'Retune to 162.2 MHz' offer outlived the retune to 162.2 MHz");

    // Painted again, then the radio goes elsewhere: re-derived against the NEW tuned window.
    tuned = { centerHz: 100.9e6, spanHz: 2.4e6 };
    form.fire("submit");
    assert.equal(offer.hidden, false);
    tuned = { centerHz: 144.6e6, spanHz: 10e6 };
    c.tuningChanged();
    assert.equal(offer.hidden, false, "the pane is still off the tuned window: an offer still applies");
    assert.match(why.textContent, /10\.000 MHz span/, "the offer was not re-derived against the new tuned window");
  } finally {
    g.document = saved.document; g.window = saved.window;
  }
});

// ---------------------------------------------------------------------------
// T-1006: the front-end picker in the viewport menu
// ---------------------------------------------------------------------------

test("T-1006: the viewport menu picks the pane's front end, and no press reaches a route", () => {
  const g = globalThis as Record<string, unknown>;
  const saved = { document: g.document, window: g.window, fetch: g.fetch };
  const fetched: unknown[] = [];
  g.document = {
    createElement: (t: string) => new FakeEl(t),
    createElementNS: (_ns: string, t: string) => new FakeEl(t),
    createComment: () => new FakeEl("#comment"),
    querySelector: () => null,
    body: new FakeEl("body"),
    activeElement: null,
  };
  g.window = { addEventListener() {}, removeEventListener() {} };
  g.fetch = (...a: unknown[]) => { fetched.push(a); return Promise.reject(new Error("the picker must not reach the network")); };
  const picked: string[] = [];
  let perDevice = 0;
  // Two front ends attached, the pane on the union — the case that makes the picker necessary.
  let pane = "any";
  try {
    const acts = paneActions(model(), () => model().list()[0].id);
    const host = {
      ...acts,
      measuring: () => false, setMeasuring() {}, goTo() {}, centreHz: () => null,
      gotoOffer: () => null, viewChanged() {}, toast() {},
      split() {}, closePane() {}, wholeSurface() {}, paneCount: () => 1,
      layerMenu: () => ({ pane: "pane 1 of 2", bases: [], data: [], overlays: [], viewWide: [], scale: { rows: [], note: "" } }),
      setBase() {}, toggleOverlay() {}, toggleViewWide() {}, setScale() {},
      deviceMenu: () => ({
        pane: "pane 1 of 2",
        rows: [
          { id: "any", label: "Any front end", hint: "grey is the union of every radio; a retune must name one", on: pane === "any" },
          { id: "hackrf:aaab", label: "HackRF · 2.4 Msps", hint: "hackrf:aaab", on: pane === "hackrf:aaab" },
          { id: "rtlsdr:1", label: "RTL-SDR · 2.4 Msps", hint: "rtlsdr:1", on: pane === "rtlsdr:1" },
        ],
        note: "This viewport's grey is the union of every front end.",
        offer: { label: "One viewport per front end", why: "2 viewports, one pinned to each front end", enabled: true },
      }),
      setPaneDevice: (id: string) => { picked.push(id); pane = id; },
      splitPerDevice: () => { perDevice++; },
    } as unknown as Parameters<typeof mountMapControls>[0];
    const c = mountMapControls(host);
    const root = c.el as unknown as FakeEl;
    const btn = root.find("map-pane-btn")!, menu = root.find("map-pane-menu")!;
    const section = menu.find("map-pane-device")!;
    assert.equal(section.hidden, true, "the picker is built on open, not before");
    btn.fire("click");
    assert.equal(section.hidden, false, "opening the viewport menu must state which front end this pane draws");
    const rows = root.find("map-pane-devices")!;
    assert.equal(rows.children.length, 3, "the union plus both front ends");
    const inputs = rows.children.map((l) => l.children.find((x) => x.tag === "input")!);
    assert.deepEqual(inputs.map((i) => i.getAttribute("data-pane-device")), ["any", "hackrf:aaab", "rtlsdr:1"]);
    assert.deepEqual(inputs.map((i) => i.checked), [true, false, false], "the pane's own choice is marked");
    // Pressing the second radio pins the pane to it — a view change, not a device call.
    inputs[2].fire("change");
    assert.deepEqual(picked, ["rtlsdr:1"]);
    assert.deepEqual(rows.children.map((l) => l.children.find((x) => x.tag === "input")!.checked), [false, false, true],
      "the picker re-states the pane's choice in the same press");
    // …and the split offer.
    const perBtn = section.children.find((x) => x.getAttribute("data-pane-act") === "per-device")!;
    assert.equal(perBtn.disabled, false);
    perBtn.fire("click");
    assert.equal(perDevice, 1, "'one viewport per front end' must be pressable");
    assert.deepEqual(fetched, [], "a front-end pick reached the network: choosing whose grey a pane draws is a view change");
  } finally {
    g.document = saved.document; g.window = saved.window;
    if (saved.fetch) g.fetch = saved.fetch; else delete g.fetch;
  }
});

// ——— T-1028: retune mode's chip — the one control here whose STATE decides what a gesture does ———

test("T-1028: the retune-mode chip states the mode, toggles it, and still reaches no route itself", () => {
  const g = globalThis as Record<string, unknown>;
  const saved = { document: g.document, window: g.window };
  g.document = {
    createElement: (t: string) => new FakeEl(t),
    createElementNS: (_ns: string, t: string) => new FakeEl(t),
    createComment: () => new FakeEl("#comment"),
    querySelector: () => null,
    body: new FakeEl("body"),
    activeElement: null,
  };
  g.window = { addEventListener() {}, removeEventListener() {} };
  const fetched: unknown[] = [];
  const realFetch = g.fetch;
  g.fetch = (...a: unknown[]) => { fetched.push(a); return Promise.reject(new Error("the chip reached the network")); };
  try {
    const m = model();
    const id = m.list()[0].id;
    const acts = paneActions(m, () => id);
    // The surface host's own state: a mode flag and a momentary key, exactly the two bits
    // `RetuneModeController` exposes.
    let on = false, held = false;
    const host = {
      ...acts,
      measuring: () => false, setMeasuring() {},
      retuneMode: () => ({ on: on || held, held }),
      setRetuneMode: (next: boolean) => { on = next; },
      goTo() {}, centreHz: () => 100e6, gotoOffer: () => null, viewChanged() {}, toast() {},
      split() {}, closePane() {}, wholeSurface() {}, paneCount: () => 1,
      layerMenu: () => ({ pane: "this pane", bases: [], data: [], overlays: [], viewWide: [], scale: { rows: [], note: "" } }),
      setBase() {}, toggleOverlay() {}, toggleViewWide() {}, setScale() {},
    } as unknown as Parameters<typeof mountMapControls>[0];
    const c = mountMapControls(host);
    const root = c.el as unknown as FakeEl;
    const chip = root.find("map-retune-btn")!;
    const banner = root.find("map-retune-mode")!;

    // OFF is the default, and it is SAID: an unlit chip and no banner.
    assert.equal(chip.hidden, false, "a host that has the mode must show its chip");
    assert.equal(chip.getAttribute("aria-pressed"), "false");
    assert.equal(banner.hidden, true, "no banner while the mode is off");

    // The press turns it on through the host — the chip holds no state of its own.
    chip.fire("click");
    assert.equal(on, true, "the chip did not reach the host");
    assert.equal(chip.getAttribute("aria-pressed"), "true", "the mode is on and the chip does not say so");
    assert.ok(chip.classes.has("is-on"));
    assert.equal(banner.hidden, false, "the mode is on and nothing on screen says what a pan will now do");
    assert.match(banner.textContent, /pan or zoom/, "the banner must say what the mode does to a gesture");

    // The HELD form is marked apart from the latch: "ends when I let go" is a different promise.
    on = false; held = true;
    c.syncRetuneMode();
    assert.equal(chip.getAttribute("aria-pressed"), "true", "a held key is the mode being ON");
    assert.equal(chip.dataset.held, "true", "the momentary form is not distinguishable from the latch");
    held = false;
    c.syncRetuneMode();
    assert.equal(chip.getAttribute("aria-pressed"), "false");
    assert.equal(banner.hidden, true, "releasing the key left the banner claiming the mode is on");

    // The press a second time turns it off again, through the same host call.
    on = true;
    chip.fire("click");
    assert.equal(on, false);
  } finally {
    g.document = saved.document; g.window = saved.window;
    if (realFetch) g.fetch = realFetch; else delete g.fetch;
  }
  assert.deepEqual(fetched, [], "the chip itself must reach nothing: it changes what a GESTURE means");
});

test("T-1028 x T-996: retune mode's pane status is SAID on the capture block beside Retune", () => {
  // T-1028 put this line on the per-viewport row; T-996 retired those rows from the app, so the
  // line moved to the floating capture block. A retune the user's own pan asked for must never be
  // silent: shown while the host says something, hidden (not emptied) when it says nothing, and
  // shown even when the viewport offers no Retune of its own.
  const g = globalThis as Record<string, unknown>;
  const saved = { document: g.document, window: g.window };
  g.document = {
    createElement: (t: string) => new FakeEl(t),
    createElementNS: (_ns: string, t: string) => new FakeEl(t),
    createComment: () => new FakeEl("#comment"),
    querySelector: () => null,
    body: new FakeEl("body"),
    activeElement: null,
  };
  g.window = { addEventListener() {}, removeEventListener() {} };
  try {
    const m = model();
    const id = m.list()[0].id;
    let said: string | null = null;
    let offer: { label: string; why: string; enabled: boolean } | null = null;
    const host = {
      ...paneActions(m, () => id),
      measuring: () => false, setMeasuring() {},
      paneRetune: () => offer, pressPaneRetune() {},
      paneStatus: () => said,
      goTo() {}, centreHz: () => 100e6, gotoOffer: () => null, viewChanged() {}, toast() {},
      split() {}, closePane() {}, wholeSurface() {}, paneCount: () => 1,
      layerMenu: () => ({ pane: "this pane", bases: [], data: [], overlays: [], viewWide: [], scale: { rows: [], note: "" } }),
      setBase() {}, toggleOverlay() {}, toggleViewWide() {}, setScale() {},
    } as unknown as Parameters<typeof mountMapControls>[0];
    const c = mountMapControls(host);
    const root = c.el as unknown as FakeEl;
    const block = root.find("map-retune")!;
    const line = root.find("map-retune-status")!;
    const row = root.find("map-retune-row")!;
    assert.equal(block.hidden, true, "nothing to offer and nothing to say: no block");
    assert.equal(line.getAttribute("role"), "status", "a retune the pan asked for must reach a screen reader");

    said = "Retuning to 433.92 MHz (settling…)";
    c.syncRetune();
    assert.equal(block.hidden, false, "retune mode is acting on the viewport and the map says nothing");
    assert.equal(line.hidden, false);
    assert.equal(line.textContent, said);
    assert.equal(row.hidden, true, "a Retune button with no offer behind it was shown");

    offer = { label: "Retune", why: "to 433.92 MHz", enabled: true };
    c.syncRetune();
    assert.equal(row.hidden, false, "the viewport's own Retune was hidden by the status line");
    assert.equal(line.hidden, false);

    said = null;
    c.syncRetune();
    assert.equal(line.hidden, true, "the line claims a retune after the mode stopped saying one");
    assert.equal(block.hidden, false, "the persistent Retune went with the status line");
  } finally {
    g.document = saved.document; g.window = saved.window;
  }
});

// T-1053: nine chips reached 80 % of a 400 px pane — a bar by another name (T-1025) — and squeezed
// Go-to's input to 4 px. On a phone the four MODE chips fold into the ⋯ menu; what is asserted here
// is that the SAME elements move (so their handlers and pressed state go with them), both ways, as
// the width changes. That the result fits, is pressable and reads as chips is the browser tier's
// (`app-top-chrome`, `app-phone`, `app-surface` at 420 px).
test("T-1053: on a phone the mode chips fold into the ⋯ menu and come back when the pane widens", () => {
  const g = globalThis as Record<string, unknown>;
  const saved = { document: g.document, window: g.window, matchMedia: g.matchMedia };
  g.document = {
    createElement: (t: string) => new FakeEl(t),
    createElementNS: (_ns: string, t: string) => new FakeEl(t),
    createComment: () => new FakeEl("#comment"),
    querySelector: () => null,
    body: new FakeEl("body"),
    activeElement: null,
  };
  g.window = { addEventListener() {}, removeEventListener() {} };
  let phone = true;
  let onChange: (() => void) | null = null;
  const asked: string[] = [];
  g.matchMedia = (q: string) => {
    asked.push(q);
    return { get matches() { return phone; }, addEventListener: (_t: string, fn: () => void) => { onChange = fn; } };
  };
  try {
    const m = model();
    const id = m.list()[0].id;
    let on = false;
    const host = {
      ...paneActions(m, () => id),
      measuring: () => false, setMeasuring() {},
      annotating: () => null, setAnnotating() {},
      retuneMode: () => ({ on, held: false }),
      setRetuneMode: (next: boolean) => { on = next; },
      goTo() {}, centreHz: () => 100e6, gotoOffer: () => null, viewChanged() {}, toast() {},
      split() {}, closePane() {}, wholeSurface() {}, paneCount: () => 1,
      layerMenu: () => ({ pane: "this pane", bases: [], data: [], overlays: [], viewWide: [], scale: { rows: [], note: "" } }),
      setBase() {}, toggleOverlay() {}, toggleViewWide() {}, setScale() {},
    } as unknown as Parameters<typeof mountMapControls>[0];
    const root = mountMapControls(host).el as unknown as FakeEl;
    assert.deepEqual(asked, ["(max-width: 600px)"], "the fold follows the stylesheet's phone breakpoint");
    const row = root.find("map-topright")!, tools = root.find("map-more-tools")!, pane = root.find("map-pane-btn")!;
    const MODES = ["map-measure-btn", "map-annotate-btn", "map-pin-btn", "map-retune-btn"];
    const kids = (el: FakeEl) => el.children.map((c) => [...c.classes].find((k) => k.endsWith("-btn") || k.endsWith("-home")));

    // Phone: the row keeps the panels' buttons; the four modes are the ⋯ menu's Tools, in order.
    assert.deepEqual(kids(tools), MODES, "the mode chips are not in the ⋯ menu on a phone");
    assert.equal(tools.hidden, false, "the ⋯ menu's Tools group is hidden while it holds the modes");
    assert.deepEqual(kids(row), ["map-layers-btn", "map-research-btn", "map-pane-btn", "map-review-home", "map-more-btn"],
      "the phone's chip row: five small chips, the modes moved (never copied) out of it");
    // The moved chip is the same control: its press still reaches the host and states the mode.
    const chip = tools.children[3];
    chip.fire("click");
    assert.equal(on, true, "a folded retune chip no longer reaches the host");
    assert.equal(chip.getAttribute("aria-pressed"), "true");

    // Wider: they come back into the row, before the viewport button, and Tools is empty and hidden.
    phone = false;
    onChange!();
    assert.deepEqual(kids(tools), [], "the modes stayed in the ⋯ menu on a wide pane");
    assert.equal(tools.hidden, true);
    assert.deepEqual(kids(row).slice(0, 7), ["map-layers-btn", "map-research-btn", ...MODES, "map-pane-btn"],
      "the modes did not return to their place in the row");
    assert.equal(row.children.indexOf(pane), 6);
  } finally {
    g.document = saved.document; g.window = saved.window;
    if (saved.matchMedia) g.matchMedia = saved.matchMedia; else delete g.matchMedia;
  }
});

test("T-1028: the cluster module still names no route, no client and no DeviceAction", () => {
  const src = readFileSync("src/app/chrome/map-controls.ts", "utf8");
  for (const bad of ["/api/control/", "applyDeviceAction", "retuneAction", "controls/client"]) {
    assert.ok(!src.includes(bad), `map-controls.ts names ${bad}: the device stays behind the host`);
  }
});

test("T-1004: the offer's BUTTON says what the press does — 'Go live here' on a frozen pane, 'Retune' otherwise", () => {
  const g = globalThis as Record<string, unknown>;
  const saved = { document: g.document, window: g.window };
  g.document = {
    createElement: (t: string) => new FakeEl(t),
    createElementNS: (_ns: string, t: string) => new FakeEl(t),
    createComment: () => new FakeEl("#comment"),
    querySelector: () => null,
    body: new FakeEl("body"),
    activeElement: null,
  };
  g.window = { addEventListener() {}, removeEventListener() {} };
  try {
    const m = model();
    const id = m.list()[0].id;
    const tuned = { centerHz: 100.9e6, spanHz: 2.4e6 };
    // The host answers as `surface.ts` does: a frozen pane's offer is a go-live, and carries its own
    // word for the button; a live pane's offer is the plain Retune, which names no label at all.
    let pressed = 0, frozen = true;
    const offerFor = () => frozen
      ? { why: "This viewport is frozen behind the growing edge… Go live at 162.2000 MHz", enabled: true, label: "Go live here", press: () => { pressed++; } }
      : { why: "Retune to 162.2000 MHz at 2.400 MHz span", enabled: true, press: () => { pressed++; } };
    const acts = paneActions(m, () => id, undefined, () => tuned);
    const host = {
      ...acts,
      measuring: () => false, setMeasuring() {}, goTo: (hz: number) => m.setFreq(id, hz, m.get(id)!.freq.spanHz),
      centreHz: () => tuned.centerHz, gotoOffer: offerFor, viewChanged() {}, toast() {},
      split() {}, closePane() {}, wholeSurface() {}, paneCount: () => 2,
      layerMenu: () => ({ pane: "pane 1 of 2", bases: [], data: [], overlays: [], viewWide: [], scale: { rows: [], note: "" } }),
      setBase() {}, toggleOverlay() {}, toggleViewWide() {}, setScale() {},
    } as unknown as Parameters<typeof mountMapControls>[0];
    const c = mountMapControls(host);
    const root = c.el as unknown as FakeEl;
    const form = root.find("map-goto")!, go = root.find("map-offer-go")!, why = root.find("map-offer-why")!;
    const input = form.children.find((x) => x.tag === "input")!;

    input.value = "162.2M";
    form.fire("submit");
    assert.equal(go.textContent, "Go live here", "the button still says Retune while the press also unfreezes the pane");
    assert.equal(go.disabled, false, "a frozen pane's offer is takeable as a go-live");
    assert.match(why.textContent, /frozen behind the growing edge/);
    go.fire("click");
    assert.equal(pressed, 1);

    // The pane is at the live edge now: the same control is the plain retune again, by its word.
    frozen = false;
    form.fire("submit");
    assert.equal(go.textContent, "Retune", "the button kept the go-live word after the pane went live");
    // And a re-derivation on a tuning change re-states the word, not only the sentence.
    frozen = true;
    c.tuningChanged();
    assert.equal(go.textContent, "Go live here");
  } finally {
    g.document = saved.document; g.window = saved.window;
  }
});
