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
    // An `exact` fold on both axes measured every cell it served (T-441). `decodeTile` always fills
    // this in; a fixture that stands in for it has to say what the decoder would have said.
    measured: { nf: 2, nt: 2 },
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

test("a sideways drag does not pause the waterfall: a scrub of ZERO leaves a following pane following", () => {
  // **T-484.** `Preview.drag` pans BOTH axes on every pointer move, so a drag straight along
  // frequency reaches the model as `panFreq(dx)` followed by `panTime(0)` — and `panTime` froze the
  // pane before it looked at the delta. The user changed frequency and the pane silently entered
  // "scrubbed into the past", which `follow`'s own contract says may only happen by an explicit act.
  //
  // It is asserted through the pane's OWN state rather than through a retune control, because the
  // property is about pausing: the control was only the messenger (`paneRetuneOffer` began
  // answering `block: "past"` for a viewport whose only gesture had been sideways, and it was right
  // to). A pane that is following has no centre to store, so `isFollowing` is the whole claim.
  const m = model();
  const [p] = m.list();
  const before = boxOf(m.get(p.id)!, T0);
  m.panFreq(p.id, 2e6);
  m.panTime(p.id, 0);
  assert.equal(m.isFollowing(p.id), true, "a drag along frequency must not stop the pane following the live edge");
  assert.deepEqual(Object.keys(m.get(p.id)!.time).sort(), ["live", "spanNs"], "and it must not have acquired a centre");
  // The time axis really is untouched: same window at the same edge, and it still grows with it.
  assert.deepEqual(boxOf(m.get(p.id)!, T0).t1Ns, before.t1Ns);
  assert.equal(boxOf(m.get(p.id)!, T0 + 30 * S).t1Ns, T0 + 30 * S, "and it still follows");
  // The mutation: the guard is exactly zero, not "small". A real scrub moves the window on its very
  // first pixel (T-456) and, once past T-486's dead zone, still pauses the pane.
  //
  // **It is five seconds rather than one nanosecond, and the change is a correction.** This line
  // read `panTime(p.id, -1)` and claimed "however small". At capture-time magnitudes — T0 is
  // ~1.7e18 ns — a double's ulp is **256 ns**, so a 1 ns scrub does not change the centre at all;
  // the assertion passed because `panTime` froze the pane whether or not the window moved, which is
  // a different proposition from the one it stated. T-486 makes the follow state a function of
  // where the window ended up, so the ulp became visible, and the honest claim is about a scrub
  // that moves something.
  const wasLive = boxOf(m.get(p.id)!, T0);
  m.panTime(p.id, -5 * S);
  assert.notEqual(boxOf(m.get(p.id)!, T0).t1Ns, wasLive.t1Ns, "a real scrub moves the window from its first pixel");
  assert.equal(m.isFollowing(p.id), false, "and a scrub past the dead zone still pauses");
  // And a zero pan on an already-frozen pane changes nothing either.
  const frozen = boxOf(m.get(p.id)!, T0);
  m.panTime(p.id, 0);
  assert.deepEqual(boxOf(m.get(p.id)!, T0), frozen, "a zero pan must not move a frozen pane");
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
  m.panTime(p.id, -600 * S);   // scrub well back, so the drag below is really a drag TOWARD the edge
  m.settleTime(p.id, T0);
  m.panTime(p.id, 10_000 * S); // drag hard toward the edge
  assert.equal(m.isFollowing(p.id), false, "the pan itself does not re-enter follow: the decision belongs to the release");
  assert.ok(boxOf(m.get(p.id)!, T0).t1Ns <= T0 + 1e-6);
  // **T-486 revises the second half of this sentence, and only that half.** It used to read
  // "…and panning time forward does not re-enter follow", full stop, on the reasoning that
  // re-entering follow must be an explicit act rather than a side effect of a gesture ending near
  // the edge. The user reported the consequence twice: a drag released AT the edge left the pane
  // frozen one pixel short of live, and a 1 px twitch cost you live outright. So the explicit act
  // is now the *release* — `settleTime`, called once per stroke — and a stroke that clamped hard
  // against the edge ends live. The pan above still commits nothing; the release does.
  m.settleTime(p.id, T0);
  assert.equal(m.isFollowing(p.id), true, "a drag RELEASED at the live edge is a return to live, not a pause one pixel short of it");
  assert.equal(boxOf(m.get(p.id)!, T0).t1Ns, T0, "and it pins EXACTLY to the edge");
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

// ——— 4. T-486: the snap-to-live dead zone at the live edge ———
//
// **The user reported this twice.** A 1 px time-pan dropped the pane out of live, so a twitch while
// dragging along *frequency* cost you the live edge; and a drag back toward the top, released as new
// rows appended under the cursor, landed a few pixels short and re-paused. The two are one defect:
// the follow/pause transition had no dead zone, so "at the live edge" was a measure-zero condition
// that no hand and no advancing clock could hit.
//
// **What gains a threshold is the DERIVED STATE, not the pan** — the distinction is the ticket.
// T-456's unthresholded drag is unchanged and asserted unchanged below: the view moves 1:1 with the
// pointer from the first pixel. The threshold is on where the viewport *comes to rest* against the
// live edge, it reads no pointer, and it commands no device — which is why T-407 (a gate on a
// pointer stream, and travel summed across axes) is not the precedent. Its one rule that does carry
// is honoured trivially: the only distance measured here is along one axis, time.
//
// **The unit is DEVICE PIXELS, not rows.** See `settleTime`'s doc for the argument and the cost at
// the zoomed-out extreme; the tests below fix the behaviour at both ends of the zoom.

/** One device pixel of this pane, in ns — the unit the dead zone is actually in. */
const pxNs = (m: PaneModel, id: string) => m.get(id)!.time.spanNs / m.rects().get(id)!.h;
/** How far a pane's newest row sits behind `edge`, in its own pixels. */
const lagPx = (m: PaneModel, id: string, edge: number) => (edge - boxOf(m.get(id)!, edge).t1Ns) / pxNs(m, id);

test("T-486 half one: a 1 px time-pan leaves the pane FOLLOWING, and the release pins it EXACTLY to the edge", () => {
  const m = model();
  const [p] = m.list();
  const px = pxNs(m, p.id);

  // T-456, unchanged and asserted first: the window moves by exactly one pixel's worth on the first
  // pixel. Nothing is suppressed, held back or gated — the view tracks the pointer the whole way.
  m.panTime(p.id, -px);
  assert.equal(boxOf(m.get(p.id)!, T0).t1Ns, T0 - px, "T-456: the pan is unthresholded — the view moves 1:1 from the FIRST pixel");
  // ...and the pane has nonetheless not dropped out of live, because nothing has decided to pause.
  assert.equal(m.isFollowing(p.id), true, "a 1 px twitch must not cost the user the live edge");

  m.settleTime(p.id, T0);
  assert.equal(m.isFollowing(p.id), true);
  assert.deepEqual(Object.keys(m.get(p.id)!.time).sort(), ["live", "spanNs"], "it is REALLY following: no centre left beside `live`");
  assert.equal(boxOf(m.get(p.id)!, T0).t1Ns, T0, "pinned EXACTLY to the edge — a pane resting a pixel short is the bug, not a near-miss of the fix");
  assert.equal(boxOf(m.get(p.id)!, T0 + 30 * S).t1Ns, T0 + 30 * S, "and it grows with the edge again");

  // **Non-vacuity.** The same gesture on a model with no dead zone — which is exactly the code
  // before this ticket — reproduces the report. If the assertions above were true of anything, this
  // one would pass too.
  const flat = model({ holdPx: 0, snapPx: 0 });
  const [q] = flat.list();
  flat.panTime(q.id, -pxNs(flat, q.id));
  flat.settleTime(q.id, T0);
  assert.equal(flat.isFollowing(q.id), false, "control: with no dead zone a 1 px pan still drops the pane out of live — the reported bug");
});

test("T-486 half two: released near the top while rows APPEND under the cursor, the pane returns to live", () => {
  const m = model();
  const [p] = m.list();
  const px = pxNs(m, p.id);

  // Park it forty pixels into the past — a deliberate scrub, committed.
  m.pause(p.id, T0);
  m.panTime(p.id, -40 * px);
  m.settleTime(p.id, T0);
  assert.equal(m.isFollowing(p.id), false, "forty pixels back is a pause, and stays one");

  // Now drag back to what WAS the top. The user's hand lands on the edge they can see; by the time
  // they let go, four more rows have been recorded and the edge has moved out from under them.
  m.panTime(p.id, 40 * px);
  m.settleTime(p.id, T0 + 4 * px);
  assert.equal(m.isFollowing(p.id), true, "the rows that appended DURING the drag must not be what re-pauses the pane");
  assert.equal(boxOf(m.get(p.id)!, T0 + 4 * px).t1Ns, T0 + 4 * px, "and it pins to the edge as it is NOW, not as it was at the last pointer move");

  // The boundary is real and stated, not infinite: a release that lands well outside the snap-back
  // zone is a pause, however plausibly it was meant.
  const n = model();
  const [r] = n.list();
  const rpx = pxNs(n, r.id);
  n.pause(r.id, T0);
  n.panTime(r.id, -40 * rpx);
  n.settleTime(r.id, T0);
  n.panTime(r.id, 40 * rpx);
  n.settleTime(r.id, T0 + 12 * rpx);
  assert.equal(n.isFollowing(r.id), false, "twelve pixels behind is beyond the snap-back zone — the zone has an edge and this is it");
});

test("T-486: a drag BEYOND the zone commits to pause, and the pause is where the pointer left it", () => {
  const m = model();
  const [p] = m.list();
  const px = pxNs(m, p.id);
  m.panTime(p.id, -30 * px);
  assert.equal(m.isFollowing(p.id), false, "past the hold zone the stroke has left follow — before the release, not after it");
  m.settleTime(p.id, T0);
  assert.equal(m.isFollowing(p.id), false);
  assert.equal(boxOf(m.get(p.id)!, T0).t1Ns, T0 - 30 * px, "and the window is exactly where the pointer put it: the dead zone did not move it");
  // Capture is untouched throughout — the pane is frozen, the edge keeps advancing, and the frozen
  // window does not move with it.
  assert.equal(boxOf(m.get(p.id)!, T0 + 60 * S).t1Ns, T0 - 30 * px, "pause freezes the VIEW, not the capture");
});

test("T-486 hysteresis: the SAME release position, opposite states, decided by where the stroke came from", () => {
  const at8 = (from: "live" | "parked" | "returning", opts: Partial<ConstructorParameters<typeof PaneModel>[0]> = {}) => {
    const m = model(opts);
    const [p] = m.list();
    const px = pxNs(m, p.id);
    if (from === "live") m.panTime(p.id, -8 * px);                       // a twitch, never out of the zone
    if (from === "parked") {                                              // already scrubbed; nudge toward live
      m.pause(p.id, T0); m.panTime(p.id, -20 * px); m.settleTime(p.id, T0);
      m.panTime(p.id, 12 * px);
    }
    if (from === "returning") { m.panTime(p.id, -30 * px); m.panTime(p.id, 22 * px); } // out and part-way back
    const released = lagPx(m, p.id, T0);
    m.settleTime(p.id, T0);
    return { released, following: m.isFollowing(p.id) };
  };

  const live = at8("live"), parked = at8("parked"), returning = at8("returning");
  // All three release at the same place. This is the premise the rest of the test is a claim about,
  // so it is asserted rather than assumed.
  for (const [name, r] of [["live", live], ["parked", parked], ["returning", returning]] as const) {
    // To a thousandth of a pixel. Exact equality is not available and is not the claim: at capture-
    // time magnitudes (T0 ≈ 1.7e18 ns) a double's ulp is 256 ns, so three different routes to the
    // same instant land within a few ulps of each other rather than on it.
    assert.ok(Math.abs(r.released - 8) < 1e-3, `${name} should release 8 px behind the edge, got ${r.released}`);
  }
  assert.equal(live.following, true, "a stroke that never left the hold zone was never a pause");
  assert.equal(parked.following, false, "a pane already parked is not yanked back to live by a fine adjustment");
  assert.equal(returning.following, false, "and leaving the hold zone is a ONE-WAY DOOR within a stroke: coming part-way back does not undo it");

  // **Non-vacuity: the asymmetry is load-bearing.** Make the two thresholds equal and two of the
  // three answers flip — the band between them is the only thing distinguishing these cases.
  const sym = { holdPx: 10, snapPx: 10 };
  assert.equal(at8("live", sym).following, true);
  assert.equal(at8("parked", sym).following, true, "control: with a symmetric threshold the parked pane IS yanked to live");
  assert.equal(at8("returning", sym).following, true, "control: and so is the one that came part-way back");
});

test("T-486: NOTHING FLAPS with the live edge advancing — neither state can be left without a gesture", () => {
  // The claim is about the follow state under an ADVANCING edge, so the edge advances throughout.
  // It is asserted from the pane model, never from a chrome readout, whose offset from the edge
  // drifts with wall-clock lag (T-478) and would be a property of the lag rather than of the state.
  const m = model();
  const a = m.list()[0].id;
  const b = m.split(a, "columns")!;
  const px = pxNs(m, a);

  // One pane following, one parked just OUTSIDE the snap-back zone — the worst place to sit.
  m.pause(b, T0);
  m.panTime(b, -7 * px);
  m.settleTime(b, T0);
  assert.equal(m.isFollowing(a), true);
  assert.equal(m.isFollowing(b), false);

  // Now run capture forward for four hundred rows, settling on EVERY one of them — a host that
  // committed per frame rather than per gesture, which is the most flap-prone caller there is.
  let flips = 0;
  let prev = [m.isFollowing(a), m.isFollowing(b)];
  for (let i = 1; i <= 400; i++) {
    const edge = T0 + i * px;
    m.views(edge);
    m.settleTime(a, edge);
    m.settleTime(b, edge);
    const now = [m.isFollowing(a), m.isFollowing(b)];
    if (now[0] !== prev[0] || now[1] !== prev[1]) flips++;
    prev = now;
  }
  assert.equal(flips, 0, "an advancing edge changed a follow state: a pane's state must only be moved by a gesture");
  assert.equal(m.isFollowing(a), true, "the following pane is PINNED — its lag is identically zero, so capture cannot carry it out of the zone");
  assert.equal(m.isFollowing(b), false, "and an advancing edge moves a frozen pane AWAY from the snap zone, never toward it");
  assert.ok(lagPx(m, b, m.lastEdgeNs) > 400, "which is the reason: the frozen pane is now four hundred pixels back");

  // And within a single stroke that jitters across the boundary while the edge advances: the state
  // is MONOTONE — it falls once, at most, and only the release can raise it.
  const j = model();
  const [q] = j.list();
  const jpx = pxNs(j, q.id);
  const seen: boolean[] = [j.isFollowing(q.id)];
  const naive: boolean[] = [true]; // what a thresholds-only rule, with no one-way door, would say
  for (let i = 0; i < 40; i++) {
    const edge = T0 + i * jpx;
    j.views(edge);
    j.panTime(q.id, (i % 2 ? -1 : 1) * 6 * jpx); // a hand hovering right at the boundary
    seen.push(j.isFollowing(q.id));
    naive.push(lagPx(j, q.id, edge) <= 10);
  }
  const transitions = (xs: readonly boolean[]) => xs.filter((v, i) => i > 0 && v !== xs[i - 1]).length;
  assert.equal(transitions(seen), 1, `follow state flapped within one stroke: ${seen.map((v) => (v ? "1" : "0")).join("")}`);
  assert.ok(seen.indexOf(false) > 0 && !seen.slice(seen.indexOf(false)).includes(true), "once a stroke has left follow it stays left until the release");
  // The control: the same stroke, judged by the threshold alone, oscillates — which is what the
  // one-way door exists to prevent, and the reason this test is not vacuous.
  assert.ok(transitions(naive) > 1, `control: a thresholds-only rule must flap here, or this test proves nothing: ${naive.map((v) => (v ? "1" : "0")).join("")}`);
});

test("T-486: a tap, and a frequency-only drag, commit NOTHING — a release is only a decision if a time gesture was in flight", () => {
  const m = model();
  const [p] = m.list();
  // A frozen pane, and a pointer-up that panned no time: `input.ts` settles on every release, so
  // this is the path a plain click takes. It must not snap a scrubbed pane to live.
  m.pause(p.id, T0);
  m.panTime(p.id, -3 * pxNs(m, p.id));
  m.settleTime(p.id, T0);
  const parked = m.get(p.id)!;
  m.settleTime(p.id, T0);
  m.settleTime(p.id, T0 + 5 * S);
  assert.equal(m.isFollowing(p.id), false, "a tap on a paused pane is not a request to go live");
  assert.deepEqual(m.get(p.id)!.time, parked.time, "and it moved nothing");

  // T-484's case: `Preview.drag` pans both axes on every move, so a sideways drag arrives as
  // `panFreq(dx)` then `panTime(0)`. That starts no time gesture, so the release decides nothing —
  // and a following pane stays following for the ordinary reason, not by passing through the zone.
  const n = model();
  const [q] = n.list();
  n.panFreq(q.id, 2e6);
  n.panTime(q.id, 0);
  n.settleTime(q.id, T0);
  assert.equal(n.isFollowing(q.id), true);
  assert.deepEqual(Object.keys(n.get(q.id)!.time).sort(), ["live", "spanNs"], "a sideways drag must not even give the pane a centre");
});

test("T-486 × T-460: a pane that READS as following is a pane whose newest row is at the live edge", () => {
  // T-460's live-edge refresh runs only for a FOLLOWING viewport, which is one of the three things
  // that make it cheap by construction. Widening what counts as following therefore widens what is
  // refreshed — so the budget's guard has to be that "following" never means "far from the edge".
  // That is the invariant asserted here, over the whole gesture vocabulary.
  const m = model();
  const a = m.list()[0].id;
  const b = m.split(a, "columns")!;
  const px = () => pxNs(m, a);
  const viewports = 2;

  const check = (where: string) => {
    let live = 0;
    for (const p of m.list()) {
      if (!m.isFollowing(p.id)) continue;
      live++;
      assert.ok(lagPx(m, p.id, m.lastEdgeNs) <= 10 + 1e-9,
        `${where}: pane ${p.id} reads as following but its newest row is ${lagPx(m, p.id, m.lastEdgeNs).toFixed(1)} px behind the edge`);
    }
    assert.ok(live <= viewports, `${where}: more viewports in the refresh set than exist`);
    return live;
  };

  assert.equal(check("at open"), viewports, "every viewport opens following — so T-486 cannot RAISE the peak refresh set, which was already at its bound");
  m.panTime(a, -px()); check("mid twitch");
  m.settleTime(a, T0); check("after the twitch");
  m.panTime(a, -300 * px()); check("mid scrub");
  assert.equal(m.isFollowing(a), false, "a real scrub leaves the refresh set immediately, not at the release");
  m.settleTime(a, T0); assert.equal(check("after the scrub"), viewports - 1, "a committed pause is a view over data that cannot change, and T-460 must not pay for it");
  m.follow(a); assert.equal(check("after Play"), viewports);
});

test("T-486: the zone is DEVICE PIXELS, so it scales with the zoom rather than with the data", () => {
  // The choice, stated as behaviour. A pixel is a gesture affordance and stays constant to the hand;
  // a row is a claim about data and — after T-484 took the finest tier from a 1 s cell to the display
  // row — changed meaning by ~25× without anything about the hand changing. The test is that the
  // SAME gesture, in pixels, does the same thing at two zooms three decades apart in span.
  for (const spanNs of [2 * S, 2000 * S]) {
    const m = model({ spanNs });
    const [p] = m.list();
    const px = pxNs(m, p.id);
    m.panTime(p.id, -8 * px);
    m.settleTime(p.id, T0);
    assert.equal(m.isFollowing(p.id), true, `an 8 px twitch at a ${spanNs / S} s span must hold live`);

    const n = model({ spanNs });
    const [q] = n.list();
    n.panTime(q.id, -40 * pxNs(n, q.id));
    n.settleTime(q.id, T0);
    assert.equal(n.isFollowing(q.id), false, `a 40 px drag at a ${spanNs / S} s span must be a pause`);
  }
  // The stated cost at the zoomed-out extreme: ten pixels of a very wide window is a long time, so a
  // pane cannot be parked within that much of live there. It is the right trade because at that
  // scale the two windows ARE the same picture — ten pixels of it.
  const wide = model({ spanNs: 86_400 * S });
  const [w] = wide.list();
  assert.ok(pxNs(wide, w.id) * 10 > 900 * S, "on a day-wide viewport the ten-pixel zone really is minutes — stated, not hidden");
});

test("T-486: a model that was never given a viewport has NO dead zone — fail closed, not on a guess", () => {
  // The zone is in pixels, so without a pixel height there is no threshold to be in. Answering
  // anything else would snap panes to live on a guess; the pre-T-486 behaviour is the safe default.
  const m = model({ width: 0, height: 0 });
  const [p] = m.list();
  // A millisecond: far below any pixel at this model's ~25 ms/px, and far ABOVE the 256 ns ulp of a
  // double at capture-time magnitudes (T0 ≈ 1.7e18 ns), which is the real floor on every threshold
  // in this file — a 1 ns scrub of an absolute capture instant does not change the number at all.
  m.panTime(p.id, -1e6);
  m.settleTime(p.id, T0);
  assert.equal(m.isFollowing(p.id), false, "with no viewport, any scrub at all is a scrub");
});
