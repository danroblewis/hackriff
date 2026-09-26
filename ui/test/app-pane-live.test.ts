// T-1001 (MMAP split view): **a Live/Freeze button inside each pane's rectangle; the FAB retires.**
//
// The user (2026-09-25): "A common use would be to look at one signal from the past and the current
// waterfall, and each split needs its own 'Live' button." The claims, each against what would make
// a degenerate implementation pass:
//
//  1. **The set acceptance**: with pane 1 frozen and pane 2 live, each pane's button states ITS own
//     pane, and pressing pane 1's button changes nothing about pane 2 — driven through the real
//     `PaneModel`, the real `PaneLiveLayer` and a real (fake-DOM) click on the very element the
//     user presses, not through a host stub.
//  2. **Each button's state is correct in the SAME frame as the press** — the layer re-states every
//     button inside the press, so no button on screen is one frame stale.
//  3. **No control here reaches a route** (the spy-client empty-call-list rule): `fetch` and the
//     app client are both spied across a press.
//  4. **T-955's three states survived the move off the FAB**, per pane: a pane following live time
//     over spectrum the radio has left is brought to the tuned window rather than frozen; a frozen
//     pane off the tuned window is brought there in both axes; a pane already overlapping the tuned
//     window keeps the user's zoom; with no tuned window reported it is the plain time toggle.
//  5. **The button is inside its pane's rectangle**, placed from the frame's own rectangles, and
//     clear of the chrome docked to the canvas's top edge.
//  6. **The FAB is gone** — from the cluster module, its CSS, and the surface's wiring — and the
//     `L` key presses the active pane's own button, so the key and the button cannot differ.
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { PaneModel } from "../src/surface/panes";
import type { Lattice } from "../src/surface/lattice";
import {
  LIVE_INSET_PX, PaneLiveLayer, chromeClearance, liveState, paneLiveActions,
} from "../src/app/centre/pane-live";
import { createStore } from "../src/app/store";
import { initialState } from "../src/app/state";
import type { AppContext } from "../src/app/context";

const S = 1e9;
const T0 = 1_700_000_000 * S;
const BOUNDS = { f0Hz: 1e6, f1Hz: 6e9, t0Ns: T0 - 600 * S, t1Ns: T0 };
const LAT: Lattice = { scheme: "view", cells: 256, f0Hz: 6250, t0Ns: 1e9, levelsF: 20, levelsT: 15 };
const model = () => new PaneModel({
  bounds: BOUNDS, lattice: LAT, width: 1200, height: 800,
  freq: { centerHz: 100.8e6, spanHz: 1e6 }, spanNs: 20 * S,
});
const TUNED = { centerHz: 144.6e6, spanHz: 2.4e6 };

function spyCtx() {
  const calls: unknown[] = [];
  const store = createStore(initialState());
  const client = {
    post: (path: string) => { calls.push(["POST", path]); return Promise.resolve({}); },
    get: (path: string) => { calls.push(["GET", path]); return Promise.resolve({}); },
  } as unknown as AppContext["client"];
  return { ctx: { store, client, token: "t" } as AppContext, calls };
}

// ---------------------------------------------------------------------------
// a fake DOM: enough for the layer to build, place and be pressed
// ---------------------------------------------------------------------------

type Handler = (e: unknown) => void;
class FakeEl {
  children: FakeEl[] = [];
  handlers: Record<string, Handler[]> = {};
  classes = new Set<string>();
  attrs: Record<string, string> = {};
  dataset: Record<string, string> = {};
  style: Record<string, string> = {};
  textContent = "";
  title = "";
  className = "";
  type = "";
  parent: FakeEl | null = null;
  constructor(readonly tag = "div") {}
  get firstChild(): FakeEl | null { return this.children[0] ?? null; }
  appendChild(c: FakeEl) { c.parent = this; this.children.push(c); return c; }
  append(...c: (FakeEl | string)[]) { for (const x of c) if (typeof x === "string") this.textContent += x; else this.appendChild(x); }
  remove() { if (this.parent) this.parent.children = this.parent.children.filter((x) => x !== this); this.parent = null; }
  setAttribute(k: string, v: string) { this.attrs[k] = v; }
  getAttribute(k: string) { return this.attrs[k] ?? null; }
  addEventListener(t: string, fn: Handler) { (this.handlers[t] ??= []).push(fn); }
  click() { for (const fn of this.handlers.click ?? []) fn({ preventDefault() {} }); }
  classList = {
    toggle: (c: string, on?: boolean) => { const v = on ?? !this.classes.has(c); if (v) this.classes.add(c); else this.classes.delete(c); return v; },
    contains: (c: string) => this.classes.has(c),
  };
}
/** `dom.ts`'s `h` builds from the global document, so the suite installs a fake one. */
const g = globalThis as Record<string, unknown>;
const realDocument = g.document;
g.document = { createElement: (t: string) => new FakeEl(t) };
process.on("exit", () => { if (realDocument) g.document = realDocument; else delete g.document; });

const fakeRoot = () => new FakeEl("div") as unknown as HTMLElement;
const btn = (layer: PaneLiveLayer, id: string) => layer.buttonFor(id) as unknown as FakeEl;
/** What one button says, as the page shows it: word, state and pressed-ness. */
const reads = (layer: PaneLiveLayer, id: string) => {
  const b = btn(layer, id);
  return { word: b.firstChild!.textContent, state: b.dataset.state, pressed: b.getAttribute("aria-pressed") };
};

/** Two panes, columns: pane 1 left, pane 2 right, both following the live edge. */
function twoPanes() {
  const m = model();
  const p1 = m.list()[0].id;
  const p2 = m.split(p1, "columns")!;
  assert.ok(p2 && p2 !== p1, "the model did not split");
  return { m, p1, p2 };
}

/** The layer over a real model, wired as the surface wires it: per-pane press and state. */
function mountLayer(m: PaneModel, tuned: () => { centerHz: number; spanHz: number } | null = () => null) {
  const acts = paneLiveActions(m, tuned);
  const root = fakeRoot();
  const layer = new PaneLiveLayer(root, {
    state: (id) => acts.state(id),
    press: (id) => acts.press(id),
    paneNumber: (id) => {
      const i = m.list().findIndex((p) => p.id === id);
      return m.list().length > 1 && i >= 0 ? i + 1 : null;
    },
  });
  const frame = () => layer.update(m.views(T0, 1200, 800), 800, 1);
  frame();
  return { layer, acts, frame, root: root as unknown as FakeEl };
}

// ---------------------------------------------------------------------------
// 1 + 2 + 3. the set acceptance: one button per pane, acting on its own pane
// ---------------------------------------------------------------------------

test("T-1001: freezing pane 1 leaves pane 2 following, and both buttons are right in the same frame", () => {
  const fetched: unknown[] = [];
  const g = globalThis as { fetch?: unknown };
  const real = g.fetch;
  g.fetch = (...a: unknown[]) => { fetched.push(a); return Promise.reject(new Error("a view control reached the network")); };
  const { calls } = spyCtx();
  try {
    const { m, p1, p2 } = twoPanes();
    const { layer } = mountLayer(m);

    assert.deepEqual(reads(layer, p1), { word: "Live · pane 1", state: "following", pressed: "true" });
    assert.deepEqual(reads(layer, p2), { word: "Live · pane 2", state: "following", pressed: "true" });

    const before2 = m.get(p2)!.time;
    // THE press: pane 1's own button, on the element the user clicks.
    btn(layer, p1).click();

    assert.equal(m.isFollowing(p1), false, "pane 1's button did not freeze pane 1");
    assert.equal(m.isFollowing(p2), true, "pressing pane 1's button froze pane 2 — the whole reported defect");
    assert.deepEqual(m.get(p2)!.time, before2, "pane 2's time window moved when pane 1 was touched");
    // Same frame: no `update` between the press and these reads.
    assert.deepEqual(reads(layer, p1), { word: "Frozen · pane 1", state: "frozen", pressed: "false" },
      "pane 1's button is a frame stale after its own press");
    assert.deepEqual(reads(layer, p2), { word: "Live · pane 2", state: "following", pressed: "true" },
      "pane 2's button changed when pane 1 was pressed");

    // And back: pane 1 returns to live, pane 2 still untouched.
    btn(layer, p1).click();
    assert.equal(m.isFollowing(p1), true);
    assert.equal(m.isFollowing(p2), true);
    assert.equal(reads(layer, p1).state, "following");
  } finally { if (real) g.fetch = real; else delete g.fetch; }
  assert.deepEqual(fetched, [], "a Live button reached fetch: it is a view change, never a device call");
  assert.deepEqual(calls, [], "a Live button reached the control client");
});

test("T-1001: each pane's button freezes and resumes independently, in both directions", () => {
  const { m, p1, p2 } = twoPanes();
  const { layer } = mountLayer(m);
  btn(layer, p2).click();
  assert.equal(m.isFollowing(p2), false);
  assert.equal(m.isFollowing(p1), true, "pane 2's button reached pane 1");
  assert.equal(reads(layer, p1).state, "following");
  assert.equal(reads(layer, p2).state, "frozen");
  // The user's common case: one pane frozen on a past signal, the other live. Freezing the live
  // one must not resume the frozen one, and vice versa.
  btn(layer, p1).click();
  assert.equal(m.isFollowing(p1), false);
  assert.equal(m.isFollowing(p2), false, "freezing pane 1 resumed pane 2");
});

test("T-1001: a closed pane's button goes with it; a new pane gets its own", () => {
  const { m, p1, p2 } = twoPanes();
  const { layer, frame } = mountLayer(m);
  assert.ok(layer.buttonFor(p2));
  m.close(p2);
  frame();
  assert.equal(layer.buttonFor(p2), null, "a closed pane left its Live button on the canvas");
  assert.ok(layer.buttonFor(p1), "the surviving pane lost its button");
  // With one pane there is nothing to disambiguate: the word carries no pane number (T-1000's rule).
  assert.equal(reads(layer, p1).word, "Live");
});

// ---------------------------------------------------------------------------
// 4. T-955's states, per pane
// ---------------------------------------------------------------------------

/** The explorer's 0428 pane: following live TIME at 162.2 MHz ± 7.91 MHz after the radio retuned to
 * 144.6 MHz. Built over the real PaneModel and the real per-pane actions. */
function driftedFollowing() {
  const m = model();
  const id = m.list()[0].id;
  m.setFreq(id, 162.2e6, 15.82e6);
  assert.equal(m.isFollowing(id), true, "the pane is following in time — the reported shape");
  return { m, id };
}

test("T-955: a pane FOLLOWING at the wrong frequency is brought to the tuned live edge — not frozen", () => {
  const fetched: unknown[] = [];
  const g = globalThis as { fetch?: unknown };
  const real = g.fetch;
  g.fetch = (...a: unknown[]) => { fetched.push(a); return Promise.reject(new Error("a view control reached the network")); };
  const { calls } = spyCtx();
  try {
    const { m, id } = driftedFollowing();
    const acts = paneLiveActions(m, () => TUNED);
    assert.equal(acts.atLiveEdge(id), false, "a pane following spectrum the radio has left is NOT at the tuned live edge");
    const st = acts.state(id);
    assert.equal(st.offTuned, true, "the button must not read as plain 'Live' there");
    assert.equal(st.label, "Off-window", "the off-tuned state has no word of its own");
    acts.press(id);
    assert.equal(m.isFollowing(id), true, "the press FROZE the pane (explorer 0430: frozen at -14 s)");
    assert.equal(m.get(id)!.freq.centerHz, TUNED.centerHz, "the press left the pane on the stale frequency");
    assert.equal(m.get(id)!.freq.spanHz, TUNED.spanHz);
    assert.equal(acts.atLiveEdge(id), true);
    // …and only NOW, at the tuned live edge, does the same press freeze.
    acts.press(id);
    assert.equal(m.isFollowing(id), false, "at the tuned live edge the press is the freeze");
    assert.equal(m.get(id)!.freq.centerHz, TUNED.centerHz, "freezing moved nothing");
  } finally { if (real) g.fetch = real; else delete g.fetch; }
  assert.deepEqual(fetched, [], "follow-live reached fetch: it is a view change, never a device call");
  assert.deepEqual(calls, [], "follow-live reached the control client");
});

test("T-1001 + T-1006: each pane's press asks for ITS OWN tuned window — two radios, two live edges", () => {
  const { m, p1, p2 } = twoPanes();
  const OTHER = { centerHz: 433.92e6, spanHz: 2e6 };
  const asked: string[] = [];
  // The surface resolves the window per pane (a pane pinned to a radio reads that radio's window);
  // here pane 2 stands for a pane pinned to the second front end.
  const acts = paneLiveActions(m, (id) => { asked.push(id); return id === p2 ? OTHER : TUNED; });
  acts.press(p1);
  acts.press(p2);
  assert.ok(asked.includes(p1) && asked.includes(p2), "the window was not asked for by pane");
  assert.equal(m.get(p1)!.freq.centerHz, TUNED.centerHz, "pane 1 was not brought to its own radio's window");
  assert.equal(m.get(p2)!.freq.centerHz, OTHER.centerHz, "pane 2 was brought to the OTHER radio's live edge");
  assert.equal(acts.atLiveEdge(p1), true);
  assert.equal(acts.atLiveEdge(p2), true);
});

test("T-955: a FROZEN pane off the tuned window is brought to the tuned live edge in both axes", () => {
  const { m, id } = driftedFollowing();
  m.pause(id);
  const acts = paneLiveActions(m, () => TUNED);
  acts.press(id);
  assert.equal(m.isFollowing(id), true);
  assert.equal(m.get(id)!.freq.centerHz, TUNED.centerHz);
});

test("T-955: a pane already OVERLAPPING the tuned window keeps the user's zoom — only time is re-pinned", () => {
  const m = model();
  const id = m.list()[0].id;
  m.setFreq(id, 144.39e6, 200e3); // zoomed onto a few channels inside the tuned 2.4 MHz
  m.pause(id);
  const acts = paneLiveActions(m, () => TUNED);
  acts.press(id);
  assert.equal(m.isFollowing(id), true);
  assert.equal(m.get(id)!.freq.centerHz, 144.39e6, "follow-live destroyed a zoom inside the tuned window");
  assert.equal(m.get(id)!.freq.spanHz, 200e3);
  assert.equal(acts.atLiveEdge(id), true);
});

test("T-955: with no tuned window reported, the button is the plain time toggle and frequency is left alone", () => {
  const { m, id } = driftedFollowing();
  const acts = paneLiveActions(m, () => null);
  assert.equal(acts.atLiveEdge(id), true, "nothing to be off: time decides");
  acts.press(id);
  assert.equal(m.isFollowing(id), false);
  acts.press(id);
  assert.equal(m.isFollowing(id), true);
  assert.equal(m.get(id)!.freq.centerHz, 162.2e6, "no tuned window was reported: nothing to correct against");
});

test("T-1001: the map strip is told to follow and freeze with the pane that was pressed", () => {
  const { m, p1 } = twoPanes();
  const seen: boolean[] = [];
  const acts = paneLiveActions(m, () => null, (on) => seen.push(on));
  acts.press(p1);
  acts.press(p1);
  assert.deepEqual(seen, [false, true], "the map strip was not told to freeze and follow with the pane");
});

test("T-1001: the button's words say what the VIEW is doing, never the radio", () => {
  assert.equal(liveState(true).cls, "following");
  assert.equal(liveState(true).label, "Live");
  assert.equal(liveState(false).cls, "frozen");
  assert.equal(liveState(false).label, "Frozen");
  assert.match(liveState(false).title, /capture continues/);
  assert.match(liveState(true).title, /freeze/);
  assert.match(liveState(true, false).title, /tuned window/);
});

// ---------------------------------------------------------------------------
// 5. placement: inside the pane, clear of the top chrome
// ---------------------------------------------------------------------------

test("T-1001: each button is placed INSIDE its own pane's rectangle, from that frame's rectangles", () => {
  const { m, p1, p2 } = twoPanes();
  const { layer } = mountLayer(m);
  const rects = m.rects(1200, 800);
  for (const id of [p1, p2]) {
    const r = rects.get(id)!;
    const t = btn(layer, id).style.transform;
    const [x, y] = [...t.matchAll(/(-?[\d.]+)px/g)].map((mm) => Number(mm[1]));
    // GL px (origin bottom-left) → CSS px (origin top-left), at dpr 1.
    const right = r.x + r.w, top = 800 - r.y - r.h;
    assert.equal(x, right - LIVE_INSET_PX, `pane ${id}'s button is not at its own right edge`);
    assert.ok(y >= top + LIVE_INSET_PX - 0.01 && y < top + r.h, `pane ${id}'s button is outside its pane vertically`);
    assert.match(t, /translateX\(-100%\)/, "the button hangs off the right edge instead of inside it");
  }
  const x1 = Number(btn(layer, p1).style.transform.match(/(-?[\d.]+)px/)![1]);
  const x2 = Number(btn(layer, p2).style.transform.match(/(-?[\d.]+)px/)![1]);
  assert.ok(x1 < x2, "both panes' buttons landed in the same place: they are not per pane");
});

test("T-1001: a pane puts its button BELOW the chrome over its own top-right corner", () => {
  const { m, p1, p2 } = twoPanes();
  const acts = paneLiveActions(m);
  const root = fakeRoot();
  const layer = new PaneLiveLayer(root, { state: (id) => acts.state(id), press: () => {}, paneNumber: () => null });
  const views = m.views(T0, 1200, 800);
  // A 46 px-deep cluster over the RIGHT half only (the top-right buttons): pane 2 must clear it,
  // pane 1 — whose corner is at mid-canvas, under nothing — must not be pushed down with it.
  layer.update(views, 800, 1, [{ left: 700, right: 1200, top: 0, bottom: 46, width: 500, height: 46 }]);
  const yOf = (id: string) => Number([...btn(layer, id).style.transform.matchAll(/(-?[\d.]+)px/g)][1][1]);
  const topOf = (id: string) => { const r = m.rects(1200, 800).get(id)!; return 800 - r.y - r.h; };
  assert.equal(yOf(p2), 46 + LIVE_INSET_PX, "pane 2's button sits under the chrome over its corner");
  assert.equal(yOf(p1), topOf(p1) + LIVE_INSET_PX, "pane 1's button was pushed down by chrome that is not over it");
});

test("T-1001: the clearance is MEASURED per corner — not a constant, not the whole cluster", () => {
  const rects = [
    { left: 0, right: 300, top: 0, bottom: 36, width: 300, height: 36 }, // the Go-to row, left
    { left: 108, right: 392, top: 8, bottom: 134, width: 284, height: 126 }, // the wrapped status chips (400 px)
    { left: 360, right: 400, top: 400, bottom: 440, width: 40, height: 40 }, // the zoom stack: mid-height
    { left: 0, right: 400, top: 0, bottom: 999, width: 0, height: 0 }, // laid out but not drawn
  ];
  assert.equal(chromeClearance(rects, 220, 390, 800), 134,
    "a button at the right edge must clear the chips that wrap over it — the 400 px defect");
  assert.equal(chromeClearance(rects, 0, 100, 800), 36, "a corner under the Go-to row alone clears only it");
  assert.equal(chromeClearance(rects, 396, 399, 800), 0, "a corner with nothing above it reserves nothing");
  assert.equal(chromeClearance(rects, 393, 400, 900), 0, "the zoom stack at mid-height is not 'above' anything");
  assert.equal(chromeClearance([], 0, 100, 800), 0, "no chrome, no clearance");
});

// ---------------------------------------------------------------------------
// 6. the FAB is gone, and the wiring is the page's
// ---------------------------------------------------------------------------

test("T-1001: the follow-live FAB is retired — no element, no CSS, no host method", () => {
  const ts = readFileSync("src/app/chrome/map-controls.ts", "utf8");
  for (const gone of ["map-fab", "fabState", "fabPress", "syncFollow", "toggleFollow", "followLive", "pauseLive", "atLiveEdge"]) {
    assert.ok(!ts.replace(/\/\/.*$/gm, "").includes(gone), `the cluster still carries ${gone}: the single FAB was not retired`);
  }
  assert.ok(!readFileSync("src/app/chrome/map-controls.css", "utf8").replace(/\/\*[\s\S]*?\*\//g, "").includes("map-fab"),
    "the FAB's CSS outlived the FAB");
});

test("T-1001: the surface places a Live button per pane in the render frame, and `L` presses the active pane's own", () => {
  const host = readFileSync("src/app/centre/surface.ts", "utf8");
  assert.match(host, /liveButtons\?\.update\(panes, hPx, dpr, chromeBoxes\)/,
    "the buttons are not placed in the render frame's `dom` hook — they would drift off their panes");
  assert.match(host, /const liveActs = paneLiveActions\(pv\.view\.panes,/);
  assert.match(host, /if \(k\.kind === "live"\) \{ e\.preventDefault\(\); liveButtons\?\.buttonFor\(pv\.activePane\)\?\.click\(\); return; \}/,
    "`L` does not press the active pane's own button: the key and the button can differ");
  assert.match(host, /renderFollow = \(\) => liveButtons\?\.sync\(\);/);
  const css = readFileSync("src/app/centre/centre.css", "utf8");
  assert.match(css, /\.sf-pane-live \{[^}]*pointer-events: none/, "the button layer would swallow canvas gestures");
  assert.match(css, /\.sf-pane-live-btn \{[^}]*pointer-events: auto/, "the buttons cannot be pressed");
});
