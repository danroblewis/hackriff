// T-809 (MAP-09): pins — signal/event markers with rest / hover (MapTip) / selected states
// (docs/24 §14, ADR-0023 §3; AWARE-053). The claims, each against the degenerate implementation
// that would otherwise pass:
//
//  1. **A pin stands for a served object and never fabricates a timespan**: only Candidate/Confirmed
//     rows with a stated interval are pinned (a row with none gets no pin, as it gets no box), and
//     the pin sits INSIDE its interval — for an ongoing signal, at the live edge by assumption.
//  2. **Placed in capture time/Hz and moves with pan/zoom**: the same box shifted in time moves the
//     pin by exactly the pixels the rows move; a pin outside the pane is not clamped onto it.
//  3. **Shape carries the kind**, and detection vs curated markers stay distinguishable in class,
//     source and accessible name — never hue alone.
//  4. **MapTip** states centre / bandwidth / family suggestion / on-air, from the row as served.
//  5. **Picking is a quadtree** that agrees with brute force; arrow keys step to the right neighbour.
//  6. **The cap** (400 per pane) is enforced, the excess counted and stated, curated/confirmed kept.
//  7. **Wired into the frame**: the `dom` hook gets the SAME pane views the data pass drew, per frame.
//  8. **Thin client**: hover/focus/select reach no route; the module names no route or client.
//
// The measurement §14.4 owes (layout + index + hit-test at the cap) is printed by test 6b.

import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import {
  PIN_CAP_PER_PANE, PinIndex, PinLayer, curatedPins, detectionPins, layoutPanePins, neighbourOf,
  pinLabel, pinTipLines, type Pin, type PinRow, type PlacedPin,
} from "../src/surface/pins";
import type { PaneRect } from "../src/surface/surface";
import { keyOf, type Lattice, type TileAddr } from "../src/surface/lattice";
import { CELL } from "../src/surface/cellrule";
import { TileCache } from "../src/surface/tilecache";
import type { TileData } from "../src/surface/tile";
import { SurfaceView } from "../src/surface/view";
import type { PaneView } from "../src/surface/surface";
import { stubGl } from "./surface-glstub";

const S = 1e9;
const T0 = 1_700_000_000; // capture-clock seconds
const BOX = { f0Hz: 100e6, f1Hz: 102e6, t0Ns: (T0 - 20) * S, t1Ns: T0 * S };
// GL-convention rect, 1000 x 500 device px, in a 500 px-high canvas; dpr 1.
const RECT: PaneRect = { x: 0, y: 0, w: 1000, h: 500 };
const near = (a: number, b: number, eps = 1e-6) => assert.ok(Math.abs(a - b) <= eps, `${a} ≉ ${b}`);

function row(id: string, over: Partial<PinRow> = {}): PinRow {
  return {
    id, state: "candidate", f_center_hz: 101e6, bandwidth_hz: 200e3, f_lo_hz: 100.9e6, f_hi_hz: 101.1e6,
    family: "FM broadcast", explanations: [{ label: "FM broadcast" }],
    presence: { last_interval: { t_start_s: T0 - 10, t_end_s: T0 - 5, open: false } },
    ...over,
  };
}

// ---------------------------------------------------------------------------
// 1 + 3 + 4: what is pinned, as what, saying what
// ---------------------------------------------------------------------------

test("only served Candidate/Confirmed rows with an interval are pinned; kind is the served state", () => {
  const pins = detectionPins([
    row("c", { state: "confirmed" }),
    row("k"),
    row("u", { family: null, explanations: [] }),
    row("noiv", { presence: { last_interval: null } }),
    row("nopres", { presence: null }),
    row("sup", { relation: { kind: "suppressed-by" } }),
    row("dup", { relation: { kind: "duplicate-of" } }),
    row("gone", { state: "deleted" }),
  ]);
  assert.deepEqual(pins.map((p) => [p.id, p.kind]), [["c", "confirmed"], ["k", "candidate"], ["u", "unknown"]]);
  assert.ok(pins.every((p) => p.source === "detection"));
  // A Confirmed row with no explanation stays a Confirmed square: the stronger claim is not demoted.
  assert.equal(detectionPins([row("cu", { state: "confirmed", family: null, explanations: [] })])[0].kind, "confirmed");
});

test("the interval is the served one: open → t1 null (live edge), closed → the served end", () => {
  const [open] = detectionPins([row("o", { presence: { last_interval: { t_start_s: T0 - 30, t_end_s: T0 - 1, open: true } } })]);
  assert.equal(open.t0Ns, (T0 - 30) * S);
  assert.equal(open.t1Ns, null);
  assert.equal(open.tip.onAir, true);
  const [closed] = detectionPins([row("c")]);
  assert.equal(closed.t1Ns, (T0 - 5) * S);
  assert.equal(closed.tip.onAir, false);
});

test("MapTip: centre (refined / user band win, as served), bandwidth, family suggestion, on-air", () => {
  const [p] = detectionPins([row("a", { refined: { center_hz: 101.0123e6 }, presence: { last_interval: { t_start_s: T0 - 3, t_end_s: T0, open: true } } })]);
  const lines = pinTipLines(p);
  assert.match(lines[0], /^101\.0123 MHz$/);
  assert.match(lines[1], /candidate signal · bw 200\.0 kHz/);
  assert.match(lines[2], /suggests FM broadcast \(a suggestion, not a finding\)/);
  assert.match(lines[3], /^on air since \d\d:\d\d:\d\dZ$/);
  const [ub] = detectionPins([row("b", { user_band: { f_lo: 101.2e6, f_hi: 101.4e6 } })]);
  near(ub.fHz, 101.3e6);
  near(ub.tip.bandwidthHz!, 200e3);
  const [u] = detectionPins([row("u", { family: null, explanations: [] })]);
  assert.ok(pinTipLines(u).includes("no explanation yet"));
  assert.match(pinTipLines(u)[3], /^ended /);
});

test("detection and curated markers are distinguishable in kind, source and accessible name", () => {
  const [d] = detectionPins([row("d", { state: "confirmed" })]);
  const [c] = curatedPins([{ id: "m1", name: "odd burst", f_hz: 101e6, t_s: T0 - 4 }]);
  assert.equal(c.kind, "curated");
  assert.equal(c.source, "curated");
  assert.equal(c.t0Ns, c.t1Ns, "a curated marker is an instant, placed exactly where it was put");
  assert.match(pinLabel(d), /^confirmed signal at 101\.0000 MHz$/);
  assert.match(pinLabel(c), /^odd burst, my marker at 101\.0000 MHz$/);
  const labels = new Set(["confirmed", "candidate", "unknown", "curated"].map((k) => pinLabel({ ...d, kind: k } as Pin).split(" at ")[0]));
  assert.equal(labels.size, 4, "every kind has its own words — state is never hue alone");
});

// ---------------------------------------------------------------------------
// 2: placement in capture time / Hz
// ---------------------------------------------------------------------------

test("placed in capture time/Hz: x at the centre, y inside the interval near its newest edge", () => {
  const [p] = detectionPins([row("a")]); // 101 MHz, [T0-10, T0-5]
  const { placed } = layoutPanePins([p], "p1", BOX, RECT, 500, 1, T0 * S);
  assert.equal(placed.length, 1);
  near(placed[0].x, 500);
  const yTop = 500 * (1 - 15 / 20), yBot = 500 * (1 - 10 / 20); // newest at the top
  assert.ok(placed[0].y > yTop && placed[0].y <= yBot, `y ${placed[0].y} inside [${yTop}, ${yBot}]`);
  near(placed[0].y, yTop + 10);
});

test("a pan moves the pin by exactly the pixels the rows move — no second layout", () => {
  const [p] = detectionPins([row("a")]);
  const a = layoutPanePins([p], "p1", BOX, RECT, 500, 1, T0 * S).placed[0];
  // The pane scrolled 2 s older: every row moves UP by 2 s of a 20 s pane = 50 px, and so does the pin.
  const shifted = { ...BOX, t0Ns: BOX.t0Ns - 2 * S, t1Ns: BOX.t1Ns - 2 * S };
  const b = layoutPanePins([p], "p1", shifted, RECT, 500, 1, T0 * S).placed[0];
  near(b.y - a.y, -500 * (2 / 20));
  const panned = { ...BOX, f0Hz: BOX.f0Hz + 0.5e6, f1Hz: BOX.f1Hz + 0.5e6 };
  near(layoutPanePins([p], "p1", panned, RECT, 500, 1, T0 * S).placed[0].x, 250);
  // Zoomed out 2x in frequency about the same centre: the pin halves its distance from the centre.
  // (T-910: a feature is placed on its BOX, so the fixture's band moves with its centre.)
  const [q] = detectionPins([row("q", { f_center_hz: 101.5e6, f_lo_hz: 101.4e6, f_hi_hz: 101.6e6 })]);
  const wide = { ...BOX, f0Hz: 100e6 - 1e6, f1Hz: 102e6 + 1e6 };
  near(layoutPanePins([q], "p1", BOX, RECT, 500, 1, T0 * S).placed[0].x, 750);
  near(layoutPanePins([q], "p1", wide, RECT, 500, 1, T0 * S).placed[0].x, 625);
});

test("an ongoing signal that began off-screen is pinned on-screen, at the live edge, inside its box", () => {
  const [p] = detectionPins([row("o", { presence: { last_interval: { t_start_s: T0 - 3600, t_end_s: T0 - 3000, open: true } } })]);
  const { placed } = layoutPanePins([p], "p1", BOX, RECT, 500, 1, T0 * S);
  assert.equal(placed.length, 1);
  near(placed[0].y, 10, 1e-9); // the pane's top is the live edge; the pin is inset into the box
});

test("outside the pane in time or frequency → no pin (never clamped onto the border)", () => {
  const pins = detectionPins([
    row("old", { presence: { last_interval: { t_start_s: T0 - 100, t_end_s: T0 - 50, open: false } } }),
    row("hi", { f_center_hz: 103e6, f_lo_hz: 102.9e6, f_hi_hz: 103.1e6 }),
    row("lo", { f_center_hz: 99e6, f_lo_hz: 98.9e6, f_hi_hz: 99.1e6 }),
  ]);
  assert.deepEqual(layoutPanePins(pins, "p1", BOX, RECT, 500, 1, T0 * S).placed, []);
});

test("CSS px: a dpr-2 canvas and a pane off the canvas's bottom are converted as the HUD labels are", () => {
  const [p] = detectionPins([row("a")]);
  // Two stacked panes in a 1000-device-px-high canvas; this one is the UPPER (GL y 500..1000).
  const rect: PaneRect = { x: 200, y: 500, w: 1000, h: 500 };
  const pl = layoutPanePins([p], "p1", BOX, rect, 1000, 2, T0 * S).placed[0];
  near(pl.x, 100 + 250);
  near(pl.y, 0 + 250 * 0.25 + 10);
});

// ---------------------------------------------------------------------------
// 5 + 6: picking, neighbours, the cap
// ---------------------------------------------------------------------------

function scatter(n: number, seed = 1): PlacedPin[] {
  let s = seed;
  const rnd = () => ((s = (s * 1103515245 + 12345) % 2147483648) / 2147483648);
  const kinds = ["confirmed", "candidate", "unknown", "curated"] as const;
  return Array.from({ length: n }, (_, i) => ({
    pin: { ...curatedPins([{ id: `x${i}`, name: "", f_hz: 1, t_s: 1 }])[0], kind: kinds[i % 4] },
    paneId: "p1", x: rnd() * 1200, y: rnd() * 700,
  }));
}

test("the quadtree picks exactly what a brute-force nearest-within-radius does", () => {
  const pts = scatter(PIN_CAP_PER_PANE);
  const idx = new PinIndex(pts);
  for (let i = 0; i < 2000; i++) {
    const x = (i * 37) % 1200, y = (i * 91) % 700;
    let best: PlacedPin | null = null, bd = 12 * 12;
    for (const p of pts) { const d = (p.x - x) ** 2 + (p.y - y) ** 2; if (d < bd || (d === bd && !best)) { best = p; bd = d; } }
    const got = idx.pick(x, y);
    assert.equal(got ? (got.x - x) ** 2 + (got.y - y) ** 2 : null, best ? bd : null, `at (${x}, ${y})`);
  }
  assert.equal(new PinIndex([]).pick(0, 0), null);
  // Coincident pins (a stack) do not recurse forever, and the curated/stronger one wins the tie.
  const stack = Array.from({ length: 50 }, (_, i) => ({ ...pts[i], x: 10, y: 10 }));
  const top = new PinIndex(stack).pick(10, 10)!;
  assert.equal(top.pin.kind, "curated");
});

test("arrow keys: the neighbour on that side, preferring the one level with you", () => {
  const mk = (id: string, x: number, y: number): PlacedPin => ({ pin: { ...curatedPins([{ id, name: id, f_hz: 1, t_s: 1 }])[0] }, paneId: "p1", x, y });
  const c = mk("c", 100, 100), r = mk("r", 150, 100), far = mk("far", 120, 300), l = mk("l", 40, 100), other = { ...mk("o", 101, 100), paneId: "p2" };
  const all = [c, r, far, l, other];
  assert.equal(neighbourOf(all, c, "right"), r);
  assert.equal(neighbourOf(all, c, "left"), l);
  assert.equal(neighbourOf(all, c, "down"), far);
  assert.equal(neighbourOf(all, c, "up"), null);
  assert.equal(neighbourOf(all, l, "left"), null, "and never into another pane");
});

test("the cap: at most 400 per pane, the excess counted, curated and confirmed kept first", () => {
  const rows = Array.from({ length: 500 }, (_, i) => row(`r${i}`, {
    state: i % 50 === 0 ? "confirmed" : "candidate", f_center_hz: 100e6 + (i / 500) * 2e6,
  }));
  const pins = [...detectionPins(rows), ...curatedPins([{ id: "m", name: "mine", f_hz: 101e6, t_s: T0 - 7 }])];
  const { placed, overCap } = layoutPanePins(pins, "p1", BOX, RECT, 500, 1, T0 * S);
  assert.equal(placed.length, PIN_CAP_PER_PANE);
  assert.equal(overCap, 101);
  assert.ok(placed.some((p) => p.pin.kind === "curated"));
  assert.equal(placed.filter((p) => p.pin.kind === "confirmed").length, 10);
});

test("measurement (docs/24 §14.4): layout + index + hit-test cost at the cap, per pane", () => {
  const rows = Array.from({ length: PIN_CAP_PER_PANE }, (_, i) => row(`r${i}`, {
    f_center_hz: 100e6 + (i / PIN_CAP_PER_PANE) * 2e6,
    presence: { last_interval: { t_start_s: T0 - 20 + (i % 20), t_end_s: T0 - 19 + (i % 20), open: i % 3 === 0 } },
  }));
  const frames = 300;
  const t0 = performance.now();
  let n = 0;
  for (let f = 0; f < frames; f++) {
    const pins = detectionPins(rows);
    const { placed } = layoutPanePins(pins, "p1", BOX, RECT, 500, 1, T0 * S);
    const idx = new PinIndex(placed);
    const at = placed[(f * 7) % placed.length];
    if (idx.pick(at.x + 3, at.y - 3)) n++;
  }
  const perFrameMs = (performance.now() - t0) / frames;
  // Printed, not asserted: a latency bound belongs to the `timing` tier (docs/10 §3.6). The ticket
  // records this number against §14.4's 2 ms-per-pane GPU-picking trigger.
  console.log(`# MAP-09 pins at the cap (${PIN_CAP_PER_PANE}): derive+layout+index+pick ${perFrameMs.toFixed(3)} ms/frame/pane (${n} hits)`);
  assert.ok(Number.isFinite(perFrameMs));
});

// ---------------------------------------------------------------------------
// The DOM layer: states, keyboard, pooling
// ---------------------------------------------------------------------------

/** Just enough DOM for `PinLayer`: buttons with attributes, listeners, focus and a pooled parent. */
class FakeEl {
  children: FakeEl[] = [];
  hidden = false;
  className = "";
  type = "";
  textContent: string | null = "";
  style: Record<string, string> = {};
  dataset: Record<string, string> = {};
  attrs: Record<string, string> = {};
  removed = false;
  listeners: Record<string, ((e: unknown) => void)[]> = {};
  static focused: FakeEl | null = null;
  ownerDocument = { createElement: () => new FakeEl() };
  appendChild(c: FakeEl) { const i = this.children.indexOf(c); if (i >= 0) this.children.splice(i, 1); this.children.push(c); return c; }
  remove() { this.removed = true; }
  setAttribute(k: string, v: string) { this.attrs[k] = v; }
  getAttribute(k: string) { return this.attrs[k] ?? null; }
  addEventListener(t: string, f: (e: unknown) => void) { (this.listeners[t] ??= []).push(f); }
  fire(t: string, e: unknown = {}) { for (const f of this.listeners[t] ?? []) f(e); }
  focus() { FakeEl.focused?.fire("blur"); FakeEl.focused = this; this.fire("focus"); }
}

function laidOut(ids: [string, string, number, number][]) {
  const pins = ids.map(([id, kind]) => ({ ...detectionPins([row(id, kind === "confirmed" ? { state: "confirmed" } : {})])[0] }));
  return [{ placed: pins.map((pin, i) => ({ pin, paneId: "p1", x: ids[i][2], y: ids[i][3] })), overCap: 0 }];
}

test("rest / hover / selected: classes, aria, per-frame transform; pooled elements keep identity", () => {
  const root = new FakeEl();
  const layer = new PinLayer(root as unknown as HTMLElement);
  layer.update(laidOut([["a", "confirmed", 100, 50], ["b", "candidate", 300, 80]]), null, null);
  const a = layer.element("p1", "a") as unknown as FakeEl;
  const b = layer.element("p1", "b") as unknown as FakeEl;
  // T-910: a detection with no box area is a generalized feature — `symbol`, no DOM glyph.
  assert.equal(a.className, "sf-pin confirmed detection symbol");
  assert.equal(b.className, "sf-pin candidate detection symbol");
  assert.equal(a.type, "button", "a real button: focusable, Enter/Space activate");
  assert.match(a.getAttribute("aria-label")!, /^confirmed signal at /);
  assert.equal(a.getAttribute("aria-pressed"), "false");
  assert.equal(a.style.transform, "translate(100.0px, 50.0px)");
  // Hover b, select a; the next frame moves a — the same element, restyled.
  layer.update(laidOut([["a", "confirmed", 120, 60], ["b", "candidate", 300, 80]]), "b", "a");
  assert.equal(layer.element("p1", "a") as unknown as FakeEl, a, "pooled, so focus survives a frame");
  assert.equal(a.className, "sf-pin confirmed detection symbol selected");
  assert.equal(a.getAttribute("aria-pressed"), "true");
  assert.equal(b.className, "sf-pin candidate detection symbol hovered");
  assert.equal(a.style.transform, "translate(120.0px, 60.0px)");
  // Gone from the window → the element goes.
  layer.update(laidOut([["b", "candidate", 300, 80]]), null, null);
  assert.ok(a.removed);
  assert.equal(layer.pick(300, 85)?.pin.id, "b", "the pointer's pick is the quadtree over this frame");
  assert.equal(layer.pick(100, 50), null);
});

test("keyboard: focus → MapTip hook, Enter/click → select hook, arrows → neighbour focus", () => {
  const root = new FakeEl();
  const focused: (string | null)[] = [];
  const selected: string[] = [];
  const layer = new PinLayer(root as unknown as HTMLElement, {
    onFocus: (p) => focused.push(p ? p.pin.id : null),
    onSelect: (p) => selected.push(p.pin.id),
  });
  layer.update(laidOut([["a", "candidate", 100, 100], ["b", "candidate", 200, 100], ["c", "candidate", 100, 300]]), null, null);
  const a = layer.element("p1", "a") as unknown as FakeEl;
  a.focus();
  assert.deepEqual(focused, ["a"]);
  let prevented = false;
  a.fire("keydown", { key: "ArrowRight", preventDefault: () => { prevented = true; } });
  assert.ok(prevented);
  assert.equal(FakeEl.focused, layer.element("p1", "b") as unknown as FakeEl);
  assert.deepEqual(focused, ["a", null, "b"]);
  (layer.element("p1", "b") as unknown as FakeEl).fire("click");
  assert.deepEqual(selected, ["b"]);
});

test("over the cap: the excess is stated in words, not silently dropped", () => {
  const root = new FakeEl();
  const layer = new PinLayer(root as unknown as HTMLElement);
  layer.update([{ placed: [], overCap: 7 }], null, null);
  const over = root.children[0];
  assert.equal(over.hidden, false);
  assert.match(over.textContent!, /^7 more markers in view than can be pinned — zoom in/);
  layer.update([{ placed: [], overCap: 0 }], null, null);
  assert.equal(over.hidden, true);
});

// ---------------------------------------------------------------------------
// 7: in the frame
// ---------------------------------------------------------------------------

const LAT: Lattice = { scheme: "view", cells: 256, f0Hz: 6250, t0Ns: 1e9, levelsF: 20, levelsT: 15 };
const BOUNDS = { f0Hz: 1e6, f1Hz: 6e9, t0Ns: (T0 - 86_400) * S, t1Ns: T0 * S };
function tile(a: TileAddr): TileData {
  return {
    addr: a, key: keyOf(a), nf: 2, nt: 2,
    value: new Float32Array([-90, -80, -70, -60]),
    state: new Uint8Array([CELL.OBSERVED, CELL.OBSERVED, CELL.UNOBSERVED, CELL.OBSERVED]),
    tier: "survey-overview", answeredLevel: 1, fold: { frequency: "exact", time: "exact" },
    rangeDb: { lo: -100, hi: -60 }, bytes: 192 * 1024, serverInFlightLimit: null, serverInFlightShare: null,
  };
}

test("the dom hook runs once per frame with the very pane views the data pass drew (not the minimap)", () => {
  const g = stubGl(1200, 600);
  const got: { panes: readonly PaneView[]; edge: number; h: number }[] = [];
  const view = new SurfaceView({
    canvas: g.canvas, lattice: LAT, bounds: BOUNDS, minimapPx: 150,
    cache: (tex) => new TileCache(tex, (a) => Promise.resolve(tile(a)), { inFlight: 64, now: () => 0 }),
    surface: { pinParents: false },
    freq: { centerHz: 101e6, spanHz: 2e6 }, spanNs: 20 * S,
    dom: (panes, edge, h) => { got.push({ panes, edge, h }); },
  });
  const f = view.frame(T0 * S, []);
  assert.equal(got.length, 1);
  assert.equal(got[0].edge, T0 * S);
  assert.equal(got[0].h, 600);
  const paneViews = f.views.filter((v) => got[0].panes.some((p) => p.id === v.id));
  assert.equal(got[0].panes.length, view.panes.count, "panes only — the minimap is not a place to pin");
  for (const p of got[0].panes) assert.deepEqual(p, paneViews.find((v) => v.id === p.id));
  view.frame(T0 * S + S, []);
  assert.equal(got.length, 2, "every frame, never a poll");
  view.dispose();
});

// ---------------------------------------------------------------------------
// 8: thin client
// ---------------------------------------------------------------------------

test("thin client: hover, focus and select reach no route; the module names no route or client", () => {
  const fetched: unknown[] = [];
  const g = globalThis as { fetch?: unknown };
  const real = g.fetch;
  g.fetch = (...a: unknown[]) => { fetched.push(a); return Promise.reject(new Error("a pin reached the network")); };
  try {
    const root = new FakeEl();
    const layer = new PinLayer(root as unknown as HTMLElement, { onFocus: () => {}, onSelect: () => {} });
    const lo = laidOut([["a", "candidate", 10, 10], ["b", "confirmed", 50, 10]]);
    for (let i = 0; i < 5; i++) layer.update(lo, i % 2 ? "a" : "b", "a");
    const a = layer.element("p1", "a") as unknown as FakeEl;
    a.focus();
    a.fire("keydown", { key: "ArrowRight", preventDefault: () => {} });
    a.fire("click");
    layer.pick(10, 10);
    layer.dispose();
  } finally { if (real) g.fetch = real; else delete g.fetch; }
  assert.deepEqual(fetched, []);
  const src = readFileSync("src/surface/pins.ts", "utf8").replace(/\/\/.*$/gm, "").replace(/\/\*[\s\S]*?\*\//g, "");
  for (const banned of ["fetch(", "/api/", "client.", "DeviceAction", "Date.now", "performance.now"]) {
    assert.ok(!src.includes(banned), `pins.ts reaches for ${banned}`);
  }
});

// ---------------------------------------------------------------------------
// 9: GIS generalization (user decision 2026-09-24): a signal is a RECTANGLE. The box is the
//    representation; the glyph exists only when the box is < 6 px on screen in either axis.
// ---------------------------------------------------------------------------

test("a box >= 6 px in both axes draws NO glyph: its button is an invisible area over the box", () => {
  // 200 kHz of a 2 MHz / 1000 px pane = 100 px wide; 5 s of a 20 s / 500 px pane = 125 px tall.
  const [p] = detectionPins([row("big")]);
  const { placed } = layoutPanePins([p], "p1", BOX, RECT, 500, 1, T0 * S);
  assert.ok(placed[0].area, "a readable box is not generalized");
  near(placed[0].area!.x0, 450); near(placed[0].area!.x1, 550);
  near(placed[0].area!.y0, 500 * (1 - 15 / 20)); near(placed[0].area!.y1, 500 * (1 - 10 / 20));
  const root = new FakeEl();
  const layer = new PinLayer(root as unknown as HTMLElement);
  layer.update([{ placed, overCap: 0 }], null, null);
  const el = layer.element("p1", "big") as unknown as FakeEl;
  assert.equal(el.className, "sf-pin candidate detection area", "the `area` class suppresses the glyph");
  assert.equal(el.style.transform, "translate(450.0px, 125.0px)");
  assert.equal(el.style.width, "100.0px");
  assert.equal(el.style.height, "125.0px");
  assert.match(el.getAttribute("aria-label")!, /^candidate signal at /, "still focusable with its name");
  const css = readFileSync("src/app/centre/centre.css", "utf8");
  assert.match(css, /\.sf-pin\.area::before, \.sf-pin\.area::after, \.sf-pin\.symbol::before, \.sf-pin\.symbol::after \{ content: none; display: none; \}/);
});

test("T-910: a box < 6 px in BOTH axes generalizes to its symbol; small in ONE axis is a bar at its true extent", () => {
  // 5 kHz of a 2 MHz / 1000 px pane = 2.5 px wide, but 5 s tall (125 px): a thin bar of its
  // duration, hit through a padded area — never collapsed to a dot (docs/23 §10.6 rule 6).
  const [narrow] = detectionPins([row("n", { f_lo_hz: 100.9975e6, f_hi_hz: 101.0025e6, bandwidth_hz: 5e3 })]);
  // 0.1 s of a 20 s / 500 px pane = 2.5 px tall, 100 px wide: a single-frame impulse is "a thin
  // bar of its measured bandwidth".
  const [brief] = detectionPins([row("b", { presence: { last_interval: { t_start_s: T0 - 10, t_end_s: T0 - 9.9, open: false } } })]);
  // Both: 2.5 px × 2.5 px → the symbol.
  const [dot] = detectionPins([row("d", { f_lo_hz: 100.9975e6, f_hi_hz: 101.0025e6, bandwidth_hz: 5e3,
    presence: { last_interval: { t_start_s: T0 - 10, t_end_s: T0 - 9.9, open: false } } })]);
  const { placed } = layoutPanePins([narrow, brief, dot], "p1", BOX, RECT, 500, 1, T0 * S);
  const by = (id: string) => placed.find((q) => q.pin.id === id)!;
  assert.ok(by("n").area && by("b").area, "one small axis is a bar, not a symbol");
  near(by("n").area!.x1 - by("n").area!.x0, 12); // padded to MIN_HIT_CSS_PX about its centre line
  near((by("n").area!.x0 + by("n").area!.x1) / 2, 500);
  near(by("b").area!.x1 - by("b").area!.x0, 100); // the full measured bandwidth
  assert.equal(by("d").area, null, "both small → generalized");
  // A curated marker is a point: always the glyph.
  const [c] = curatedPins([{ id: "m", name: "mine", f_hz: 101e6, t_s: T0 - 5 }]);
  assert.equal(layoutPanePins([c], "p1", BOX, RECT, 500, 1, T0 * S).placed[0].area, null);
  const root = new FakeEl();
  const layer = new PinLayer(root as unknown as HTMLElement);
  layer.update([{ placed, overCap: 0 }], null, null);
  const el = layer.element("p1", "d") as unknown as FakeEl;
  assert.equal(el.className, "sf-pin candidate detection symbol", "no DOM glyph: the overlay draws the symbol");
  near(by("d").x, 500);
  assert.equal(el.style.transform, `translate(500.0px, ${by("d").y.toFixed(1)}px)`);
  // Zooming in until the same feature is readable turns it back into its box.
  const zoomed = { ...BOX, f0Hz: 100.99e6, f1Hz: 101.01e6, t0Ns: (T0 - 10.5) * S, t1Ns: (T0 - 9.5) * S };
  assert.ok(layoutPanePins([dot], "p1", zoomed, RECT, 500, 1, T0 * S).placed[0].area);
});

test("hover/click anywhere inside a large box hits the feature, far from its centre", () => {
  const [p] = detectionPins([row("big")]); // box x 450..550, y 125..250; centre glyph point (500, 135)
  const root = new FakeEl();
  const layer = new PinLayer(root as unknown as HTMLElement);
  layer.update([layoutPanePins([p], "p1", BOX, RECT, 500, 1, T0 * S)], null, null);
  assert.equal(layer.pick(455, 245)?.pin.id, "big", "bottom-left corner, 60 px from the centre point");
  assert.equal(layer.pick(545, 200)?.pin.id, "big");
  assert.equal(layer.pick(440, 200), null, "outside the box: nothing");
  assert.equal(layer.pick(500, 260), null);
  // A generalized glyph inside a big box is the more specific target.
  const [small] = detectionPins([row("s", { f_center_hz: 101.03e6, f_lo_hz: 101.0299e6, f_hi_hz: 101.0301e6 })]);
  layer.update([layoutPanePins([p, small], "p1", BOX, RECT, 500, 1, T0 * S)], null, null);
  const s = layer.pins.find((q) => q.pin.id === "s")!;
  assert.equal(layer.pick(s.x, s.y)?.pin.id, "s");
  assert.equal(layer.pick(460, 240)?.pin.id, "big");
});
