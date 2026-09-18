// T-442, the pane model. Three claims, asserted the way T-347 asserted its own:
//
//  1. **A pane's pause IS its time window.** Not a flag beside one. The type has two arms carrying
//     different data, so "scrubbed but not paused" and "paused but with the live window" are states
//     that cannot be written down; and freezing is a coordinate change, so the frame you pause on
//     is identical to the frame before it.
//  2. **Pausing one pane provably does not affect another** — and capture keeps advancing
//     throughout. T-347's defect was invisible to a single-client test, so this drives TWO panes
//     over a run of frames with the live edge advancing, and asserts **values**: the running pane's
//     box top strictly advances, the paused one's does not move at all, and the requests the client
//     builds for each go on saying that. Plus the wire leg: pausing a pane makes **no call at all**,
//     which is what makes it unable to reach another viewer or the radio (T-340's spy-client shape).
//  3. **The level is stated per pane** (docs/16 §8.5a): two viewports at different levels
//     legitimately differ, so each pane says which level it resolved to, from the `PaneReport` the
//     renderer actually drew with.

import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { CELL } from "../src/surface/cellrule";
import { keyOf, type Lattice, type TileAddr } from "../src/surface/lattice";
import {
  PaneModel, boxOf, freezeAt, following, levelDivergenceNote, paneStatuses,
  type PaneState, type TimeWindow,
} from "../src/surface/panes";
import { Surface } from "../src/surface/surface";
import { TileCache } from "../src/surface/tilecache";
import type { TileData } from "../src/surface/tile";
import { stubGl } from "./surface-glstub";

const LAT: Lattice = { scheme: "view", cells: 256, f0Hz: 6250, t0Ns: 1e9, levelsF: 20, levelsT: 15 };
const S = 1e9; // one second in ns
const T0 = 1_700_000_000 * S; // an absolute capture instant; the surface's own clock, not a wall clock
// The surface's own extent: the device's tunable range, and a day of retained history.
const BOUNDS = { f0Hz: 1e6, f1Hz: 6e9, t0Ns: T0 - 86_400 * S, t1Ns: T0 };
const W = 1200, H = 800;

const model = (over: Partial<ConstructorParameters<typeof PaneModel>[0]> = {}) =>
  new PaneModel({ bounds: BOUNDS, lattice: LAT, width: W, height: H, freq: { centerHz: 100.8e6, spanHz: 2.4e6 }, spanNs: 20 * S, ...over });

const flush = () => new Promise((r) => setImmediate(r));

function data(a: TileAddr): TileData {
  return {
    addr: a, key: keyOf(a), nf: 2, nt: 2,
    value: new Float32Array([-90, -80, -70, -60]),
    state: new Uint8Array([CELL.OBSERVED, CELL.OBSERVED, CELL.OBSERVED, CELL.OBSERVED]),
    tier: "spectrum-history", answeredLevel: 1, fold: { frequency: "exact", time: "exact" },
    rangeDb: { lo: -100, hi: -60 }, bytes: 192 * 1024, serverInFlightLimit: null,
  };
}

/** A surface over the stub, recording every tile address the client asks for. */
function harness() {
  const g = stubGl(W, H);
  const asked: TileAddr[] = [];
  const surface = new Surface(
    g.canvas, LAT,
    (tex) => new TileCache(tex, (a) => { asked.push(a); return Promise.resolve(data(a)); }, { inFlight: 64, now: () => 0 }),
    { pinParents: false },
  );
  surface.setScale(-100, -60);
  return { g, surface, asked };
}

// ——— 1. the pause is the window ———

test("a pane's pause is its TIME WINDOW, and there is no flag beside it", () => {
  const m = model();
  const [p] = m.list();
  assert.equal(following(p), true);
  // The following arm carries NO centre: a pinned pane borrows the edge, it does not store one.
  assert.deepEqual(Object.keys(p.time).sort(), ["live", "spanNs"]);
  m.pause(p.id, T0);
  const paused = m.get(p.id)!;
  assert.equal(following(paused), false);
  assert.deepEqual(Object.keys(paused.time).sort(), ["centerNs", "live", "spanNs"]);
  // Nothing anywhere in a pane's state is a pause flag.
  const keys = [...Object.keys(paused), ...Object.keys(paused.time), ...Object.keys(paused.freq)];
  assert.ok(!keys.some((k) => /paus/i.test(k)), `a pause flag came back beside the window: ${keys.join(",")}`);
  const src = readFileSync("src/surface/panes.ts", "utf8");
  assert.equal(/\bpaused\s*:/.test(src), false, "T-347 removed `paused` as a field; a per-pane one is the same defect scoped smaller");
});

test("there is no third state: a scrubbed pane is a paused pane, and following carries no offset", () => {
  const m = model();
  const [p] = m.list();
  m.panTime(p.id, -5 * S); // scrub back five seconds while the pane was following
  assert.equal(m.isFollowing(p.id), false, "a pane cannot be pinned to the edge AND offset from it");
  m.follow(p.id);
  assert.deepEqual(Object.keys(m.get(p.id)!.time).sort(), ["live", "spanNs"], "resuming must DROP the centre, not keep a stale one beside `live`");
});

test("pausing is a COORDINATE CHANGE, not a mode change: the frame you pause on is the frame before it", () => {
  const m = model();
  const [p] = m.list();
  const before = boxOf(m.get(p.id)!, T0);
  m.pause(p.id, T0);
  assert.deepEqual(boxOf(m.get(p.id)!, T0), before, "pausing moved the view");
  // …and pausing twice does not teleport a scrubbed pane to the live edge.
  m.panTime(p.id, -60 * S);
  const scrubbed = boxOf(m.get(p.id)!, T0 + 30 * S);
  m.pause(p.id, T0 + 30 * S);
  assert.deepEqual(boxOf(m.get(p.id)!, T0 + 30 * S), scrubbed);
  const live: TimeWindow = { live: true, spanNs: 20 * S };
  assert.deepEqual(freezeAt(live, T0), { live: false, centerNs: T0 - 10 * S, spanNs: 20 * S });
  assert.deepEqual(freezeAt(freezeAt(live, T0), T0 + 99 * S), freezeAt(live, T0), "freezing a frozen window must be a no-op");
});

test("follow-mode pins a pane to the GROWING EDGE, and zooming time keeps it pinned", () => {
  const m = model();
  const [p] = m.list();
  for (let i = 0; i < 5; i++) {
    const edge = T0 + i * S;
    const b = boxOf(m.get(p.id)!, edge);
    assert.equal(b.t1Ns, edge, "a following pane's newest row IS the edge, re-derived every frame");
    assert.equal(b.t0Ns, edge - 20 * S);
  }
  m.zoomTime(p.id, 4);
  assert.equal(m.isFollowing(p.id), true, "a zoom is not a pause");
  const b = boxOf(m.get(p.id)!, T0 + 5 * S);
  assert.equal(b.t1Ns, T0 + 5 * S);
  assert.equal(b.t1Ns - b.t0Ns, 80 * S);
});

// ——— 2. independence, asserted on values over a run of frames ———

test("PAUSING ONE PANE DOES NOT AFFECT ANOTHER, and capture keeps advancing throughout", () => {
  const m = model();
  const a = m.list()[0].id;
  const b = m.split(a, "columns")!;
  // They start on the identical box — one surface, two places to look from.
  assert.deepEqual(boxOf(m.get(a)!, T0), boxOf(m.get(b)!, T0));

  const edges: number[] = [];
  const aTops: number[] = [], bTops: number[] = [];
  let bStateAtPause: PaneState | null = null;
  for (let frame = 0; frame < 12; frame++) {
    const edge = T0 + frame * S; // capture is always-on: the edge advances every frame, unconditionally
    edges.push(edge);
    if (frame === 4) { bStateAtPause = m.get(b)!; m.pause(a, edge); }
    if (frame === 7) { m.panFreq(a, 5e6); m.zoomTime(a, 2); m.panTime(a, -3 * S); } // keep poking A
    const views = m.views(edge);
    aTops.push(views.find((v) => v.id === a)!.box.t1Ns);
    bTops.push(views.find((v) => v.id === b)!.box.t1Ns);
  }

  // Capture never stopped, and nothing in the model could have stopped it.
  for (let i = 1; i < edges.length; i++) assert.ok(edges[i] > edges[i - 1]);
  // B kept running: strictly advancing capture timestamps, every frame, including after A paused.
  for (let i = 1; i < bTops.length; i++) {
    assert.ok(bTops[i] > bTops[i - 1], `B's view stopped advancing at frame ${i} — one pane froze another`);
    assert.equal(bTops[i], edges[i], "B follows the live edge");
  }
  // A stopped, and stayed stopped, at the instant it was paused.
  for (let i = 5; i < aTops.length; i++) assert.ok(aTops[i] <= aTops[4], "a paused pane drifted forward");
  assert.equal(aTops[4], edges[4]);
  assert.ok(aTops[11] < edges[11], "…and the edge left it behind, which is the point of pausing");
  // Everything done to A left B's state byte-identical.
  assert.deepEqual(m.get(b), bStateAtPause, "a mutation of A reached B");
});

test("the requests the client builds say it too: the paused pane's tiles stop moving, the live pane's do not", async () => {
  const h = harness();
  const m = model({ spanNs: 400 * S }); // a span wide enough that a tile boundary is crossed within the run
  const a = m.list()[0].id;
  const b = m.split(a, "columns")!;
  m.pause(a, T0);

  // Per frame, the set of time rows each pane addressed — the request the client builds, not the
  // response it renders (the ui/test rule T-367 left behind).
  const rows = new Map<string, string[]>([[a, []], [b, []]]);
  for (let frame = 0; frame < 40; frame++) {
    const edge = T0 + frame * 30 * S;
    for (const v of m.views(edge)) {
      h.asked.length = 0;
      h.surface.render([v]);
      rows.get(v.id)!.push(JSON.stringify([...new Set(h.asked.map((x) => x.tIndex))].sort((x, y) => x - y)));
    }
    await flush();
  }
  const distinct = (id: string) => new Set(rows.get(id)!.filter((s) => s !== "[]")).size;
  assert.equal(distinct(a), 1, "a paused pane addressed a different time row: its window moved while capture advanced");
  assert.ok(distinct(b) > 1, "the live pane never crossed a tile row, so the run proves nothing");
});

test("pausing, panning and zooming a pane call NOTHING: on the wire, pausing is nothing (T-347/T-340)", () => {
  const calls: unknown[] = [];
  const g = globalThis as { fetch?: unknown };
  const real = g.fetch;
  g.fetch = (...args: unknown[]) => { calls.push(args); return Promise.reject(new Error("the pane model must not reach the network")); };
  try {
    const m = model();
    const a = m.list()[0].id;
    const b = m.split(a, "rows")!;
    m.pause(a); m.follow(a); m.panTime(a, -9 * S); m.panFreq(a, 1e6);
    m.zoomFreq(a, 0.5, 0.25); m.zoomTime(b, 2); m.setDevice(b, "hackrf-0");
    m.setFollowing(b, false); m.goTo(b, T0 - 60 * S); m.setSplitFraction(b, 0.3);
    m.views(T0 + S); m.close(b);
  } finally { if (real) g.fetch = real; else delete g.fetch; }
  assert.deepEqual(calls, [], "the pane model reached the network; a view-state change must be unable to touch the device or another viewer");
});

test("independent pan and zoom: moving one pane changes neither the other's box nor the level it resolves to", () => {
  const h = harness();
  const m = model();
  const a = m.list()[0].id;
  const b = m.split(a, "columns")!;
  const before = h.surface.render(m.views(T0));
  m.zoomFreq(a, 200);         // A pulls out several levels on both axes
  m.panFreq(a, 30e6);
  m.zoomTime(a, 500);
  const after = h.surface.render(m.views(T0));
  const bBefore = before.find((r) => r.id === b)!, bAfter = after.find((r) => r.id === b)!;
  assert.equal(bAfter.levelF, bBefore.levelF, "A's frequency zoom moved B's level");
  assert.equal(bAfter.levelT, bBefore.levelT, "A's time zoom moved B's level");
  const aBefore = before.find((r) => r.id === a)!, aAfter = after.find((r) => r.id === a)!;
  assert.ok(aAfter.levelF > aBefore.levelF && aAfter.levelT > aBefore.levelT, "A did not actually move, so the test proves nothing");
});

// ——— the surface stays ONE surface ———

test("split gives two viewports onto the SAME surface: one context, tiled rects, and the last pane never closes", () => {
  const h = harness();
  const m = model();
  const a = m.list()[0].id;
  const b = m.split(a, "columns")!;
  const c = m.split(b, "rows")!;
  assert.equal(m.count, 3);
  h.surface.render(m.views(T0));
  assert.equal(h.g.contextCount(), 1, "panes are viewports, not canvases");

  const rects = [...m.rects().values()];
  assert.equal(rects.length, 3);
  const area = rects.reduce((s, r) => s + r.w * r.h, 0);
  assert.ok(area > 0.95 * W * H && area <= W * H, `panes must tile the canvas (covered ${area} of ${W * H})`);
  for (let i = 0; i < rects.length; i++) {
    for (let j = i + 1; j < rects.length; j++) {
      const p = rects[i], q = rects[j];
      assert.ok(p.x + p.w <= q.x || q.x + q.w <= p.x || p.y + p.h <= q.y || q.y + q.h <= p.y, "two panes overlap");
    }
  }
  assert.equal(m.close(c), true);
  assert.equal(m.close(b), true);
  assert.equal(m.close(a), false, "with no viewport there is nowhere to look FROM; the surface does not stop existing");
  assert.equal(m.count, 1);
});

test("panes far apart stay one surface: 100 MHz and 2.4 GHz in one frame, one cache, one edge", () => {
  const h = harness();
  const m = model();
  const a = m.list()[0].id;
  const b = m.split(a, "columns")!;
  m.setFreq(b, 2.45e9, 80e6);
  const reports = h.surface.render(m.views(T0));
  assert.equal(reports.length, 2);
  assert.notEqual(reports[0].levelF, reports[1].levelF, "the two ranges are not watchable at one density — which is why panes exist");
  const views = m.views(T0);
  assert.equal(views[0].box.t1Ns, views[1].box.t1Ns, "one surface, one time axis: both panes read the same edge");
});

test("a pan is a pan: panning past the surface's extent clamps, and panning time forward does not re-enter follow", () => {
  const m = model();
  const [p] = m.list();
  m.panFreq(p.id, 1e12);
  const f = m.get(p.id)!.freq;
  assert.ok(f.centerHz + f.spanHz / 2 <= BOUNDS.f1Hz + 1e-6, "a pane wandered off the device's range");
  m.pause(p.id, T0);
  m.panTime(p.id, 10_000 * S); // drag hard toward the edge
  assert.equal(m.isFollowing(p.id), false, "ending a drag near the live edge must not silently re-enter follow-mode");
  assert.ok(boxOf(m.get(p.id)!, T0).t1Ns <= T0 + 1e-6);
});

// ——— 3. state the level per pane (docs/16 §8.5a) ———

test("each pane STATES the level it resolved to, from the report the renderer actually drew with", () => {
  const h = harness();
  const m = model();
  const a = m.list()[0].id;
  const b = m.split(a, "columns")!;
  m.setFreq(b, 3e9, 4e9);       // the minimap-shaped case: nearly the whole device range
  m.zoomTime(b, 64);
  m.pause(a, T0);
  const reports = h.surface.render(m.views(T0 + 45 * S));
  const st = paneStatuses(m.list(), reports, LAT, T0 + 45 * S, m.rects());

  const A = st.find((s) => s.id === a)!, B = st.find((s) => s.id === b)!;
  assert.equal(A.levelF, reports.find((r) => r.id === a)!.levelF, "the stated level must be the DRAWN level, not a second calculation");
  assert.ok(B.levelF > A.levelF, "a 4 GHz-wide pane cannot be at the same frequency level as a 2.4 MHz one");
  assert.ok(A.levelLabel.includes(`level ${A.levelF}/${A.levelT}`));
  assert.ok(/Hz/.test(A.levelLabel) && /s\b|ms/.test(A.levelLabel), `the label must state the CELL, not just an index: ${A.levelLabel}`);
  assert.deepEqual(A.differsFrom, [b]);
  assert.deepEqual(B.differsFrom, [a]);
  assert.equal(A.following, false);
  assert.equal(A.timeLabel, "−45 s", "a frozen pane says how far behind the live edge it is");
  assert.equal(B.timeLabel, "LIVE");
  assert.ok(A.rect && A.rect.w > 0);

  const note = levelDivergenceNote(st);
  assert.ok(note && /different pyramid levels/.test(note) && /maximum over more cells/.test(note),
    "§8.5a: the fix is to STATE the difference, not to hide it");
});

test("…and when two panes are at the same level there is nothing to explain", () => {
  const h = harness();
  const m = model();
  const a = m.list()[0].id;
  m.split(a, "rows");
  const reports = h.surface.render(m.views(T0));
  const st = paneStatuses(m.list(), reports, LAT, T0, m.rects());
  assert.equal(st.length, 2);
  assert.deepEqual(st[0].differsFrom, []);
  assert.equal(levelDivergenceNote(st), null);
});

test("a pane carries WHOSE coverage decides its grey, and it reaches the renderer", () => {
  const m = model();
  const a = m.list()[0].id;
  assert.equal(m.get(a)!.device, "any", "the union of every front end is the default: extra SDRs widen coverage");
  m.setDevice(a, "hackrf-0");
  assert.equal(m.views(T0)[0].device, "hackrf-0");
});
