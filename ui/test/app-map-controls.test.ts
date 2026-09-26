// T-802 (MAP-02): the floating control cluster — Go-to, layers, zoom, the follow-live FAB.
//
// The claims, each against what would make a degenerate implementation pass:
//  1. **Every control is view arithmetic and reaches no route** (docs/23 §4/§10.4, the spy-client
//     empty-call-list rule). Driven over a REAL `PaneModel` through the very `paneActions` the page
//     mounts, with `fetch` and the app client both spied — and with the control that the pane
//     actually moved, so "the buttons do nothing" cannot pass.
//  2. **Zoom keeps a following pane on the growing edge; the FAB re-pins a frozen one.**
//  3. **The fade is idle-driven and never fades while held** (an open menu, a focused control).
//  4. **The only device path is the retune OFFER's explicit press**, and it is the pane row's gate:
//     the cluster module itself names no route, no client and no `DeviceAction`.
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { PaneModel } from "../src/surface/panes";
import type { Lattice } from "../src/surface/lattice";
import { IDLE_MS, IdleFade, ZOOM_STEP, fabPress, fabState, mountMapControls, paneActions, parseGoto } from "../src/app/chrome/map-controls";
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

test("MAP-02: zoom, follow, go-to and layer toggles change the view and reach NO route", () => {
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
    // T-995: no map strip to keep in step — the minimap is retired, so `paneActions` has no
    // follow hook and follow/freeze is the active pane's alone.
    const acts = paneActions(m, () => id);
    const span0 = m.get(id)!.freq.spanHz;

    for (let i = 0; i < 4; i++) acts.zoom(ZOOM_STEP);
    const zoomedIn = m.get(id)!.freq.spanHz;
    assert.ok(zoomedIn < span0, `zoom-in did not narrow the pane: ${span0} -> ${zoomedIn}`);
    for (let i = 0; i < 12; i++) acts.zoom(1 / ZOOM_STEP);
    assert.ok(m.get(id)!.freq.spanHz > zoomedIn, "zoom-out did not widen the pane");

    ctx.store.set(requestGoto(433.92e6));
    assert.equal(m.get(id)!.freq.centerHz, 433.92e6, "go-to did not move the pane");

    m.pause(id);
    assert.equal(acts.isFollowing(), false);
    acts.followLive();
    assert.equal(acts.isFollowing(), true, "the FAB did not re-pin the pane to the growing edge");

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

// ——— T-955: the follow-live control's states are relative to the TUNED window's live edge ———

const TUNED = { centerHz: 144.6e6, spanHz: 2.4e6 };

/** The explorer's 0428 pane: following live TIME at 162.2 MHz ± 7.91 MHz after the radio retuned to
 * 144.6 MHz. Built over the real PaneModel and the real `paneActions`, with fetch spied. */
function driftedFollowing() {
  const m = model();
  const id = m.list()[0].id;
  m.setFreq(id, 162.2e6, 15.82e6);
  assert.equal(m.isFollowing(id), true, "the pane is following in time — the reported shape");
  return { m, id };
}

test("T-955: the FAB on a pane FOLLOWING at the wrong frequency brings it to the tuned live edge — it does not freeze it", () => {
  const fetched: unknown[] = [];
  const g = globalThis as { fetch?: unknown };
  const real = g.fetch;
  g.fetch = (...a: unknown[]) => { fetched.push(a); return Promise.reject(new Error("a view control reached the network")); };
  const { ctx, calls } = spyCtx();
  try {
    const { m, id } = driftedFollowing();
    const acts = paneActions(m, () => id, undefined, () => TUNED);
    assert.equal(acts.atLiveEdge(), false, "a pane following spectrum the radio has left is NOT at the tuned live edge");
    assert.equal(fabState(acts.isFollowing(), acts.atLiveEdge()).offTuned, true, "the FAB must not read as plain 'following' there");
    fabPress(acts);
    assert.equal(m.isFollowing(id), true, "the press FROZE the pane (explorer 0430: frozen at -14 s)");
    assert.equal(m.get(id)!.freq.centerHz, TUNED.centerHz, "the press left the pane on the stale frequency");
    assert.equal(m.get(id)!.freq.spanHz, TUNED.spanHz);
    assert.equal(acts.atLiveEdge(), true);
    // …and only NOW, at the tuned live edge, does the same press freeze.
    fabPress(acts);
    assert.equal(m.isFollowing(id), false, "at the tuned live edge the press is the freeze");
    assert.equal(m.get(id)!.freq.centerHz, TUNED.centerHz, "freezing moved nothing");
    void ctx;
  } finally { if (real) g.fetch = real; else delete g.fetch; }
  assert.deepEqual(fetched, [], "follow-live reached fetch: it is a view change, never a device call");
  assert.deepEqual(calls, [], "follow-live reached the control client");
});

test("T-955: a FROZEN pane off the tuned window is brought to the tuned live edge in both axes", () => {
  const { m, id } = driftedFollowing();
  m.pause(id);
  const acts = paneActions(m, () => id, undefined, () => TUNED);
  fabPress(acts);
  assert.equal(m.isFollowing(id), true);
  assert.equal(m.get(id)!.freq.centerHz, TUNED.centerHz);
});

test("T-955: a pane already OVERLAPPING the tuned window keeps the user's zoom — only time is re-pinned", () => {
  const m = model();
  const id = m.list()[0].id;
  m.setFreq(id, 144.39e6, 200e3); // zoomed onto a few channels inside the tuned 2.4 MHz
  m.pause(id);
  const acts = paneActions(m, () => id, undefined, () => TUNED);
  fabPress(acts);
  assert.equal(m.isFollowing(id), true);
  assert.equal(m.get(id)!.freq.centerHz, 144.39e6, "follow-live destroyed a zoom inside the tuned window");
  assert.equal(m.get(id)!.freq.spanHz, 200e3);
  assert.equal(acts.atLiveEdge(), true);
});

test("T-955: with no tuned window reported, the FAB is the plain time toggle and frequency is left alone", () => {
  const { m, id } = driftedFollowing();
  const acts = paneActions(m, () => id, undefined, () => null);
  assert.equal(acts.atLiveEdge(), true, "nothing to be off: time decides");
  fabPress(acts);
  assert.equal(m.isFollowing(id), false);
  fabPress(acts);
  assert.equal(m.isFollowing(id), true);
  assert.equal(m.get(id)!.freq.centerHz, 162.2e6, "no tuned window was reported: nothing to correct against");
});

test("MAP-02: no active pane means every control is a no-op, not a throw", () => {
  const m = model();
  const acts = paneActions(m, () => null);
  acts.zoom(ZOOM_STEP);
  acts.followLive();
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

test("MAP-02: go-to parsing and the FAB's words", () => {
  assert.deepEqual(parseGoto("433.92M", null), { hz: 433.92e6 });
  assert.deepEqual(parseGoto("101.3", null), { hz: 101.3e6 });
  assert.deepEqual(parseGoto("+200k", 100e6), { hz: 100.2e6 });
  assert.ok("error" in parseGoto("+200k", null), "a relative entry with no tuned centre must refuse, not guess");
  assert.ok("error" in parseGoto("fm", 100e6));
  assert.equal(fabState(true).cls, "following");
  assert.equal(fabState(false).cls, "frozen");
  assert.match(fabState(false).title, /capture continues/);
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

test("T-882: the FAB is the retired Live button too — it freezes a following pane and re-pins a frozen one, no route", () => {
  const fetched: unknown[] = [];
  const g = globalThis as { fetch?: unknown };
  const real = g.fetch;
  g.fetch = (...a: unknown[]) => { fetched.push(a); return Promise.reject(new Error("the FAB reached the network")); };
  try {
    const m = model();
    const id = m.list()[0].id;
    // T-995: no map strip to keep in step — the minimap is retired, so `paneActions` has no
    // follow hook and follow/freeze is the active pane's alone.
    const acts = paneActions(m, () => id);
    assert.equal(acts.isFollowing(), true);
    const before = m.get(id)!.time;
    acts.pauseLive();
    assert.equal(acts.isFollowing(), false, "the FAB did not freeze a following pane");
    // T-442: freezing is a coordinate change — the frozen window is the one that was on screen.
    assert.equal(m.get(id)!.time.spanNs, before.spanNs);
    acts.followLive();
    assert.equal(acts.isFollowing(), true);
    paneActions(m, () => null).pauseLive(); // no active pane: a no-op, not a throw
  } finally { if (real) g.fetch = real; else delete g.fetch; }
  assert.deepEqual(fetched, []);
  assert.equal(fabState(true).title.includes("freeze"), true, "the following FAB does not say a press freezes");
});

test("T-882: the rehomed controls live in the cluster — Measure, the viewport menu, the colour scale — and no toolbar row remains", () => {
  const ts = readFileSync("src/app/chrome/map-controls.ts", "utf8");
  assert.match(ts, /class: "map-ibtn map-measure-btn"/);
  assert.match(ts, /class: "map-ibtn map-pane-btn"/);
  for (const act of ["split", "close", "whole"]) assert.match(ts, new RegExp(`paneItem\\("${act}"`));
  assert.match(ts, /"data-axis": "scale", role: "radiogroup"/);
  assert.match(ts, /fade\.hold\("pane-menu", open\)/, "an open viewport menu must not fade");
  const host = readFileSync("src/app/centre/surface.ts", "utf8");
  assert.doesNotMatch(host, /sf-bar|sf-actions|sf-live|sf-tracebtn|sf-contrast|sf-vscale|sf-signalsbtn|sf-measurebtn/,
    "a retired toolbar control is still built by the surface mount");
  // T-918: the stage is the surface's ONLY child — full-bleed, no bar above it and no row below it
  // (docs/23 §10.1: no chrome subtracts from the canvas). The statuses that were rows under the
  // stage float over it in one bottom-left stack, as screen-space chrome, with the readout.
  assert.match(host, /el\.replaceChildren\(stage\);/, "the stage is the surface's only row: full-bleed, nothing above or below it");
  assert.match(host, /h\("div", \{ class: "sf-status", "data-band": "chrome", "data-open": "false" \}, statusBody, statusLine\)/,
    "the statuses float over the stage as one chrome stack, collapsed by default");
  assert.match(host, /paneMenuExtras: \[recordBtn\]/, "Record IQ has a home in the viewport menu");
});

// T-919 (user P1, docs/23 §10.6 rule 1 + §10.2): the floating status is a COMPACT LINE that
// expands on demand and has a visible dismiss — it is not a permanent 560 x 184 px panel over the
// waterfall. What is compiled in here is the composition and the toggle; that the collapsed box is
// actually small, that the tier/level statement is in it in every state, and that expand / collapse
// / dismiss work on the real page are `ui/e2e/app-status.e2e.mjs`'s to measure.
test("T-919: the status box is a collapsed line by default, with a toggle and a dismiss that never hides the honesty statements", () => {
  const host = readFileSync("src/app/centre/surface.ts", "utf8");
  // The paragraphs — the ones that made it tall — are the BODY, and the body starts hidden.
  assert.match(host, /class: "sf-status-body", id: "sf-status-body", hidden: true \},\s*\n?\s*traceEl, ringEl, fogEl, priorsEl, note\)/,
    "the sentences are not the collapsible body, or the body does not start closed");
  // The line is always on the picture, and carries the per-viewport level/tier row (§10.2: an
  // honesty statement is never hidden) and the colour-scale sentence (T-470).
  assert.match(host, /class: "sf-status-line" \},\s*\n?\s*chrome, readout,/,
    "the always-visible line must carry the viewport level readout and the colour-scale readout");
  for (const [re, why] of [
    [/class: "sf-status-toggle", type: "button", "aria-controls": "sf-status-body", "aria-expanded"/, "no accessible expand control"],
    [/class: "sf-status-close", type: "button", "aria-label": "Close the status detail"/, "no visible dismiss (P1)"],
    [/trackOverlay\("surface-status", \(\) => setStatusOpen\(false\)\)/, "the detail is not on the one Escape stack (T-900)"],
    [/statusClose\.addEventListener\("click", \(\) => setStatusOpen\(false\)\)/, "the dismiss does not close the detail"],
  ] as const) assert.match(host, re, why);
  // Dismissing returns to the LINE, never to nothing: nothing removes or hides `statusEl` itself.
  assert.doesNotMatch(host, /statusEl\.hidden|statusLine\.hidden|statusEl\.remove\(\)/,
    "the status line itself can be hidden, which would switch an honesty statement off");
  const css = readFileSync("src/app/centre/centre.css", "utf8");
  assert.match(css, /\.sf-status \{[\s\S]*?max-width: min\(560px, calc\(100% - 80px\)\)/,
    "the status has no width bound, or runs under the right-edge cluster");
  assert.match(css, /\.sf-status:not\(\[data-open="true"\]\) \.sf-chrome \{[^}]*max-height: 46px/,
    "the collapsed viewport row is not bounded to a line");
  // What the collapse may NOT put away: the level/tier (§10.2) and the offer's own sentence, which
  // is what a press consents to (T-476's painted-offer rule).
  for (const keep of ["hk-surface-level", "hk-surface-why"]) {
    assert.doesNotMatch(css, new RegExp(`\\.sf-status:not\\(\\[data-open="true"\\]\\) \\.${keep} \\{[^}]*display: none`),
      `the collapsed line hides .${keep}`);
  }
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
  append(...c: (FakeEl | string)[]) { for (const x of c) if (typeof x !== "string") this.children.push(x); else this.textContent += x; }
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

test("T-1028: the cluster module still names no route, no client and no DeviceAction", () => {
  const src = readFileSync("src/app/chrome/map-controls.ts", "utf8");
  for (const bad of ["/api/control/", "applyDeviceAction", "retuneAction", "controls/client"]) {
    assert.ok(!src.includes(bad), `map-controls.ts names ${bad}: the device stays behind the host`);
  }
});
