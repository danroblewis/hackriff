// T-805 (MAP-05): HUD axes — a frequency ruler along each pane's bottom and a time ruler down its
// left, anchored in content space. The claims, each one of docs/23 §9 / §10.1:
//
//  1. **Anchored in (Hz, capture time), not screen space.** A tick's value is fixed in absolute Hz /
//     ns and its position goes through the pane's box, so a pan moves a tick with the rows, and the
//     live edge advancing under a following pane scrolls the time ticks exactly as it scrolls rows.
//  2. **Placed by the data pass's own mapping** — `toClip` — so a tick and the energy under it
//     cannot disagree (T-388's drift family).
//  3. **Honesty floors the step at the drawn cell**, for minor marks too; a pane with no room for a
//     mark draws none.
//  4. **Labels state units** (MHz; an offset behind the live edge with the UTC instant beside it).
//  5. **Wired into the frame**: one ruler per pane (never the minimap), built from the frame's own
//     `PaneStatus` cell, ticks drawn by the overlay program AFTER the data pass (which stays
//     byte-identical with the HUD on or off), labels placed in the same frame, fading with the chrome.
//  6. **Thin client**: nothing here can reach a route.

import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { CELL } from "../src/surface/cellrule";
import {
  HUD_TICK, HudAxes, fmtRulerAge, fmtRulerHz, hudLabels, hudTickQuads, paneRuler, timeLabelReserved,
} from "../src/surface/hud";
import { keyOf, type Lattice, type TileAddr } from "../src/surface/lattice";
import { toClip, type PaneRect } from "../src/surface/surface";
import { TileCache } from "../src/surface/tilecache";
import type { TileData } from "../src/surface/tile";
import { SurfaceView } from "../src/surface/view";
import { stubGl } from "./surface-glstub";

const S = 1e9;
const T0 = 1_700_000_000 * S;
const BOX = { f0Hz: 99.6e6, f1Hz: 102e6, t0Ns: T0 - 20 * S, t1Ns: T0 };
const RECT: PaneRect = { x: 0, y: 0, w: 1000, h: 500 };
const near = (a: number, b: number, eps = 1e-6) => assert.ok(Math.abs(a - b) <= eps, `${a} ≉ ${b}`);

test("frequency ticks sit at nice Hz multiples, each placed where toClip puts that Hz", () => {
  const r = paneRuler("p", BOX, RECT, 1e3, 0.01, T0);
  assert.ok(r.freq.length >= 3, "a 2.4 MHz pane over 1000 px has marks to offer");
  assert.equal(r.freqStepHz, 500e3, "≥96 CSS px per label over 2.4 MHz/1000 px → a 0.5 MHz step");
  for (const t of r.freq) {
    const [cx] = toClip({ ...BOX, f0Hz: t.value, f1Hz: t.value }, BOX);
    near(t.pos, ((cx + 1) / 2) * RECT.w, 1e-6);
    assert.ok(t.value >= BOX.f0Hz && t.value <= BOX.f1Hz);
  }
  assert.deepEqual(r.freq.filter((t) => t.major).map((t) => t.value), [100e6, 100.5e6, 101e6, 101.5e6, 102e6]);
});

test("a pan moves a frequency tick with the content: same Hz, shifted by exactly the pan in px", () => {
  const a = paneRuler("p", BOX, RECT, 1e3, 0.01, T0);
  const pan = 0.24e6; // 100 px at 2.4 kHz/px
  const b = paneRuler("p", { ...BOX, f0Hz: BOX.f0Hz + pan, f1Hz: BOX.f1Hz + pan }, RECT, 1e3, 0.01, T0);
  const ta = a.freq.find((t) => t.value === 101e6)!, tb = b.freq.find((t) => t.value === 101e6)!;
  near(ta.pos - tb.pos, 100, 1e-6);
  assert.equal(ta.label, tb.label, "and it still says the same frequency — the label belongs to the Hz, not the pixel");
});

test("the live edge advancing scrolls a time tick down exactly as it scrolls the rows", () => {
  const a = paneRuler("p", BOX, RECT, 1e3, 0.01, T0);
  const adv = 2 * S; // 50 px at 20 s / 500 px
  const b = paneRuler("p", { ...BOX, t0Ns: BOX.t0Ns + adv, t1Ns: BOX.t1Ns + adv }, RECT, 1e3, 0.01, T0 + adv);
  const v = T0 - 10 * S;
  assert.ok(a.time.some((t) => t.major && t.value === v), "a labelled mark at −10 s");
  const ta = a.time.find((t) => t.value === v)!, tb = b.time.find((t) => t.value === v)!;
  near(tb.pos - ta.pos, 50, 1e-6);
  // Placed through toClip: pos from the TOP, the newest instant is the top row.
  const [, cy] = toClip({ ...BOX, t0Ns: v, t1Ns: v }, BOX);
  near(ta.pos, ((1 - cy) / 2) * RECT.h, 1e-6);
  assert.equal(ta.sub, tb.sub, "the UTC instant beside a tick is the tick's own, not the screen row's");
});

test("honesty: the major step and every minor mark are ≥ the cell the pane was drawn at", () => {
  // A 2.4 MHz pane drawn at 200 kHz cells: 96 px/label would want 0.5 MHz, whose minor 100 kHz is
  // finer than the cell — so no minor marks at all, and the major step stays ≥ the cell.
  const r = paneRuler("p", BOX, RECT, 200e3, 0.01, T0);
  assert.ok(r.freqStepHz >= 200e3);
  assert.ok(r.freq.every((t) => t.major), "a minor mark finer than the drawn cell would claim resolution never captured");
  // A cell coarser than the whole span leaves nothing honest to mark between the edges.
  const coarse = paneRuler("p", BOX, RECT, 10e6, 30, T0);
  assert.ok(coarse.freqStepHz >= 10e6 && coarse.timeStepNs >= 30 * S);
  assert.ok(coarse.freq.length <= 1 && coarse.time.length <= 1);
  // Degenerate inputs draw nothing rather than guess.
  const none = paneRuler("p", BOX, RECT, 0, 0, T0);
  assert.deepEqual([none.freq.length, none.time.length], [0, 0]);
  assert.deepEqual(hudTickQuads(paneRuler("p", BOX, { x: 0, y: 0, w: 0, h: 0 }, 1e3, 0.01, T0)), []);
});

test("labels state units: MHz on the frequency ruler, an age behind the live edge + UTC on the time ruler", () => {
  const r = paneRuler("p", BOX, RECT, 1e3, 0.01, T0);
  for (const t of r.freq.filter((x) => x.major)) assert.match(t.label!, /^\d+\.\d MHz$/);
  const majors = r.time.filter((x) => x.major);
  assert.ok(majors.some((t) => t.label === "live edge"), "the mark on the live edge says so");
  for (const t of majors) {
    assert.match(t.label!, /^(live edge|−\d.*(ms|s|h))$/);
    assert.match(t.sub!, /^\d\d:\d\d:\d\d(\.\d{3})?Z$/);
  }
  assert.equal(fmtRulerHz(433.92e6, 10e3), "433.92 MHz");
  assert.equal(fmtRulerAge(T0 - 12 * S, T0, S), "−12 s");
  assert.equal(fmtRulerAge(T0 + 3 * S, T0, S), "+3.0 s", "a frozen window past the edge is never labelled as history");
});

test("ticks are strokes from the pane's bottom (frequency) and left (time) edges, never a wash", () => {
  const r = paneRuler("p", BOX, RECT, 1e3, 0.01, T0);
  const q = hudTickQuads(r, { majorPx: 10, minorPx: 5, thickPx: 1, alpha: 0.5 });
  assert.equal(q.length, r.freq.length + r.time.length);
  for (const x of q) {
    assert.equal(x.kind, "hud-tick");
    const wPx = ((x.clip[2] - x.clip[0]) / 2) * RECT.w, hPx = ((x.clip[3] - x.clip[1]) / 2) * RECT.h;
    assert.ok(wPx <= 10 + 1e-9 && hPx <= 10 + 1e-9, `a tick is a few px, not an area (${wPx}×${hPx})`);
    assert.ok(x.clip[1] === -1 || x.clip[0] === -1, "anchored to the bottom or the left edge");
    assert.ok(x.rgba[3] <= HUD_TICK[3] * 0.5 + 1e-9, "the fade multiplies the ink");
  }
});

test("labels land on their ticks in CSS px (GL rect → DOM, device px → CSS) and clear the corner", () => {
  const rect: PaneRect = { x: 0, y: 200, w: 2000, h: 1000 }; // above a 200 px map, dpr 2
  const r = paneRuler("p", BOX, rect, 1e3, 0.01, T0, 2);
  const ls = hudLabels(r, 1200, 2);
  const f = ls.filter((l) => l.axis === "freq"), t = ls.filter((l) => l.axis === "time");
  assert.ok(f.length > 0 && t.length > 0);
  for (const l of f) {
    const tick = r.freq.find((x) => x.value === l.value)!;
    near(l.x, tick.pos / 2);
    near(l.y, (1200 - 1200) / 2 + 500, 1e-9); // the pane's bottom edge, in CSS px from the canvas top
    assert.ok(l.x >= 64, "no frequency label under the time ruler's corner");
  }
  for (const l of t) near(l.y, r.time.find((x) => x.value === l.value)!.pos / 2);
});

// T-997: the map's floating chrome is docked down the SAME left edge the time ruler runs down
// (Go-to, the nudge row, the inventory pills, the retune offer). A label under a control is a label
// lost — exactly the complaint the user made of the T-895 chip at mid-height — so a label that would
// print into the chrome's box is DROPPED, never moved off the instant it names.
test("a time label that would print under the floating chrome is dropped, never moved", () => {
  const rect: PaneRect = { x: 0, y: 0, w: 2000, h: 1600 }; // dpr 2 → 1000 x 800 CSS
  const r = paneRuler("p", BOX, rect, 1e3, 0.01, T0, 2);
  const all = hudLabels(r, 1600, 2).filter((l) => l.axis === "time");
  assert.ok(all.length > 1, "no time labels at all, so this proves nothing");
  // The left column as `map-controls.css` places it: 8 px in, ~330 px wide, down to the pills' row.
  const reserve = { left: 8, right: 160, bottom: 200 };
  const kept = hudLabels(r, 1600, 2, reserve).filter((l) => l.axis === "time");
  assert.ok(kept.length < all.length, "the reserve dropped nothing — the case is not exercised");
  for (const l of kept) {
    assert.ok(!timeLabelReserved(l.x, l.y, reserve), `a kept label prints into the chrome at y = ${l.y}`);
    // Dropped, never moved: every kept label is still exactly where it was without the reserve.
    const same = all.find((a) => a.value === l.value)!;
    assert.deepEqual([l.x, l.y], [same.x, same.y], "a label was moved off its tick");
  }
  // And the frequency ruler along the bottom is untouched by a top-left reserve.
  assert.equal(hudLabels(r, 1600, 2, reserve).filter((l) => l.axis === "freq").length,
    hudLabels(r, 1600, 2).filter((l) => l.axis === "freq").length);
  // A null reserve is the old behaviour, exactly.
  assert.deepEqual(hudLabels(r, 1600, 2, null), hudLabels(r, 1600, 2));
});

// ---------------------------------------------------------------------------
// In the frame
// ---------------------------------------------------------------------------

const LAT: Lattice = { scheme: "view", cells: 256, f0Hz: 6250, t0Ns: 1e9, levelsF: 20, levelsT: 15 };
const BOUNDS = { f0Hz: 1e6, f1Hz: 6e9, t0Ns: T0 - 86_400 * S, t1Ns: T0 };

function tile(a: TileAddr): TileData {
  return {
    addr: a, key: keyOf(a), nf: 2, nt: 2,
    value: new Float32Array([-90, -80, -70, -60]),
    state: new Uint8Array([CELL.OBSERVED, CELL.OBSERVED, CELL.UNOBSERVED, CELL.OBSERVED]),
    tier: "survey-overview", answeredLevel: 1, fold: { frequency: "exact", time: "exact" },
    rangeDb: { lo: -100, hi: -60 }, bytes: 192 * 1024, serverInFlightLimit: null, serverInFlightShare: null,
  };
}

/** Just enough of a DOM element for `HudAxes`: a pooled child list with text, style and dataset. */
class FakeEl {
  children: FakeEl[] = [];
  hidden = false;
  className = "";
  textContent: string | null = "";
  style: Record<string, string> = {};
  dataset: Record<string, string> = {};
  removed = false;
  ownerDocument = { createElement: () => new FakeEl() };
  appendChild(c: FakeEl) { this.children.push(c); return c; }
  remove() { this.removed = true; }
}

function viewWith(hud: FakeEl | null, alpha = 1) {
  const g = stubGl(1200, 600);
  const view = new SurfaceView({
    canvas: g.canvas, lattice: LAT, bounds: BOUNDS, minimapPx: 150,
    cache: (tex) => new TileCache(tex, (a) => Promise.resolve(tile(a)), { inFlight: 64, now: () => 0 }),
    surface: { pinParents: false },
    freq: { centerHz: 100.8e6, spanHz: 2.4e6 }, spanNs: 20 * S,
    hud: hud as unknown as HTMLElement | null, hudAlpha: () => alpha,
  });
  return { g, view };
}

test("in the frame: one ruler per pane (not the map), at the cell the frame's own status reported", () => {
  const hud = new FakeEl();
  const { view } = viewWith(hud);
  const f = view.frame(T0, []);
  assert.equal(f.rulers.length, view.panes.count, "the minimap is where the panes are, not a ruler");
  for (const r of f.rulers) {
    const s = f.statuses.find((x) => x.id === r.id)!;
    const v = f.views.find((x) => x.id === r.id)!;
    assert.deepEqual(r.rect, v.rect, "the rectangle the data pass drew the pane into");
    assert.ok(r.freqStepHz >= s.cellHz && r.timeStepNs >= s.cellS * S, "floored at the level DRAWN");
  }
  assert.ok(f.hudQuads.length > 0 && f.hudQuads.every((q) => q.kind === "hud-tick"));
  // Labels placed in the same frame, from the same rulers.
  const shown = hud.children.filter((c) => !c.hidden);
  assert.ok(shown.length > 0);
  assert.ok(shown.some((c) => c.className.includes("freq") && / MHz$/.test(c.children[0].textContent!)));
  assert.ok(shown.some((c) => c.className.includes("time")));
  const values = new Set(f.rulers.flatMap((r) => [...r.freq, ...r.time].map((t) => String(t.value))));
  for (const c of shown) assert.ok(values.has(c.dataset.value), "every label is one of this frame's ticks");
  view.dispose();
  assert.ok(hud.children.every((c) => c.removed));
});

test("the next frame re-lays-out the labels: a pan moves them with no poll in between", () => {
  const hud = new FakeEl();
  const { view } = viewWith(hud);
  view.frame(T0, []);
  const id = view.panes.list()[0].id;
  const before = hud.children.filter((c) => !c.hidden && c.className.includes("freq")).map((c) => [c.dataset.value, c.style.transform]);
  const p = view.panes.get(id)!;
  view.panes.setFreq(id, p.freq.centerHz + 0.3e6, p.freq.spanHz);
  view.frame(T0, []);
  const after = new Map(hud.children.filter((c) => !c.hidden && c.className.includes("freq")).map((c) => [c.dataset.value, c.style.transform]));
  const moved = before.filter(([v, tr]) => after.has(v!) && after.get(v!) !== tr);
  assert.ok(moved.length > 0, "a tick that survives the pan is at a new screen position");
});

test("the data pass is byte-identical with the HUD on and off; ticks come after it, in the overlay program", () => {
  const run = (on: boolean) => {
    const { g, view } = viewWith(on ? new FakeEl() : null);
    view.hudAxes = on;
    view.frame(T0, []);
    const data = g.ops.filter((o) => o.kind === "draw" && o.u && "uBox" in o.u);
    const lastData = g.ops.lastIndexOf(data[data.length - 1]);
    const ticks = g.ops.filter((o, i) => i > lastData && o.kind === "draw" && o.u && "uInk" in o.u);
    view.dispose();
    return { data: JSON.stringify(data), ticks: ticks.length };
  };
  const on = run(true), off = run(false);
  assert.equal(on.data, off.data, "a ruler can never tint a measurement");
  assert.ok(on.ticks > off.ticks, "the ticks are overlay-program draws after the data pass");
});

test("the ticks fade with the chrome: the host's alpha multiplies their ink every frame", () => {
  const { view } = viewWith(null, 0.45);
  view.hudAxes = true;
  const f = view.frame(T0, []);
  assert.ok(f.hudQuads.length > 0);
  for (const q of f.hudQuads) assert.ok(q.rgba[3] <= HUD_TICK[3] * 0.45 + 1e-9);
  view.dispose();
});

test("thin client: the HUD module reaches no route and holds no signal logic", () => {
  const src = readFileSync("src/surface/hud.ts", "utf8");
  assert.doesNotMatch(src, /fetch\(|\/api\/|client\.|DeviceAction/);
  const host = readFileSync("src/app/centre/surface.ts", "utf8");
  assert.match(host, /hud: hudEl/, "the app mounts the label layer");
  assert.match(host, /chrome-idle/, "and fades it on the chrome's idle class");
});
