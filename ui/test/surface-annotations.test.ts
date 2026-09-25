// T-820 / MAP-20: human-authored annotations on the canvas (`ui/src/surface/annotations.ts`).
//
// What must hold: an annotation is placed through the SAME `toClip` every other mark and every tile
// is placed by (so it moves with the rows on the same frame); it is visibly a different KIND of mark
// from anything that claims something about the air (dashed, rose — never a solid box); a note off
// the pane draws nothing; and the label/hover hit-test read the geometry that was drawn.
import { test } from "node:test";
import assert from "node:assert/strict";
import {
  ANNOTATION_MARK, annotationAt, annotationLabels, annotationQuads, isBoxShaped, type MarkAnnotation,
} from "../src/surface/annotations";
import { CONFIRMED_MARK, MEASUREMENT_MARK, SELECTION_MARK } from "../src/surface/marks";
import { toClip, type PaneRect } from "../src/surface/surface";
import type { Box } from "../src/surface/lattice";

const T0 = 1_789_300_000;
const PANE: Box = { f0Hz: 100e6, f1Hz: 110e6, t0Ns: T0 * 1e9, t1Ns: (T0 + 100) * 1e9 };
const RECT: PaneRect = { x: 0, y: 0, w: 1000, h: 500 };

const BOX: MarkAnnotation = { id: "a-box", kind: "box", f_lo_hz: 102e6, f_hi_hz: 104e6, t0_s: T0 + 20, t1_s: T0 + 60, label: "off-raster" };
const MARKER: MarkAnnotation = { id: "a-mk", kind: "marker", f_lo_hz: 105e6, f_hi_hz: 105e6, t0_s: T0 + 50, t1_s: T0 + 50, label: "here" };
const NOTE: MarkAnnotation = { id: "a-tx", kind: "text", f_lo_hz: 108e6, f_hi_hz: 108e6, t0_s: T0 + 80, t1_s: T0 + 80, label: "revisit" };

test("a box is placed through toClip — its dashed edges lie exactly on the rectangle toClip gives", () => {
  const q = annotationQuads([BOX], null, PANE, RECT);
  const [x0, y0, x1, y1] = toClip({ f0Hz: BOX.f_lo_hz, f1Hz: BOX.f_hi_hz, t0Ns: BOX.t0_s * 1e9, t1Ns: BOX.t1_s * 1e9 }, PANE);
  const eps = 1e-9;
  const xs = q.flatMap((x) => [x.clip[0], x.clip[2]]), ys = q.flatMap((x) => [x.clip[1], x.clip[3]]);
  assert.ok(Math.abs(Math.min(...xs) - x0) < eps && Math.abs(Math.max(...xs) - x1) < eps);
  assert.ok(Math.abs(Math.min(...ys) - y0) < eps && Math.abs(Math.max(...ys) - y1) < eps);
  for (const x of q) { assert.equal(x.kind, "annotation"); assert.equal(x.id, "a-box"); }
});

test("an annotation is DISTINCT from every claim about the air: dashed (many short edges), in its own ink", () => {
  const q = annotationQuads([BOX], null, PANE, RECT);
  assert.ok(q.length > 4, `a solid box is 4 edges; a dashed one is many — got ${q.length}`);
  for (const ink of [CONFIRMED_MARK, SELECTION_MARK, MEASUREMENT_MARK]) assert.notDeepEqual(ANNOTATION_MARK.slice(0, 3), ink.slice(0, 3));
  for (const x of q) assert.deepEqual(x.rgba, ANNOTATION_MARK);
});

test("the box moves with the pane's time mapping: scrolling the pane moves every edge by the same clip delta", () => {
  const later: Box = { ...PANE, t0Ns: PANE.t0Ns + 10e9, t1Ns: PANE.t1Ns + 10e9 };
  const a = annotationQuads([BOX], null, PANE, RECT), b = annotationQuads([BOX], null, later, RECT);
  assert.equal(a.length, b.length);
  const dy = b[0].clip[1] - a[0].clip[1];
  assert.ok(Math.abs(dy - (-2 * 10 / 100)) < 1e-9, "10 s of a 100 s pane is 0.2 clip units down");
  for (let i = 0; i < a.length; i++) assert.ok(Math.abs((b[i].clip[1] - a[i].clip[1]) - dy) < 1e-9);
});

test("a marker is a + and a text note a hollow square, centred on the point; off-pane draws NOTHING", () => {
  const m = annotationQuads([MARKER], null, PANE, RECT);
  const n = annotationQuads([NOTE], null, PANE, RECT);
  assert.equal(m.length, 2);
  assert.equal(n.length, 4);
  const [cx, cy] = toClip({ f0Hz: 105e6, f1Hz: 105e6, t0Ns: (T0 + 50) * 1e9, t1Ns: (T0 + 50) * 1e9 }, PANE);
  for (const x of m) {
    assert.ok(Math.abs((x.clip[0] + x.clip[2]) / 2 - cx) < 1e-9 && Math.abs((x.clip[1] + x.clip[3]) / 2 - cy) < 1e-9);
  }
  const away: MarkAnnotation = { ...MARKER, f_lo_hz: 200e6, f_hi_hz: 200e6 };
  const farBox: MarkAnnotation = { ...BOX, t0_s: T0 + 500, t1_s: T0 + 600 };
  assert.deepEqual(annotationQuads([away, farBox], null, PANE, RECT), []);
  assert.deepEqual(annotationLabels([away, farBox], PANE, RECT), []);
});

test("a zero-area 'box' is drawn as a point, never a fabricated rectangle", () => {
  assert.equal(isBoxShaped(BOX), true);
  assert.equal(isBoxShaped({ ...BOX, t1_s: BOX.t0_s }), false);
  assert.equal(isBoxShaped(MARKER), false);
});

test("labels: at a box's top-left and just right of a point, in the pane's px", () => {
  const ls = annotationLabels([BOX, MARKER], PANE, RECT);
  assert.deepEqual(ls.map((l) => l.text), ["off-raster", "here"]);
  assert.ok(Math.abs(ls[0].x - 200) < 1e-6, "102 MHz of 100–110 across 1000 px");
  assert.ok(Math.abs(ls[0].y - 300) < 1e-6, "the box's newest edge, 60 s of 100 over 500 px");
  assert.ok(ls[1].x > 500 && Math.abs(ls[1].y - 250) < 1e-6);
});

test("annotationAt hits what was drawn: inside the box, on the glyph, and nothing elsewhere", () => {
  const items = [BOX, MARKER, NOTE];
  assert.equal(annotationAt(items, PANE, RECT, { x: 300, y: 200 })?.id, "a-box");
  assert.equal(annotationAt(items, PANE, RECT, { x: 502, y: 249 })?.id, "a-mk");
  assert.equal(annotationAt(items, PANE, RECT, { x: 800, y: 400 })?.id, "a-tx");
  assert.equal(annotationAt(items, PANE, RECT, { x: 50, y: 50 }), null);
});
