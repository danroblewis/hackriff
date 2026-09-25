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
import { IDLE_MS, IdleFade, ZOOM_STEP, fabState, paneActions, parseGoto } from "../src/app/chrome/map-controls";
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
    const mapFollow: boolean[] = [];
    const acts = paneActions(m, () => id, (on) => mapFollow.push(on));
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
    assert.deepEqual(mapFollow, [true], "the map strip was not told to follow with the pane");

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

test("T-955: follow-live brings a drifted pane back to the front end's OWN tuned window, not only its time", () => {
  const m = model();
  const id = m.list()[0].id;
  // A pane that has drifted off the tuned window while still frozen — a stale reload, a pan, a
  // retune elsewhere — reproduced directly rather than through the bootstrap: `setFreq` is exactly
  // what a stale-view reload would have left the pane showing.
  m.pause(id);
  m.setFreq(id, 162.2e6, 200e3);
  const tuned = { centerHz: 144.6e6, spanHz: 200e3 };
  const acts = paneActions(m, () => id, undefined, () => tuned);
  assert.equal(m.get(id)!.freq.centerHz, 162.2e6, "the drift is real before the press");
  acts.followLive();
  assert.equal(acts.isFollowing(), true);
  assert.equal(m.get(id)!.freq.centerHz, tuned.centerHz, "follow-live left the pane on the stale frequency");
  assert.equal(m.get(id)!.freq.spanHz, tuned.spanHz);
});

test("T-955: with no tuned window to give, follow-live still moves time and leaves frequency alone", () => {
  const m = model();
  const id = m.list()[0].id;
  m.pause(id);
  m.setFreq(id, 162.2e6, 200e3);
  const acts = paneActions(m, () => id, undefined, () => null);
  acts.followLive();
  assert.equal(acts.isFollowing(), true);
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
  assert.match(host, /if \(!o \|\| o\.covered\) \{ lastPaintedGoto = null; return null; \}/);
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
    const mapFollow: boolean[] = [];
    const acts = paneActions(m, () => id, (on) => mapFollow.push(on));
    assert.equal(acts.isFollowing(), true);
    const before = m.get(id)!.time;
    acts.pauseLive();
    assert.equal(acts.isFollowing(), false, "the FAB did not freeze a following pane");
    // T-442: freezing is a coordinate change — the frozen window is the one that was on screen.
    assert.equal(m.get(id)!.time.spanNs, before.spanNs);
    acts.followLive();
    assert.equal(acts.isFollowing(), true);
    assert.deepEqual(mapFollow, [false, true], "the map strip was not told to freeze and follow with the pane");
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
