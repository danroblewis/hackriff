// T-910 (MMAP, docs/23 §10.6 rule 6): GIS feature symbolization — signals as (t, f) polygons with
// class symbology, scale-dependent generalization below ~6 px, hit-test/select on the polygon, and
// labels by placement rules. The user's words (2026-09-24): "A signal has a frequency width and a
// duration, that's a rectangle … think of this part as a type of GIS software instead of just
// Google Maps." Each claim below is stated against the degenerate implementation that would pass
// without it:
//
//  1. **True extent, one mapping.** The polygon's outline is exactly `toClip` of its (t, f) box —
//     the function the tiles are placed by — and under scroll and zoom it moves by exactly what the
//     rows move; the DOM hit area over it (band 1) is the SAME rectangle in CSS px, on the same
//     frame. A drift between the two layers is an invariant violation, not a cosmetic bug.
//  2. **Generalization, both ways, one predicate.** Under 6 CSS px in both axes the overlay draws a
//     symbol and the pin layer has no area; at 6 px it is the box again in both; zooming out and back
//     in crosses the threshold each way. One small axis is a thin bar at its true extent on the
//     other (a single-frame impulse keeps its measured bandwidth).
//  3. **Symbology per class, on the polygon**: Confirmed solid + hatch fill, Candidate dashed and
//     unfilled, unexplained plain outline (+ '?' label), artifact thin grey, curated double. Every
//     quad is still a stroke: a patterned quad inks at most its pattern's fraction.
//  4. **Identify = hit-test the polygon**: inside anywhere hits; outside does not; a wide box whose
//     CENTRE frequency is off the pane is still hit where it overlaps the pane (the T-909 note).
//     Selection = heavier outline + corner handles.
//  5. **Keyboard**: the buttons are in reading order (newest time first, then frequency).
//  6. **Labels**: inside a box that fits, above one wide enough, '?' for unexplained, thinned by
//     priority Confirmed > Candidate when they collide.
//  7. **The pass**: a patterned quad reaches the GPU as `uPat`; the shader discards off-pattern
//     fragments and still has no sampler, ramp or cell state.

import { test } from "node:test";
import assert from "node:assert/strict";
import {
  ARTIFACT_MARK, CONFIRMED_MARK, OPEN_EDGE_MARK, GENERALIZE_BELOW_CSS_PX, HANDLE_LEN_CSS_PX, SYMBOLOGY,
  markQuads, signalMarkBoxes, type MarkBox,
} from "../src/surface/marks";
import { inkFraction, quadSizePx, type OverlayQuad } from "../src/surface/minimap";
import {
  PinLayer, detectionPins, isUnexplained, labelText, layoutPanePins, placeLabels, readingOrder,
  type PinRow, type PlacedPin,
} from "../src/surface/pins";
import { OverlayPass } from "../src/surface/overlay";
import { toClip, type PaneRect } from "../src/surface/surface";
import { stubGl } from "./surface-glstub";

const S = 1e9;
const T0 = 1_700_000_000; // capture-clock seconds; the live edge
const EDGE = T0 * S;
/** 2 MHz over 20 s, ending at the edge, in a 1000 x 500 device-px pane of a 500 px canvas (dpr 1). */
const PANE = { f0Hz: 100e6, f1Hz: 102e6, t0Ns: (T0 - 20) * S, t1Ns: EDGE };
const RECT: PaneRect = { x: 0, y: 0, w: 1000, h: 500 };
const CANVAS_H = 500;
const GEN = { dpr: 1, generalizeBelowPx: GENERALIZE_BELOW_CSS_PX };
const near = (a: number, b: number, eps = 1e-6) => assert.ok(Math.abs(a - b) <= eps, `${a} ≉ ${b}`);

function row(id: string, over: Partial<PinRow> = {}): PinRow {
  return {
    id, state: "candidate", f_center_hz: 101e6, bandwidth_hz: 200e3, f_lo_hz: 100.9e6, f_hi_hz: 101.1e6,
    family: "FM broadcast", explanations: [{ label: "FM broadcast" }],
    presence: { last_interval: { t_start_s: T0 - 10, t_end_s: T0 - 5, open: false } },
    ...over,
  };
}

const boxesOf = (rows: PinRow[], focus: string | null = null) => signalMarkBoxes(rows, focus, isUnexplained);
const quadsOf = (rows: PinRow[], pane = PANE, focus: string | null = null) => markQuads(boxesOf(rows, focus), EDGE, pane, RECT, GEN);

/** A clip-space x/y as CSS px from the canvas's top-left (dpr 1, GL y-up rect). */
const cssX = (cx: number, r = RECT) => r.x + ((cx + 1) / 2) * r.w;
const cssY = (cy: number, r = RECT) => CANVAS_H - (r.y + ((cy + 1) / 2) * r.h);

/** The outer extent of a feature's EDGE quads (its outline), in clip space. */
function outline(quads: readonly OverlayQuad[], id: string) {
  const e = quads.filter((q) => q.id === id && q.part === "edge");
  assert.ok(e.length > 0, `no outline for ${id}`);
  return {
    x0: Math.min(...e.map((q) => q.clip[0])), y0: Math.min(...e.map((q) => q.clip[1])),
    x1: Math.max(...e.map((q) => q.clip[2])), y1: Math.max(...e.map((q) => q.clip[3])),
  };
}

// ---------------------------------------------------------------------------
// 1. True extent, one mapping, both layers — under scroll and zoom
// ---------------------------------------------------------------------------

test("the polygon is toClip of its (t, f) box, and the DOM hit area is the same rectangle", () => {
  const r = row("k", { state: "confirmed" });
  const want = toClip({ f0Hz: 100.9e6, f1Hz: 101.1e6, t0Ns: (T0 - 10) * S, t1Ns: (T0 - 5) * S }, PANE);
  const o = outline(quadsOf([r]), "k");
  near(o.x0, want[0]); near(o.y0, want[1]); near(o.x1, want[2]); near(o.y1, want[3]);
  const [placed] = layoutPanePins(detectionPins([r]), "p1", PANE, RECT, CANVAS_H, 1, EDGE).placed;
  near(placed.area!.x0, cssX(o.x0)); near(placed.area!.x1, cssX(o.x1));
  near(placed.area!.y0, cssY(o.y1)); near(placed.area!.y1, cssY(o.y0)); // CSS y runs down; newest on top
});

test("under scroll and zoom the polygon moves exactly with the rows, and the hit area with it (no drift)", () => {
  const r = row("k", { state: "confirmed" });
  const src = { f0Hz: 100.9e6, f1Hz: 101.1e6, t0Ns: (T0 - 10) * S, t1Ns: (T0 - 5) * S };
  const panes = [
    PANE,
    { ...PANE, t0Ns: PANE.t0Ns + 3 * S, t1Ns: PANE.t1Ns + 3 * S },       // the live edge advanced 3 s
    { ...PANE, t0Ns: PANE.t0Ns - 7.25 * S, t1Ns: PANE.t1Ns - 7.25 * S }, // scrubbed back
    { ...PANE, f0Hz: 100.5e6, f1Hz: 101.5e6 },                           // zoomed in frequency
    { ...PANE, t0Ns: (T0 - 12) * S, t1Ns: (T0 - 2) * S },                 // zoomed in time
  ];
  for (const pane of panes) {
    // Where the tile rows under the box are placed — clipped to the pane, since a polygon partly off
    // the pane is drawn (and hit) only where it is genuinely on it.
    const want = toClip(src, pane).map((v) => Math.max(-1, Math.min(1, v)));
    const o = outline(markQuads(boxesOf([r]), EDGE + 3 * S, pane, RECT, GEN), "k");
    near(o.x0, want[0]); near(o.x1, want[2]); near(o.y0, want[1]); near(o.y1, want[3]);
    const [p] = layoutPanePins(detectionPins([r]), "p1", pane, RECT, CANVAS_H, 1, EDGE + 3 * S).placed;
    near(p.area!.x0, cssX(want[0])); near(p.area!.x1, cssX(want[2]));
    near(p.area!.y0, cssY(want[3])); near(p.area!.y1, cssY(want[1]));
  }
});

test("an ongoing feature's polygon runs to the live edge by assumption, and grows with it — no poll", () => {
  const r = row("o", { presence: { last_interval: { t_start_s: T0 - 10, t_end_s: T0 - 9, open: true } } });
  const at = (edge: number) => {
    const pane = { ...PANE, t0Ns: edge - 20 * S, t1Ns: edge };
    return { o: outline(markQuads(boxesOf([r]), edge, pane, RECT, GEN), "o"), pane };
  };
  const a = at(EDGE), b = at(EDGE + 2 * S);
  near(a.o.y1, 1); near(b.o.y1, 1); // its top is the live edge in both frames
  // Its start is fixed in capture time, so it slides down by the 2 s the rows slid: 2/20 of 2 clip units.
  near(b.o.y0 - a.o.y0, -0.2);
  // The newest edge says "on air", not "ended".
  const top = markQuads(boxesOf([r]), EDGE, PANE, RECT, GEN).filter((q) => q.part === "edge" && q.clip[3] === 1 && q.clip[2] - q.clip[0] > 0.1);
  assert.equal(top.length, 1);
  assert.deepEqual(top[0].rgba, OPEN_EDGE_MARK);
  assert.equal(top[0].pattern, undefined, "the live edge is solid whatever the class's outline");
});

// ---------------------------------------------------------------------------
// 2. Generalization: both ways, one predicate
// ---------------------------------------------------------------------------

test("generalization threshold both ways: < 6 px in both axes → symbol (overlay) and no area (DOM); ≥ 6 → the box in both", () => {
  // 2 MHz / 1000 px → 2 kHz per px (so ±wPx kHz is wPx px wide); 20 s / 500 px → 40 ms per px.
  const sized = (wPx: number, hPx: number) => row(`w${wPx}h${hPx}`, {
    f_lo_hz: 101e6 - wPx * 1e3, f_hi_hz: 101e6 + wPx * 1e3, bandwidth_hz: 2 * wPx * 1e3,
    presence: { last_interval: { t_start_s: T0 - 10, t_end_s: T0 - 10 + hPx * 0.04, open: false } },
  });
  const cases: [number, number, boolean][] = [[5.9, 5.9, true], [6, 6, false], [5.9, 6, false], [6, 5.9, false], [1, 1, true]];
  for (const [w, h, gen] of cases) {
    const r = sized(w, h);
    const q = quadsOf([r]);
    const [p] = layoutPanePins(detectionPins([r]), "p1", PANE, RECT, CANVAS_H, 1, EDGE).placed;
    assert.equal(q.some((x) => x.part === "symbol"), gen, `${w}×${h}: overlay symbol`);
    assert.equal(q.some((x) => x.part === "edge"), !gen, `${w}×${h}: overlay box`);
    assert.equal(p.area === null, gen, `${w}×${h}: the DOM layer decides alike`);
  }
  // Zoom: the same 4 px × 4 px feature becomes its box at 2x in both axes (8 × 8), and a symbol again
  // on the way back out.
  const r = sized(4, 4);
  const zoomIn = { f0Hz: 100.5e6, f1Hz: 101.5e6, t0Ns: (T0 - 15) * S, t1Ns: (T0 - 5) * S }; // 2x both axes
  assert.ok(quadsOf([r]).some((x) => x.part === "symbol"));
  assert.ok(quadsOf([r], zoomIn).some((x) => x.part === "edge") && !quadsOf([r], zoomIn).some((x) => x.part === "symbol"));
  assert.ok(quadsOf([r]).some((x) => x.part === "symbol"), "and back");
});

test("the symbol sits at the centre of the visible part, in the same symbology, and is small", () => {
  const dot = row("d", { state: "confirmed", f_lo_hz: 100.999e6, f_hi_hz: 101.001e6, presence: { last_interval: { t_start_s: T0 - 10, t_end_s: T0 - 9.95, open: false } } });
  const q = quadsOf([dot]);
  const sym = q.filter((x) => x.part === "symbol");
  const cx = (Math.min(...sym.map((x) => x.clip[0])) + Math.max(...sym.map((x) => x.clip[2]))) / 2;
  const cy = (Math.min(...sym.map((x) => x.clip[1])) + Math.max(...sym.map((x) => x.clip[3]))) / 2;
  const c = toClip({ f0Hz: 100.999e6, f1Hz: 101.001e6, t0Ns: (T0 - 10) * S, t1Ns: (T0 - 9.95) * S }, PANE);
  near(cx, (c[0] + c[2]) / 2); near(cy, (c[1] + c[3]) / 2);
  const wPx = (Math.max(...sym.map((x) => x.clip[2])) - Math.min(...sym.map((x) => x.clip[0]))) / 2 * RECT.w;
  assert.ok(wPx >= 8 && wPx <= 10, `a small symbol, not a box: ${wPx} px`);
  assert.ok(q.some((x) => x.part === "fill"), "a Confirmed symbol keeps the Confirmed fill");
  const [p] = layoutPanePins(detectionPins([dot]), "p1", PANE, RECT, CANVAS_H, 1, EDGE).placed;
  near(p.x, cssX(cx)); near(p.y, cssY(cy)); // hit where drawn
});

test("a single-frame impulse is a thin bar of its measured bandwidth, never a dot", () => {
  const impulse = row("i", { f_lo_hz: 100.6e6, f_hi_hz: 101.4e6, presence: { last_interval: { t_start_s: T0 - 10, t_end_s: T0 - 9.99, open: false } } });
  const q = quadsOf([impulse]);
  assert.equal(q.some((x) => x.part === "symbol"), false);
  const bar = q.filter((x) => x.part === "edge");
  const w = (Math.max(...bar.map((x) => x.clip[2])) - Math.min(...bar.map((x) => x.clip[0]))) / 2 * RECT.w;
  near(w, 400, 1e-6); // 0.8 MHz of a 2 MHz / 1000 px pane
  assert.equal(bar[0].pattern?.mode, "dash-x", "a Candidate bar is dashed along its bandwidth");
});

// ---------------------------------------------------------------------------
// 3. Symbology per class, and still strokes
// ---------------------------------------------------------------------------

test("symbology per class: confirmed solid+fill, candidate dashed, unexplained plain, artifact grey, curated double", () => {
  const rows = [
    row("conf", { state: "confirmed" }),
    row("cand", { f_lo_hz: 100.2e6, f_hi_hz: 100.4e6 }),
    row("unex", { f_lo_hz: 101.4e6, f_hi_hz: 101.6e6, family: null, explanations: [] }),
    row("art", { f_lo_hz: 101.7e6, f_hi_hz: 101.9e6, relation: { kind: "artifact-of" } }),
  ];
  const b = boxesOf(rows);
  assert.deepEqual(b.map((x) => [x.id, x.symbology?.cls]), [["conf", "confirmed"], ["cand", "candidate"], ["unex", "unexplained"], ["art", "artifact"]]);
  const q = markQuads(b, EDGE, PANE, RECT, GEN);
  const of = (id: string) => q.filter((x) => x.id === id);
  // Confirmed: a hatch fill under a solid outline.
  const fill = of("conf").find((x) => x.part === "fill")!;
  assert.equal(fill.pattern?.mode, "hatch");
  assert.ok(fill.rgba[3] < CONFIRMED_MARK[3], "light");
  assert.ok(of("conf").filter((x) => x.part === "edge").every((x) => !x.pattern));
  // Candidate: every outline edge dashed; nothing filled.
  const candEdges = of("cand").filter((x) => x.part === "edge");
  assert.equal(candEdges.length, 4);
  assert.ok(candEdges.every((x) => x.pattern?.mode === "dash-x" || x.pattern?.mode === "dash-y"));
  assert.equal(of("cand").some((x) => x.part === "fill"), false);
  // Unexplained: a plain outline, no fill — its '?' is the label layer's (below).
  assert.ok(of("unex").every((x) => x.part === "edge" && !x.pattern));
  // Artifact: its own neutral ink.
  for (const x of of("art")) assert.deepEqual(x.rgba, ARTIFACT_MARK);
  // Curated: a double outline (eight edges) in its own style.
  const cur: MarkBox = { id: "cur", kind: "research-box", f0Hz: 100.9e6, f1Hz: 101.1e6, t0Ns: (T0 - 10) * S, t1Ns: (T0 - 5) * S, rgba: [1, 1, 1, 1], open: false, strokePx: 1, symbology: SYMBOLOGY.curated };
  assert.equal(markQuads([cur], EDGE, PANE, RECT, GEN).filter((x) => x.part === "edge").length, 8);
  // Class is never glyph shape and never hue alone: outline pattern and fill differ per class.
  const sig = (id: string) => JSON.stringify([of(id).some((x) => x.part === "fill"), of(id).find((x) => x.part === "edge")?.pattern?.mode ?? "solid"]);
  assert.equal(new Set(["conf", "cand", "unex"].map(sig)).size, 3);
});

test("every symbolized quad is a stroke — a patterned one inks only its pattern's fraction, never a wash", () => {
  const rows = [row("conf", { state: "confirmed", f_lo_hz: 100.2e6, f_hi_hz: 101.8e6, presence: { last_interval: { t_start_s: T0 - 19, t_end_s: T0 - 1, open: true } } }), row("cand")];
  for (const focus of [null, "conf", "cand"]) {
    for (const q of markQuads(boxesOf(rows, focus), EDGE, PANE, RECT, GEN)) {
      const { wPx, hPx } = quadSizePx(q, RECT);
      const thin = Math.min(Math.abs(wPx), Math.abs(hPx));
      if (q.pattern?.mode === "hatch") {
        assert.ok(inkFraction(q) <= 1 / 7 + 1e-9, `a fill inks ${inkFraction(q)} of its box`);
      } else {
        assert.ok(thin <= 5 + 1e-6, `${q.part} ${q.id} is ${wPx}×${hPx} px — a wash, not a stroke`);
      }
    }
  }
});

// ---------------------------------------------------------------------------
// 4. Identify = hit-test the polygon; select = heavier outline + handles
// ---------------------------------------------------------------------------

test("hit-test: anywhere inside the polygon hits, outside does not; nested → the smaller one", () => {
  const big = row("big", { state: "confirmed", f_lo_hz: 100.4e6, f_hi_hz: 101.6e6, presence: { last_interval: { t_start_s: T0 - 18, t_end_s: T0 - 2, open: false } } });
  const inner = row("in", { f_lo_hz: 101.2e6, f_hi_hz: 101.4e6 });
  const layer = new PinLayer(new FakeEl() as unknown as HTMLElement);
  layer.update([layoutPanePins(detectionPins([big, inner]), "p1", PANE, RECT, CANVAS_H, 1, EDGE)], null, null);
  // big: x 200..800, y 50..450; inner: x 600..700, y 125..250.
  for (const [x, y] of [[205, 55], [795, 445], [205, 445], [500, 400]]) assert.equal(layer.pick(x, y)?.pin.id, "big", `(${x}, ${y})`);
  assert.equal(layer.pick(650, 200)?.pin.id, "in");
  for (const [x, y] of [[195, 200], [805, 200], [500, 45], [500, 455]]) assert.equal(layer.pick(x, y), null, `(${x}, ${y}) is outside`);
});

test("a wide feature whose CENTRE is off the pane is still hit where its polygon overlaps it (T-909 note)", () => {
  // 99–100.5 MHz: centre 99.75 MHz is left of the 100–102 MHz pane, but 0.5 MHz of it is on it.
  const wide = row("w", { f_center_hz: 99.75e6, f_lo_hz: 99e6, f_hi_hz: 100.5e6, bandwidth_hz: 1.5e6 });
  const { placed } = layoutPanePins(detectionPins([wide]), "p1", PANE, RECT, CANVAS_H, 1, EDGE);
  assert.equal(placed.length, 1, "placed from the clipped polygon, not the centre");
  near(placed[0].area!.x0, 0); near(placed[0].area!.x1, 250);
  const layer = new PinLayer(new FakeEl() as unknown as HTMLElement);
  layer.update([{ placed, overCap: 0 }], null, null);
  assert.equal(layer.pick(10, 200)?.pin.id, "w");
  assert.equal(layer.pick(240, 200)?.pin.id, "w");
  assert.equal(layer.pick(260, 200), null);
  // And the overlay draws the part that is on the pane: its right edge, not a left edge at the border.
  const o = markQuads(boxesOf([wide]), EDGE, PANE, RECT, GEN).filter((q) => q.part === "edge");
  assert.ok(o.every((q) => q.clip[0] >= -1));
  assert.equal(o.filter((q) => q.clip[2] - q.clip[0] < 0.01).length, 1, "only the right vertical edge is genuinely on the pane");
});

test("selected = a heavier outline plus corner handles; deselected loses both", () => {
  const r = row("k", { state: "confirmed" });
  const thick = (q: readonly OverlayQuad[]) => Math.max(...q.filter((x) => x.part === "edge" && x.clip[2] - x.clip[0] < 0.1).map((x) => quadSizePx(x, RECT).wPx));
  const rest = quadsOf([r], PANE, null), sel = quadsOf([r], PANE, "k");
  assert.equal(rest.some((x) => x.part === "handle"), false);
  const handles = sel.filter((x) => x.part === "handle");
  assert.equal(handles.length, 8, "an L at each of the four corners");
  assert.ok(Math.max(...handles.map((h) => Math.max(quadSizePx(h, RECT).wPx, quadSizePx(h, RECT).hPx))) >= HANDLE_LEN_CSS_PX - 1e-6);
  assert.ok(thick(sel) > thick(rest), `selected ${thick(sel)} px > rest ${thick(rest)} px`);
  // A selected generalized symbol has handles too.
  const dot = row("d", { f_lo_hz: 100.999e6, f_hi_hz: 101.001e6, presence: { last_interval: { t_start_s: T0 - 10, t_end_s: T0 - 9.95, open: false } } });
  assert.equal(quadsOf([dot], PANE, "d").filter((x) => x.part === "handle").length, 8);
});

// ---------------------------------------------------------------------------
// 5. Keyboard: reading order
// ---------------------------------------------------------------------------

test("Tab walks features in reading order: newest time first, then frequency — and the DOM keeps up", () => {
  const iv = (a: number, b: number) => ({ last_interval: { t_start_s: T0 - a, t_end_s: T0 - b, open: false } });
  const rows = [
    row("old-left", { f_lo_hz: 100.1e6, f_hi_hz: 100.3e6, presence: iv(15, 12) }),
    row("new-right", { f_lo_hz: 101.5e6, f_hi_hz: 101.7e6, presence: iv(6, 2) }),
    row("new-left", { f_lo_hz: 100.5e6, f_hi_hz: 100.7e6, presence: iv(6, 2) }),
    row("mid", { f_lo_hz: 101.0e6, f_hi_hz: 101.2e6, presence: iv(10, 8) }),
  ];
  const lo = layoutPanePins(detectionPins(rows), "p1", PANE, RECT, CANVAS_H, 1, EDGE);
  assert.deepEqual(readingOrder(lo.placed).map((p) => p.pin.id), ["new-left", "new-right", "mid", "old-left"]);
  const root = new FakeEl();
  const layer = new PinLayer(root as unknown as HTMLElement);
  layer.update([lo], null, null);
  const buttons = () => root.children.filter((c) => c.type === "button").map((c) => c.dataset.pin);
  assert.deepEqual(buttons(), ["new-left", "new-right", "mid", "old-left"], "native Tab order = DOM order");
  // A new feature appears at the top: it is first, and a focused feature keeps its focus through the move.
  const focusedHook: (string | null)[] = [];
  const root2 = new FakeEl();
  const layer2 = new PinLayer(root2 as unknown as HTMLElement, { onFocus: (p) => focusedHook.push(p?.pin.id ?? null) });
  layer2.update([lo], null, null);
  const mid = layer2.element("p1", "mid") as unknown as FakeEl;
  mid.focus();
  const more = layoutPanePins(detectionPins([...rows, row("newest", { f_lo_hz: 101.8e6, f_hi_hz: 101.9e6, presence: iv(1.5, 1) })]), "p1", PANE, RECT, CANVAS_H, 1, EDGE);
  layer2.update([more], null, null);
  const order2 = root2.children.filter((c) => c.type === "button").map((c) => c.dataset.pin);
  assert.deepEqual(order2, ["newest", "new-left", "new-right", "mid", "old-left"]);
  assert.equal(FakeEl.focused, mid, "the focused feature keeps focus across the reorder");
  assert.deepEqual(focusedHook, ["mid"], "and the move reports no spurious blur/focus");
});

// ---------------------------------------------------------------------------
// 6. Labels by placement rules
// ---------------------------------------------------------------------------

test("labels: inside a box that fits, above one wide enough, dropped where neither — freq · bw · class", () => {
  const rows = [
    // 400 px × 125 px: fits inside.
    row("wide", { state: "confirmed", f_lo_hz: 100.2e6, f_hi_hz: 101.0e6, bandwidth_hz: 800e3, f_center_hz: 100.6e6, presence: { last_interval: { t_start_s: T0 - 10, t_end_s: T0 - 5, open: false } } }),
    // 400 px wide but 5 px tall (a burst): above.
    row("flat", { f_lo_hz: 101.1e6, f_hi_hz: 101.9e6, bandwidth_hz: 800e3, f_center_hz: 101.5e6, presence: { last_interval: { t_start_s: T0 - 10, t_end_s: T0 - 9.8, open: false } } }),
    // 20 px wide: too narrow for any label.
    row("thin", { f_lo_hz: 101.7e6, f_hi_hz: 101.74e6, bandwidth_hz: 40e3, f_center_hz: 101.72e6, presence: { last_interval: { t_start_s: T0 - 4, t_end_s: T0 - 1, open: false } } }),
  ];
  const labels = placeLabels([layoutPanePins(detectionPins(rows), "p1", PANE, RECT, CANVAS_H, 1, EDGE)]);
  const by = (id: string) => labels.find((l) => l.pinId === id);
  assert.equal(by("wide")?.where, "inside");
  assert.equal(by("wide")?.text, "100.600 MHz · 800.0 kHz · FM broadcast");
  assert.equal(by("flat")?.where, "above");
  assert.ok(by("flat")!.y + by("flat")!.h <= 245 + 1e-6, "just above the box's top edge (y 245)");
  assert.ok(by("flat")!.y + by("flat")!.h >= 240);
  assert.equal(by("thin"), undefined, "a label that would overrun its box is not drawn");
});

test("labels: an unexplained feature leads with '?', and keeps the '?' where the full label does not fit", () => {
  const [u] = detectionPins([row("u", { family: null, explanations: [] })]);
  assert.match(labelText(u), /^\? 101\.000 MHz · 200\.0 kHz$/);
  const small = row("s", { family: null, explanations: [], f_lo_hz: 101.0e6, f_hi_hz: 101.03e6, bandwidth_hz: 30e3 });
  const labels = placeLabels([layoutPanePins(detectionPins([small]), "p1", PANE, RECT, CANVAS_H, 1, EDGE)]);
  assert.equal(labels.length, 1);
  assert.equal(labels[0].text, "?");
  assert.equal(labels[0].cls, "unexplained");
});

test("labels: colliding labels are thinned by priority — Confirmed over Candidate, whatever the order served", () => {
  // Two brief bursts in the same 250 px band, 7.5 px apart in time (they do not overlap), each too
  // short for a label inside: their "above" labels collide, and only one can be drawn.
  const cand = row("cand", { f_lo_hz: 100.9e6, f_hi_hz: 101.4e6, presence: { last_interval: { t_start_s: T0 - 15, t_end_s: T0 - 14.8, open: false } } });
  const conf = row("conf", { state: "confirmed", f_lo_hz: 100.9e6, f_hi_hz: 101.4e6, presence: { last_interval: { t_start_s: T0 - 14.7, t_end_s: T0 - 14.5, open: false } } });
  for (const order of [[cand, conf], [conf, cand]]) {
    const labels = placeLabels([layoutPanePins(detectionPins(order), "p1", PANE, RECT, CANVAS_H, 1, EDGE)]);
    assert.ok(labels.some((l) => l.pinId === "conf"), "the Confirmed label survives");
    for (const a of labels) for (const b of labels) {
      if (a === b) continue;
      assert.ok(!(a.x < b.x + b.w && b.x < a.x + a.w && a.y < b.y + b.h && b.y < a.y + a.h), "no two labels overlap");
    }
    assert.deepEqual(labels.map((l) => l.pinId), ["conf"], "the Candidate's colliding label is thinned");
  }
  // The selected feature's label outranks everything.
  const sel = placeLabels([layoutPanePins(detectionPins([cand, conf]), "p1", PANE, RECT, CANVAS_H, 1, EDGE)], "cand");
  assert.deepEqual(sel.map((l) => l.pinId), ["cand"]);
});

test("labels are rendered in band 1, per frame, text-only and pooled", () => {
  const root = new FakeEl();
  const layer = new PinLayer(root as unknown as HTMLElement);
  const k = row("k", { state: "confirmed", f_lo_hz: 100.5e6, f_hi_hz: 101.5e6, bandwidth_hz: 1e6 });
  const lo = layoutPanePins(detectionPins([k]), "p1", PANE, RECT, CANVAS_H, 1, EDGE);
  layer.update([lo], null, null);
  const box = root.children.find((c) => c.className === "sf-flabels")!;
  assert.equal(box.getAttribute("aria-hidden"), "true");
  assert.equal(box.children.length, 1);
  const el = box.children[0];
  assert.equal(el.textContent, "101.000 MHz · 1.00 MHz · FM broadcast");
  assert.equal(el.className, "sf-flabel confirmed inside");
  const t0 = el.style.transform;
  const scrolled = { ...PANE, t0Ns: PANE.t0Ns + 2 * S, t1Ns: PANE.t1Ns + 2 * S };
  layer.update([layoutPanePins(detectionPins([k]), "p1", scrolled, RECT, CANVAS_H, 1, EDGE)], null, "k");
  assert.equal(box.children[0], el, "pooled");
  assert.notEqual(el.style.transform, t0, "re-placed on the frame the rows moved");
  assert.equal(el.className, "sf-flabel confirmed inside selected");
});

// ---------------------------------------------------------------------------
// 7. The pass: a pattern reaches the GPU, and the program still cannot draw a measurement
// ---------------------------------------------------------------------------

test("the overlay pass submits the pattern as uPat and discards off-pattern fragments; no sampler, no ramp", () => {
  const g = stubGl(1000, 500);
  const pass = new OverlayPass(g.gl as unknown as WebGL2RenderingContext);
  const q = quadsOf([row("conf", { state: "confirmed" }), row("cand", { f_lo_hz: 100.2e6, f_hi_hz: 100.4e6 })]);
  pass.draw(RECT, q);
  const draws = g.draws();
  assert.equal(draws.length, q.length);
  const modes = draws.map((d) => d.u!.uPat[0]);
  assert.ok(modes.includes(1), "a hatch fill was submitted");
  assert.ok(modes.includes(2) && modes.includes(3), "dashes along x and y were submitted");
  // Plain strokes turn the pattern off; they do not inherit the previous quad's.
  q.forEach((x, i) => { if (!x.pattern) assert.equal(draws[i].u!.uPat[0], 0); });
  // The pattern is anchored to the feature's own corner, so it moves with it.
  const fillAt = q.findIndex((x) => x.part === "fill");
  const c = toClip({ f0Hz: 100.9e6, f1Hz: 101.1e6, t0Ns: (T0 - 10) * S, t1Ns: (T0 - 5) * S }, PANE);
  near(draws[fillAt].u!.uPatOrigin[0], cssX(c[0])); near(draws[fillAt].u!.uPatOrigin[1], ((c[1] + 1) / 2) * RECT.h);
  const src = g.shaders.filter((s) => /uInk/.test(s)).join("\n");
  assert.match(src, /discard/);
  assert.equal(/sampler2D|cmap|cellMark|texture\(/.test(src), false);
});

// ---------------------------------------------------------------------------
// Test DOM (the same shape `surface-pins.test.ts` uses)
// ---------------------------------------------------------------------------

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

// Keep the type import used (PlacedPin is part of the public shape these tests read).
export type _Placed = PlacedPin;
