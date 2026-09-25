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
  assert.match(ts, /"data-axis": "scale", role: "radiogroup"/);
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
