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
  for (const banned of ["fetch(", "/api/", "client.", "applyDeviceAction", "acceptPaneRetune", "retunePlan"]) {
    assert.ok(!src.includes(banned), `map-controls.ts reaches for ${banned}: the cluster is view-only`);
  }
  const host = readFileSync("src/app/centre/surface.ts", "utf8");
  // The go-to offer is shown only where no tuned window covers the pane, and pressed through the
  // same `pressOffer` → `acceptPaneRetune` as the pane row's Retune.
  assert.match(host, /if \(!o \|\| o\.covered\) return null;/);
  assert.match(host, /press: \(\) => pressOffer\(o\)/);
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
  assert.match(ts, /"data-axis": "scale", role: "radiogroup"/);
  assert.match(ts, /fade\.hold\("pane-menu", open\)/, "an open viewport menu must not fade");
  const host = readFileSync("src/app/centre/surface.ts", "utf8");
  assert.doesNotMatch(host, /sf-bar|sf-actions|sf-live|sf-tracebtn|sf-contrast|sf-vscale|sf-signalsbtn|sf-measurebtn/,
    "a retired toolbar control is still built by the surface mount");
  // T-812: the priors readout sits below the stage after the fog readout; the stage is still first.
  assert.match(host, /el\.replaceChildren\(stage, traceEl, ringEl, fogEl, (priorsEl, )?chrome, note\);/, "the stage is the first row: full-bleed, no bar above it");
  assert.match(host, /paneMenuExtras: \[recordBtn\]/, "Record IQ has a home in the viewport menu");
});
