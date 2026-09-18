// **T-388/T-362's invariants, re-pointed at the canvas** (T-445).
//
// The retired `ui/test/timebox.test.ts` proved the same properties about the same user-visible
// thing — a signal's box — against the renderer that drew it then (`ui/src/timebox.ts` over
// `ui/src/waterfall.ts`). That renderer is gone; the properties are not, so they are here, against
// `ui/src/surface/marks.ts` and the pass that actually submits them.
//
// The original bug is worth restating, because it is what these tests are shaped around: the boxes
// **drifted and then jumped**, because T-261 recomputed their top and height only on the ~1 s data
// poll while the WebGL waterfall scrolled every animation frame. The mapping was right; the clock
// it ran on was wrong. So the test that matters advances the live edge **without any poll at all**
// and asserts the box moved with it — a test that only checked placement at poll boundaries passes
// on the broken code — and its control is an unchanged edge, which must move nothing.
//
// On this surface the defect is unreachable rather than fixed, and that is a structural claim these
// tests are written to hold up:
//
//  - the marks are computed **inside** `SurfaceView.frame()`, from the `PaneView` the data pass was
//    handed on that frame, so there is no second layout to run on a different cadence;
//  - they go through **`toClip`, the same function** `Surface` places its tiles with — asserted
//    numerically below, against `toClip` itself, not against a copy of its arithmetic;
//  - `marks.ts` takes no store, no clock and no poll: it is `(boxes, edgeNs, paneBox, rect)`.

import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { CELL } from "../src/surface/cellrule";
import { keyOf, type Lattice, type TileAddr } from "../src/surface/lattice";
import {
  CANDIDATE_MARK, CONFIRMED_MARK, OPEN_EDGE_MARK, PENDING_MARK, SELECTION_MARK, markAt, markQuads,
  normalizeRegion, pendingMarkBox, pointOn, quadSizePx, selectionMarkBoxes, signalMarkBoxes,
  type MarkBox,
} from "../src/surface/marks";
import { Surface, toClip, type PaneRect } from "../src/surface/surface";
import { TileCache } from "../src/surface/tilecache";
import type { TileData } from "../src/surface/tile";
import { SurfaceView } from "../src/surface/view";
import { stubGl } from "./surface-glstub";

const S = 1e9;
const T0 = 1_700_000_000 * S;
/** A pane looking at 2.4 MHz over 20 s, ending at the edge. */
const PANE = { f0Hz: 99.6e6, f1Hz: 102e6, t0Ns: T0 - 20 * S, t1Ns: T0 };
const RECT: PaneRect = { x: 0, y: 0, w: 1000, h: 500 };

const box = (o: Partial<MarkBox> = {}): MarkBox => ({
  id: "e1", kind: "signal-box", f0Hz: 100.5e6, f1Hz: 101.1e6,
  t0Ns: T0 - 10 * S, t1Ns: T0 - 2 * S, rgba: CONFIRMED_MARK, open: false, ...o,
});

/** The vertical extent, in clip space, the four edges of one box enclose. */
function extent(quads: readonly { clip: readonly [number, number, number, number] }[]) {
  return {
    y0: Math.min(...quads.map((q) => q.clip[1])), y1: Math.max(...quads.map((q) => q.clip[3])),
    x0: Math.min(...quads.map((q) => q.clip[0])), x1: Math.max(...quads.map((q) => q.clip[2])),
  };
}

const near = (a: number, b: number, eps = 1e-9) => assert.ok(Math.abs(a - b) <= eps, `${a} ≉ ${b}`);

// ---------------------------------------------------------------------------
// The one mapping
// ---------------------------------------------------------------------------

test("T-337/T-388: a mark is placed by the DATA PASS's own toClip — not by arithmetic that resembles it", () => {
  const b = box();
  const q = markQuads([b], T0, PANE, RECT, { strokePx: 2, minPx: 0 });
  const e = extent(q);
  // `toClip` is what `Surface.render` calls for every tile it draws (surface.ts). Asserting the
  // mark's own extent against it — rather than against a recomputation here — is the difference
  // between "these two agree today" and "there is one function".
  const [cx0, cy0, cx1, cy1] = toClip({ f0Hz: b.f0Hz, f1Hz: b.f1Hz, t0Ns: b.t0Ns, t1Ns: b.t1Ns! }, PANE);
  near(e.x0, cx0); near(e.x1, cx1); near(e.y0, cy0); near(e.y1, cy1);
  const src = readFileSync("src/surface/marks.ts", "utf8");
  assert.match(src, /import \{ toClip/, "and it imports it rather than reimplementing it");
});

// ---------------------------------------------------------------------------
// T-362: the box tracks the live edge between polls, and does not jump at one
// ---------------------------------------------------------------------------

test("T-362/T-410: an OPEN box follows the live edge with no poll of any kind — the edge is its only input", () => {
  const open = box({ t1Ns: null, open: true });
  // Three frames, three edges, and NOTHING ELSE CHANGES: no new row object, no re-fetched
  // inventory, no store write. On the old renderer the box could only move when the poll delivered
  // a new `t_end_s`, which is precisely why it drifted between polls and snapped at one.
  const at = (edgeNs: number) => {
    const pane = { ...PANE, t0Ns: edgeNs - 20 * S, t1Ns: edgeNs };
    return extent(markQuads([open], edgeNs, pane, RECT, { minPx: 0 })).y1;
  };
  // The pane follows the edge, so an open box's newest edge sits at the top of the pane on every
  // frame — and stays there, rather than walking away from it between polls.
  near(at(T0), 1, 1e-9);
  near(at(T0 + 3 * S), 1, 1e-9);
  near(at(T0 + 7.5 * S), 1, 1e-9);
  // A FROZEN pane is the other half: capture advances, the view holds, and the box holds with the
  // view — the paused case the old test covered, and the "pause freezes the view, not the capture"
  // invariant expressed as geometry.
  const closed = box({ t0Ns: T0 - 10 * S, t1Ns: T0 - 2 * S });
  const frozen = markQuads([closed], T0, PANE, RECT, { minPx: 0 });
  const later = markQuads([closed], T0 + 5 * S, PANE, RECT, { minPx: 0 });
  assert.deepEqual(later, frozen, "a frozen pane holds, whatever capture is doing — the edge is not its input");
  // And an OPEN box on a frozen pane keeps its measured start where it is, while its cap clips at
  // the top of the window the user is holding: the box grows past what is on screen rather than
  // dragging the view forward.
  near(extent(markQuads([open], T0 + 5 * S, PANE, RECT, { minPx: 0 })).y0,
    extent(markQuads([open], T0, PANE, RECT, { minPx: 0 })).y0, 1e-9);
});

test("T-362 control: an unchanged edge produces BYTE-IDENTICAL quads — a repeat is not a jump", () => {
  const b = [box(), box({ id: "e2", t1Ns: null, open: true })];
  const a1 = markQuads(b, T0, PANE, RECT);
  const a2 = markQuads(b, T0, PANE, RECT);
  assert.deepEqual(a2, a1, "nothing moved, so nothing may be drawn differently");
});

// ---------------------------------------------------------------------------
// T-420: the drawn extent IS the asked-for extent
// ---------------------------------------------------------------------------

test("T-420: a box spanning the pane's window fills the pane — there is no ring to paint 4 % of", () => {
  // The sliver defect was a *renderer* property: `historyRows` emitted one texture row per served
  // cell into a 512-row ring whose whole height was drawn, so 21 one-second cells lit 4 % of the
  // pane and the wider the drag the thinner the sliver. Here a rectangle in (Hz, ns) maps onto the
  // pane's own box, so covering the window means covering the pane, at any span.
  for (const spanS of [20, 600, 86_400]) {
    const pane = { ...PANE, t0Ns: T0 - spanS * S, t1Ns: T0 };
    const full = box({ f0Hz: pane.f0Hz, f1Hz: pane.f1Hz, t0Ns: pane.t0Ns, t1Ns: pane.t1Ns });
    const e = extent(markQuads([full], T0, pane, RECT, { minPx: 0 }));
    near(e.y0, -1); near(e.y1, 1); near(e.x0, -1); near(e.x1, 1);
  }
});

// ---------------------------------------------------------------------------
// Clipping, floors and honesty
// ---------------------------------------------------------------------------

test("a box off the pane draws NOTHING; one partly on it is clipped, not moved", () => {
  const off = box({ t0Ns: T0 - 100 * S, t1Ns: T0 - 90 * S });
  assert.deepEqual(markQuads([off], T0, PANE, RECT), [], "a rectangle clamped to the border would claim a boundary it does not have");
  const half = box({ f0Hz: 98e6, f1Hz: 100.5e6 }); // starts left of the pane
  const q = markQuads([half], T0, PANE, RECT, { strokePx: 2, minPx: 0 });
  const e = extent(q);
  near(e.x0, -1, 1e-9);
  assert.ok(!q.some((x) => x.clip[0] < -1 || x.clip[2] > 1), "nothing is drawn outside the pane");
  // The left edge is off screen, so it is not drawn: three edges, not four.
  assert.equal(q.length, 3);
});

test("a sub-pixel box is widened about its own centre, and the floor is never read back as a measurement", () => {
  const tiny = box({ f0Hz: 101e6, f1Hz: 101e6 + 500, t0Ns: T0 - 5 * S, t1Ns: T0 - 5 * S + 1e6 });
  const q = markQuads([tiny], T0, PANE, RECT, { strokePx: 1, minPx: 2 });
  const e = extent(q);
  const midX = ((tiny.f0Hz + tiny.f1Hz) / 2 - PANE.f0Hz) / (PANE.f1Hz - PANE.f0Hz) * 2 - 1;
  near((e.x0 + e.x1) / 2, midX, 1e-6);
  assert.ok(quadSizePx(q[0], RECT).wPx >= 1, "a 500 Hz emission in a 2.4 MHz pane is still visible");
  assert.ok(e.x1 - e.x0 >= (2 * 2) / RECT.w - 1e-12);
});

test("T-410: an open box's newest edge is marked as the LIVE EDGE, distinctly from a measured end", () => {
  const closed = markQuads([box()], T0, PANE, RECT, { minPx: 0 });
  const open = markQuads([box({ t1Ns: null, open: true })], T0, PANE, RECT, { minPx: 0 });
  assert.deepEqual(closed[closed.length - 1].rgba, CONFIRMED_MARK, "a measured end is the box's own ink");
  assert.deepEqual(open[open.length - 1].rgba, OPEN_EDGE_MARK, "an open one says the top is the live edge");
  assert.notDeepEqual(OPEN_EDGE_MARK, CONFIRMED_MARK);
});

test("every mark is a STROKE, never a wash — the overlay pass cannot tint a measurement", () => {
  const b = [box(), box({ id: "s1", kind: "selection-box", f0Hz: 99.8e6, f1Hz: 101.8e6, t0Ns: PANE.t0Ns, t1Ns: PANE.t1Ns })];
  for (const q of markQuads(b, T0, PANE, RECT, { strokePx: 2, openPx: 3 })) {
    const { wPx, hPx } = quadSizePx(q, RECT);
    assert.ok(Math.min(wPx, hPx) <= 3 + 1e-6, `${q.kind} ${q.id} is ${wPx}×${hPx} px — that is a wash, not a stroke`);
  }
});

// ---------------------------------------------------------------------------
// What a box is made of, and what it refuses to invent
// ---------------------------------------------------------------------------

test("signalMarkBoxes: only candidate/confirmed, only with a measured interval, user band wins", () => {
  const row = (o: Record<string, unknown>) => ({ id: "x", state: "confirmed", f_lo_hz: 1e6, f_hi_hz: 2e6, ...o }) as never;
  const iv = (t0: number, t1: number, open: boolean) => ({ last_interval: { t_start_s: t0, t_end_s: t1, open } });
  const rows = [
    row({ id: "k", presence: iv(100, 118, false) }),
    row({ id: "c", state: "candidate", presence: iv(110, 120, true) }),
    row({ id: "none", presence: null }),                       // no interval: no rectangle, ever
    row({ id: "gone", state: "deleted", presence: iv(1, 2, false) }),
    row({ id: "ub", presence: iv(1, 2, false), user_band: { f_lo: 1.2e6, f_hi: 1.4e6 } }),
  ];
  const out = signalMarkBoxes(rows, "c");
  assert.deepEqual(out.map((b) => b.id), ["k", "c", "ub"], "no box is fabricated, and deleted rows are on no surface");
  assert.deepEqual(out.find((b) => b.id === "k")!.rgba, CONFIRMED_MARK);
  assert.equal(out.find((b) => b.id === "c")!.rgba[3], 1, "the focused row is the opaque one");
  assert.deepEqual(CANDIDATE_MARK.slice(0, 3), out.find((b) => b.id === "c")!.rgba.slice(0, 3), "…same hue, heavier");
  assert.equal(out.find((b) => b.id === "ub")!.f0Hz, 1.2e6, "the band in force, not the superseded one");
  assert.equal(out.find((b) => b.id === "k")!.t1Ns, 118e9, "the end the API served, in ns");
  assert.equal(out.find((b) => b.id === "c")!.t1Ns, null, "open: no end, so the edge decides at draw time");
});

test("selectionMarkBoxes: a frequency-only selection spans the pane's time axis — 'any time', not a made-up duration", () => {
  const timed = { id: "a", f_lo: 100e6, f_hi: 100.2e6, t_lo: 1_700_000_000 - 8, t_hi: 1_700_000_000 - 4 };
  const untimed = { id: "b", f_lo: 101e6, f_hi: 101.2e6, t_lo: null, t_hi: null };
  const [x, y] = selectionMarkBoxes([timed, untimed], "b", PANE);
  assert.equal(x.t0Ns, timed.t_lo * S);
  assert.equal(y.t0Ns, PANE.t0Ns);
  assert.equal(y.t1Ns, PANE.t1Ns);
  assert.equal(y.rgba[3], 1, "the focused selection is the opaque one");
});

// ---------------------------------------------------------------------------
// Hit testing: the click lands on what it looks like it landed on
// ---------------------------------------------------------------------------

test("markAt: containment in BOTH axes, narrowest first — never a frequency-only guess", () => {
  const wide = box({ id: "wide", f0Hz: 100e6, f1Hz: 102e6, t0Ns: T0 - 15 * S, t1Ns: T0 });
  const burst = box({ id: "burst", f0Hz: 100.9e6, f1Hz: 101.0e6, t0Ns: T0 - 4 * S, t1Ns: T0 - 3 * S });
  const boxes = [wide, burst];
  assert.equal(markAt(boxes, T0, 100.95e6, T0 - 3.5 * S)!.id, "burst", "the narrower one wins where they overlap");
  assert.equal(markAt(boxes, T0, 100.95e6, T0 - 10 * S)!.id, "wide", "…and the wide one elsewhere in it");
  // The half that the retired click handler could not do: it resolved a click by FREQUENCY ALONE
  // (`clickTarget` over `inspectHalfWidthHz`), so it would have answered "burst" here, on a row
  // whose box is nowhere near the pointer in time.
  assert.equal(markAt([burst], T0, 100.95e6, T0 - 12 * S), null, "outside in time is outside");
  assert.equal(markAt(boxes, T0, 99.9e6, T0 - 5 * S), null, "outside in frequency is outside");
  // An open box is hittable over its whole DRAWN extent, cap included (T-410).
  const open = box({ id: "open", t1Ns: null, open: true, t0Ns: T0 - 6 * S });
  assert.equal(markAt([open], T0, 100.8e6, T0 - 0.5 * S)!.id, "open");
});

test("pointOn is the exact inverse of the placement, so hover and hit answer about the same pixel", () => {
  const p = pointOn(PANE, RECT, RECT.x + RECT.w / 2, RECT.y + RECT.h / 2);
  near(p.fHz, (PANE.f0Hz + PANE.f1Hz) / 2, 1e-6);
  near(p.tNs, (PANE.t0Ns + PANE.t1Ns) / 2, 1);
  const q = markQuads([box()], T0, PANE, RECT, { minPx: 0 });
  const e = extent(q);
  // A point at the box's clip centre maps back inside the box.
  const mid = pointOn(PANE, RECT, RECT.x + ((e.x0 + e.x1) / 2 + 1) / 2 * RECT.w, RECT.y + ((e.y0 + e.y1) / 2 + 1) / 2 * RECT.h);
  assert.ok(markAt([box()], T0, mid.fHz, mid.tNs), "the centre of the drawn box hits the box");
});

// ---------------------------------------------------------------------------
// The render pass really submits them, in the pane's own viewport
// ---------------------------------------------------------------------------

const LAT: Lattice = { scheme: "view", cells: 256, f0Hz: 6250, t0Ns: 1e9, levelsF: 20, levelsT: 15 };
const BOUNDS = { f0Hz: 1e6, f1Hz: 6e9, t0Ns: T0 - 86_400 * S, t1Ns: T0 };

function tile(a: TileAddr): TileData {
  return {
    addr: a, key: keyOf(a), nf: 2, nt: 2,
    value: new Float32Array([-90, -80, -70, -60]),
    state: new Uint8Array([CELL.OBSERVED, CELL.OBSERVED, CELL.UNOBSERVED, CELL.OBSERVED]),
    tier: "survey-overview", answeredLevel: 1, fold: { frequency: "exact", time: "exact" },
    rangeDb: { lo: -100, hi: -60 }, bytes: 192 * 1024, serverInFlightLimit: null,
  };
}

test("T-388 STRUCTURAL: the marks are computed inside the frame, from the PaneView the data pass got", () => {
  const g = stubGl(1200, 600);
  const seen: { paneId: string; box: typeof PANE; edgeNs: number }[] = [];
  const view = new SurfaceView({
    canvas: g.canvas, lattice: LAT, bounds: BOUNDS, minimapPx: 150,
    cache: (tex) => new TileCache(tex, (a) => Promise.resolve(tile(a)), { inFlight: 64, now: () => 0 }),
    surface: { pinParents: false },
    freq: { centerHz: 100.8e6, spanHz: 2.4e6 }, spanNs: 20 * S,
    marks: (pane, edgeNs) => {
      seen.push({ paneId: pane.id, box: pane.box as typeof PANE, edgeNs });
      return markQuads([box({ f0Hz: pane.box.f0Hz, f1Hz: pane.box.f1Hz, t0Ns: pane.box.t0Ns, t1Ns: null, open: true })],
        edgeNs, pane.box, pane.rect);
    },
  });
  const f = view.frame(T0, []);
  // Once per PANE, and not for the minimap: a signal box belongs where the signal is being looked
  // at, and the map is where you see WHERE the panes are.
  assert.equal(seen.length, view.panes.count);
  assert.deepEqual(seen.map((s) => s.paneId), view.panes.list().map((p) => p.id));
  assert.equal(seen[0].edgeNs, T0, "the edge is reported in, per frame");
  // The box handed to the caller is the pane's own drawn box, not one re-derived from pane state.
  const drawnBox = f.views.find((v) => v.id === seen[0].paneId)!.box;
  assert.deepEqual(seen[0].box, drawnBox);
  assert.ok(f.quads.some((q) => q.kind === "signal-box"), "and the quads reach the frame report");
  assert.ok(f.overlaysDrawn >= f.quads.length, "…and were submitted");
  // A second frame at a later edge re-runs it: there is no cache and no change event between the
  // state and the pixels, which is the whole anti-T-388 claim.
  view.frame(T0 + 3 * S, []);
  assert.equal(seen.length, 2 * view.panes.count);
  assert.equal(seen[seen.length - 1].edgeNs, T0 + 3 * S);
  view.dispose();
});

test("the data pass is byte-identical with marks on and off — a box can never tint a measurement", () => {
  const run = (withMarks: boolean) => {
    const g = stubGl(1200, 600);
    const view = new SurfaceView({
      canvas: g.canvas, lattice: LAT, bounds: BOUNDS, minimapPx: 150,
      cache: (tex) => new TileCache(tex, (a) => Promise.resolve(tile(a)), { inFlight: 64, now: () => 0 }),
      surface: { pinParents: false },
      freq: { centerHz: 100.8e6, spanHz: 2.4e6 }, spanNs: 20 * S,
      marks: withMarks
        ? (pane, edgeNs) => markQuads([box({ f0Hz: pane.box.f0Hz, f1Hz: pane.box.f1Hz, t0Ns: pane.box.t0Ns, t1Ns: null, open: true })], edgeNs, pane.box, pane.rect)
        : null,
    });
    view.frame(T0, []);
    // The data program is the one with a sampler; the overlay program has none (overlay.ts).
    const ops = g.ops.filter((o) => o.kind === "draw" && o.u && "uBox" in o.u);
    view.dispose();
    return JSON.stringify(ops);
  };
  assert.equal(run(true), run(false), "the honesty comparison needs no flag, because the pass cannot reach it");
});

// ---------------------------------------------------------------------------
// The module boundary
// ---------------------------------------------------------------------------

test("no signal logic, no RF constant and no clock in the marks module", () => {
  const src = readFileSync("src/surface/marks.ts", "utf8").replace(/\/\*[\s\S]*?\*\//g, "").replace(/^\s*\/\/.*$/gm, "");
  for (const word of ["Date.now", "performance.now", "toLocaleTimeString", "getTimezoneOffset", "fetch(", "/api/"]) {
    assert.ok(!src.includes(word), `marks.ts must not contain "${word}"`);
  }
  // No RF constant: the only literals are drawing floors, ink and the ns/s scale.
  for (const m of src.matchAll(/(\d[\d_]*(?:\.\d+)?)e(\d+)/g)) {
    assert.ok(["1e9"].includes(m[0]), `marks.ts names an RF-looking constant: ${m[0]}`);
  }
  assert.ok(!/inventory|selections\.ts|explanation|classif/i.test(src), "it decides nothing about what a signal IS");
});

// ---------------------------------------------------------------------------
// The region being stroked out (T-458)
// ---------------------------------------------------------------------------

test("normalizeRegion orders BOTH axes, so a stroke made up-left is the same rectangle as one made down-right", () => {
  const p = { fHz: 100.1e6, tNs: T0 - 5 * S }, q = { fHz: 100.9e6, tNs: T0 - 2 * S };
  const forward = normalizeRegion(p, q);
  assert.deepEqual(normalizeRegion(q, p), forward, "the corner order the hand happened to use is not data");
  assert.deepEqual(normalizeRegion({ fHz: q.fHz, tNs: p.tNs }, { fHz: p.fHz, tNs: q.tNs }), forward);
  assert.equal(forward.f0Hz, 100.1e6);
  assert.equal(forward.f1Hz, 100.9e6);
  assert.equal(forward.t0Ns, T0 - 5 * S);
  assert.equal(forward.t1Ns, T0 - 2 * S);
});

test("the pending band is drawn by the SAME pass as every other mark, in ink no measurement can wear", () => {
  const r = normalizeRegion({ fHz: 100.9e6, tNs: T0 - 2 * S }, { fHz: 100.1e6, tNs: T0 - 5 * S });
  assert.deepEqual(pendingMarkBox(null), [], "no stroke, no band");

  const box = pendingMarkBox(r);
  assert.equal(box.length, 1);
  assert.equal(box[0].kind, "pending-region", "a stroke in progress is not yet a selection, and does not claim to be");
  assert.notEqual(box[0].t1Ns, null,
    "a stroke has two ends the user made: it has nothing to say about the live edge");
  assert.equal(box[0].open, false);
  assert.deepEqual(box[0].rgba, PENDING_MARK);

  // Same call, same pane, same frame: the band is placed by `toClip` like the boxes it is drawn
  // over, which is what stops it lagging the pointer by a poll (T-388's shape).
  const quads = markQuads(box, T0, PANE, RECT);
  assert.ok(quads.length > 0, "the band must actually be submitted");
  for (const q of quads) assert.equal(q.kind, "pending-region");
  const [x0, y0, x1, y1] = toClip({ f0Hz: r.f0Hz, f1Hz: r.f1Hz, t0Ns: r.t0Ns, t1Ns: r.t1Ns }, PANE);
  assert.ok(quads.some((q) => Math.abs(q.clip[0] - x0) < 1e-9), "the left edge sits where toClip puts it");
  assert.ok(quads.some((q) => Math.abs(q.clip[2] - x1) < 1e-9));
  assert.ok(quads.some((q) => Math.abs(q.clip[1] - y0) < 1e-9));
  assert.ok(quads.some((q) => Math.abs(q.clip[3] - y1) < 1e-9));

  // And it cannot be confused with a selection that exists: different ink, different kind.
  assert.notDeepEqual(PENDING_MARK, SELECTION_MARK);
});
