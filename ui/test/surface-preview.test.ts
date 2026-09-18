// T-450: the host that mounts the unified surface over recorded history.
//
// Four things are asserted here, in the order they matter:
//
//  1. **The requests the client builds.** CLAUDE.md names this as the one class the merge gate does
//     not cover — contract tests prove the *server* serves a route, never that the client asks for
//     the right thing, and T-367's time navigator drew an empty canvas while every suite stayed
//     green because it requested `/api/timeline` with no band. So `probeSurface` is driven through a
//     recording getter and the exact paths are compared.
//  2. **Nothing follows a live edge.** The scope is historical, and `PaneModel`'s *default* window
//     is the following one, so this is asserted rather than assumed: every viewport is frozen at
//     open and two frames at the same edge are the same box.
//  3. **No gesture can reach the radio.** Structural, at the source level: the preview names no
//     device route and imports neither `./retune.ts` nor `../app/centre/view.ts`.
//  4. **The existing UI is unchanged.** Asserted by walking the app's own import graph from
//     `src/app/main.ts` and requiring that it never reaches a T-450 file. "Additive" is a claim
//     about the whole repo; a diff review cannot see it and a convention would drift.

import assert from "node:assert/strict";
import { readFileSync, readdirSync, existsSync } from "node:fs";
import { dirname, join, normalize } from "node:path";
import test from "node:test";

import {
  coverageUrl, fmtShare, latticeSpanHz, observedExtent, openingWindow, orientationNote, surfaceBounds,
} from "../src/surface/bootstrap";
import { GREY } from "../src/surface/cellrule";
import { legendEntries, swatchPixels } from "../src/surface/legend";
import type { Lattice, TileAddr } from "../src/surface/lattice";
import { ControlError } from "../src/controls/client";
import { ORIENT_CELLS, ORIENT_ROWS, SurfacePreview, isBackpressure, probeSurface, wheelAxes, wheelDelta, wheelZoom, zoomFactor, type SurfaceProbe } from "../src/surface/preview";
import { stubGl } from "./surface-glstub";

const S = 1e9;
const LAT: Lattice = { scheme: "view", cells: 8, f0Hz: 6250, t0Ns: 1e9, levelsF: 20, levelsT: 15 };
const T1 = 1_789_300_920;                       // the newest recorded capture second
const T0 = T1 - 3600;                           // the record horizon
const BOUNDS = { f0Hz: 1e6, f1Hz: 6e9, t0Ns: T0 * S, t1Ns: T1 * S };

const flush = () => new Promise((r) => setImmediate(r));

// ——— 1. the bootstrap: backend numbers, and a named fallback when there are none ———

test("the surface's extent comes from backend numbers, and every fallback says it is one", () => {
  const nav = {
    frequency: { ranges_hz: [[1e6, 6e9]] as [number, number][], center_step_hz: 28.6 },
    time: { latest_s: T1 },
  };
  const good = surfaceBounds(nav, T0, LAT);
  assert.deepEqual(good.bounds, BOUNDS);
  assert.equal(good.edgeNs, T1 * S, "the edge is the newest RECORDED instant, never this client's clock");
  assert.match(good.provenance.freq, /api\/navigation/);
  assert.match(good.provenance.time, /oldest_record_s/);

  // A replay reports no front end and a young server holds no tune record. Both fall back — and a
  // bound this client invented must never be indistinguishable from one the server reported.
  const bare = surfaceBounds({ frequency: null, time: null }, null, LAT, 1_700_000_000);
  assert.equal(bare.bounds.f0Hz, 0);
  assert.equal(bare.bounds.f1Hz, latticeSpanHz(LAT));
  assert.match(bare.provenance.freq, /no front end reported/);
  assert.match(bare.provenance.time, /no surviving tune record/);
  assert.match(bare.provenance.time, /no spectrum history reported/);
  assert.equal(bare.edgeNs, 1_700_000_000 * S);
});

test("the orientation coverage map is asked for over the whole surface, at the route's own cap", () => {
  const url = coverageUrl(BOUNDS, ORIENT_CELLS, ORIENT_ROWS);
  const q = new URLSearchParams(url.slice(url.indexOf("?") + 1));
  assert.ok(url.startsWith("/api/coverage?"));
  assert.equal(q.get("f_lo"), "1000000");
  assert.equal(q.get("f_hi"), "6000000000");
  assert.equal(q.get("cells"), String(ORIENT_CELLS));
  assert.equal(q.get("rows"), String(ORIENT_ROWS));
  assert.equal(q.get("t0"), String(T0));
  assert.equal(q.get("t1"), String(T1));
  assert.equal(ORIENT_CELLS * ORIENT_ROWS, 4096, "the route caps `cells` and `rows` at 4096 cells");
});

/** A coverage answer: `nf x nt` cells, `observed` only inside `[f0..f1] x [t0..t1]` (cell indices). */
function coverage(nf: number, nt: number, obs: { f0: number; f1: number; t0: number; t1: number } | null, unknownRows = 0) {
  const cells: { state: string }[] = [];
  for (let t = 0; t < nt; t++) {
    for (let f = 0; f < nf; f++) {
      const inside = obs && f >= obs.f0 && f <= obs.f1 && t >= obs.t0 && t <= obs.t1;
      cells.push({ state: inside ? "observed" : t < unknownRows ? "unknown" : "unobserved" });
    }
  }
  return {
    grid: { cells: nf, rows: nt, f_lo_hz: 1e6, f_cell_hz: 1e6, t0_s: T0, t_cell_s: 1 },
    any: { cells },
  };
}

test("observed extent counts the three coverage states separately and boxes only the observed ones", () => {
  const c = observedExtent(coverage(10, 10, { f0: 2, f1: 3, t0: 4, t1: 5 }, 2));
  assert.equal(c.observed, 4);
  assert.equal(c.unknown, 20, "`unknown` is a row property: two whole rows past the record horizon");
  assert.equal(c.unobserved, 100 - 4 - 20);
  assert.equal(c.observed + c.unknown + c.unobserved, c.total, "the three are never summed for you elsewhere");
  // The box covers the observed cells and nothing else: `unknown` is not evidence anything was
  // measured, so opening the view on it would be opening on a guess.
  assert.deepEqual(c.box, { f0Hz: 3e6, f1Hz: 5e6, t0Ns: (T0 + 4) * S, t1Ns: (T0 + 6) * S });

  const none = observedExtent(coverage(4, 4, null));
  assert.equal(none.observed, 0);
  assert.equal(none.box, null);
  assert.equal(observedExtent(null).total, 0, "a missing map is not an empty map");
});

test("the view opens ON observed coverage with a margin, so the edge of coverage is on screen", () => {
  const c = observedExtent(coverage(10, 10, { f0: 2, f1: 3, t0: 4, t1: 5 }));
  const o = openingWindow(BOUNDS, c.box);
  assert.equal(o.onCoverage, true);
  assert.equal(o.freq.centerHz, 4e6, "centred on the observed band");
  assert.ok(o.freq.spanHz > 2e6 && o.freq.spanHz < 3e6, "wider than coverage, so its edge is visible");
  assert.equal(o.centerNs, (T0 + 5) * S);
  // With nothing observed there is nowhere honest to open but the whole surface.
  const empty = openingWindow(BOUNDS, null);
  assert.equal(empty.onCoverage, false);
  assert.equal(empty.freq.spanHz, BOUNDS.f1Hz - BOUNDS.f0Hz);
});

test("a nearly-empty first screen arrives already explained, with a number rather than '0 %'", () => {
  // T-437's measurement: 99.4 % grey before history accumulates. The defect available here is
  // failing to SAY so, so the note must carry the share and must not round a real fraction to zero.
  const c = observedExtent(coverage(128, 32, { f0: 0, f1: 0, t0: 0, t1: 24 }));
  assert.ok(c.observed > 0 && c.observed / c.total < 0.01);
  const note = orientationNote(c, openingWindow(BOUNDS, c.box));
  assert.match(note, /0\.6 % of this surface was ever sampled/, "the share is stated, not rounded away");
  assert.match(note, /grey means nothing ever looked there/i);
  assert.equal(fmtShare(6, 100_000), "0.006 %");
  assert.equal(fmtShare(0, 100), "0 %");

  // Nothing observed anywhere: the note must still explain the grey rather than read as a failure.
  const blank = orientationNote(observedExtent(coverage(8, 8, null)), openingWindow(BOUNDS, null));
  assert.match(blank, /not a loading state and not a failure/);
});

// ——— 2. the requests the client actually builds (the T-367 guard) ———

test("probeSurface asks exactly four read-only routes, in dependency order", async () => {
  const asked: string[] = [];
  const surfaceWide = observedExtent(coverage(ORIENT_CELLS, ORIENT_ROWS, { f0: 10, f1: 20, t0: 28, t1: 31 }));
  const p = await probeSurface(async (path) => {
    asked.push(path);
    if (path.startsWith("/api/tiles")) return tileProbeResponse();
    if (path === "/api/navigation") return { frequency: { ranges_hz: [[1e6, 6e9]], center_step_hz: 28.6 }, time: { latest_s: T1 } };
    // The coarse map first, then the refinement over the box it returned — the same route, the same
    // question, at the resolution the first answer made available.
    return asked.filter((a) => a.startsWith("/api/coverage")).length === 1
      ? coverage(ORIENT_CELLS, ORIENT_ROWS, { f0: 10, f1: 20, t0: 28, t1: 31 })
      : coverage(ORIENT_CELLS, ORIENT_ROWS, { f0: 60, f1: 62, t0: 0, t1: 31 });
  });
  assert.deepEqual(asked, [
    "/api/tiles?level_f=0&level_t=0&f_index=0&t_index=0&cells=8",
    "/api/navigation",
    coverageUrl({ f0Hz: 1e6, f1Hz: 6e9, t0Ns: T0 * S, t1Ns: T1 * S }, ORIENT_CELLS, ORIENT_ROWS),
    coverageUrl(surfaceWide.box!, ORIENT_CELLS, ORIENT_ROWS),
  ]);
  assert.equal(p.lattice.cells, 256, "the probe is cheap (8 cells); the surface renders at the route's default");
  assert.equal(p.lattice.f0Hz, 6250, "level 0 is read off the answer, never chosen here");
  assert.equal(p.opening.onCoverage, true);
  assert.deepEqual(p.degraded, []);

  // The refinement decides where to OPEN; the note's share stays the surface-wide one, because it
  // is a statement about the whole surface and the refined pass measures inside coverage.
  assert.deepEqual(p.census, surfaceWide);
  assert.match(p.note, new RegExp(`${surfaceWide.observed} of ${surfaceWide.total} coverage cells`));
  assert.ok(p.opening.freq.spanHz < surfaceWide.box!.f1Hz - surfaceWide.box!.f0Hz,
    "the refinement did not narrow the opening window: a coarse cell is 51.2 MHz on a 6.5 GHz surface");
});

test("a refinement that finds nothing keeps the coarse box: an observed cell has something in it", async () => {
  let n = 0;
  const p = await probeSurface(async (path) => {
    if (path.startsWith("/api/tiles")) return tileProbeResponse();
    if (path === "/api/navigation") return { frequency: { ranges_hz: [[1e6, 6e9]], center_step_hz: 28.6 }, time: { latest_s: T1 } };
    return ++n === 1 ? coverage(ORIENT_CELLS, ORIENT_ROWS, { f0: 10, f1: 20, t0: 28, t1: 31 }) : coverage(ORIENT_CELLS, ORIENT_ROWS, null);
  });
  assert.equal(p.opening.onCoverage, true, "an empty refinement must not send the view to nowhere");
  assert.deepEqual(p.degraded, []);
});

test("a route that fails degrades visibly: the fallback is recorded, never silent", async () => {
  const p = await probeSurface(async (path) => {
    if (path.startsWith("/api/tiles")) return tileProbeResponse();
    throw new Error("boom");
  });
  assert.equal(p.degraded.length, 2);
  assert.match(p.degraded[0], /api\/navigation failed/);
  assert.match(p.degraded[1], /api\/coverage failed/);
  assert.equal(p.opening.onCoverage, false, "with no coverage map there is nowhere honest to open but the whole surface");
  assert.match(p.note, /No coverage map was returned/);
});

test("without a tile answer there is no lattice, and the preview refuses rather than guessing one", async () => {
  await assert.rejects(() => probeSurface(async () => { throw new Error("no history"); }), /no history/);
});

// ——— T-454: the probe is the one tile fetch outside TileCache, and it is what reached the user ———

const REFUSAL = "too many tile reads in flight (limit 4): tile production takes the history lock, " +
  "so the cap is ingest backpressure, not a queue — cancel tiles whose viewport you have left and " +
  "retry the ones you still want";
const refused = () => new ControlError(503, "http_503", REFUSAL);

test("the route's backpressure is answered by ASKING AGAIN, never by a banner quoting it", async () => {
  // This is the defect the user hit: `probeSurface` is the one tile request that does not go
  // through `TileCache`, so it had neither the cap, the cancellation nor the backoff — and a
  // *shared* budget is exactly what refuses a new arrival. Reloading the page mid-drag was enough,
  // because the previous page's abandoned reads still held all four of the route's slots.
  let refusals = 2;
  const asked: string[] = [];
  const slept: number[] = [];
  const p = await probeSurface(
    async (path) => {
      asked.push(path);
      if (path.startsWith("/api/tiles")) {
        if (refusals-- > 0) throw refused();
        return tileProbeResponse();
      }
      if (path === "/api/navigation") return { frequency: { ranges_hz: [[1e6, 6e9]], center_step_hz: 28.6 }, time: { latest_s: T1 } };
      return coverage(ORIENT_CELLS, ORIENT_ROWS, { f0: 10, f1: 20, t0: 28, t1: 31 });
    },
    undefined,
    { backoffMs: 4, sleep: async (ms) => { slept.push(ms); } },
  );
  const probePath = "/api/tiles?level_f=0&level_t=0&f_index=0&t_index=0&cells=8";
  assert.deepEqual(asked.slice(0, 3), [probePath, probePath, probePath],
    "the request the client builds on a refusal is the SAME request, again");
  assert.deepEqual(slept, [4, 8], "and it waits longer each time rather than re-asking on one cadence");
  assert.deepEqual(p.degraded, [], "a refusal that was answered is not a degradation");
  assert.equal(p.lattice.cells, 256, "and the surface opens exactly as if it had never been refused");
  assert.deepEqual(p.requests.slice(0, 3), [probePath, probePath, probePath],
    "every attempt is on the record: the page shows what it actually asked for");
});

test("only backpressure is retried — a real error is still an error, and at once", async () => {
  let n = 0;
  await assert.rejects(
    () => probeSurface(async () => { n++; throw new ControlError(404, "not_found", "no such node"); },
      undefined, { sleep: async () => {} }),
    /no such node/,
  );
  assert.equal(n, 1, "retrying a 404 would only make a broken surface slower to say so");
});

test("an unrelenting refusal is bounded, and is reported AS backpressure", async () => {
  let n = 0;
  await assert.rejects(
    () => probeSurface(async () => { n++; throw refused(); }, undefined, { retries: 3, sleep: async () => {} }),
    (e: unknown) => {
      // The page branches on this, and says "the route is busy" instead of quoting the route at the
      // user. `preview-main.ts` is the only consumer, and this is the predicate it uses.
      assert.ok(isBackpressure(e), "a 503 must stay recognisable as backpressure after the retries");
      return true;
    },
  );
  assert.equal(n, 4, "bounded: one attempt plus the retries, never an unbounded loop against a busy lock");
  assert.equal(isBackpressure(new ControlError(500, "internal", "boom")), false);
  assert.equal(isBackpressure(new Error("boom")), false);
});

// ——— 3. the mounted surface ———

function tileProbeResponse() {
  const n = 8 * 8;
  return {
    key: { device: "any", scheme: "view", level_f: 0, level_t: 0, f_index: 0, t_index: 0, cells: 8 },
    extent: { nf: 8, nt: 8 },
    axes: { frequency: { levels: 20, cell_hz: 6250 }, time: { levels: 15, cell_s: 1 } },
    grid: { nf: 8, nt: 8, max_db: Array<number>(n).fill(-90) },
    coverage: { any: { cells: Array.from({ length: n }, () => ({ state: "observed" })) }, horizon: { oldest_record_s: T0 } },
    resolution: { source: "spectrum-history", answered: { level: 0 } },
  };
}

function probeFor(opening = { freq: { centerHz: 100.8e6, spanHz: 2.4e6 }, centerNs: (T1 - 30) * S, spanNs: 20 * S, onCoverage: true }): SurfaceProbe {
  return {
    lattice: LAT,
    origin: {
      bounds: BOUNDS,
      edgeNs: T1 * S,
      provenance: { freq: "test", time: "test" },
    },
    census: { observed: 4, unobserved: 4092, unknown: 0, total: 4096, box: null },
    opening,
    note: "test",
    requests: [],
    degraded: [],
  };
}

function harness() {
  const g = stubGl(1200, 600);
  const asked: TileAddr[] = [];
  const fetchFn = async (url: string) => {
    asked.push(parseTileUrl(url));
    return {
      ok: true, status: 200, statusText: "OK",
      json: async () => tileAnswer(parseTileUrl(url)),
    };
  };
  const preview = new SurfacePreview({
    canvas: g.canvas, probe: probeFor(), token: "t", fetchFn, chrome: null, minimapPx: 120,
  });
  return { g, preview, asked };
}

function parseTileUrl(url: string): TileAddr {
  const q = new URLSearchParams(url.slice(url.indexOf("?") + 1));
  const n = (k: string, d: number) => (q.has(k) ? Number(q.get(k)) : d);
  return {
    device: q.get("device") ?? "any", scheme: q.get("scheme") ?? "view",
    levelF: n("level_f", 0), levelT: n("level_t", 0),
    fIndex: n("f_index", 0), tIndex: n("t_index", 0), cells: n("cells", 256),
  };
}

function tileAnswer(a: TileAddr) {
  const n = a.cells * a.cells;
  return {
    key: { device: a.device, scheme: a.scheme, level_f: a.levelF, level_t: a.levelT, f_index: a.fIndex, t_index: a.tIndex, cells: a.cells },
    extent: { nf: a.cells, nt: a.cells },
    axes: { frequency: { levels: LAT.levelsF, cell_hz: LAT.f0Hz * 2 ** a.levelF }, time: { levels: LAT.levelsT, cell_s: (LAT.t0Ns * 2 ** a.levelT) / 1e9 } },
    grid: { nf: a.cells, nt: a.cells, max_db: Array<number>(n).fill(-90), range_db: { lo: -100, hi: -60 } },
    coverage: { any: { cells: Array.from({ length: n }, () => ({ state: "observed" })) } },
    resolution: { source: "spectrum-history", answered: { level: a.levelF } },
  };
}

test("every viewport is FROZEN at open: this preview has no live edge to follow", () => {
  const { preview } = harness();
  for (const p of preview.view.panes.list()) {
    assert.equal(p.time.live, false, `pane ${p.id} is following a growing edge this view does not read`);
  }
  assert.equal(preview.view.minimap.following, false);
  // And the edge itself never moves: two frames, the same box, byte for byte.
  const a = preview.frame();
  const b = preview.frame();
  assert.deepEqual(a.views[0].box, b.views[0].box);
  assert.equal(a.edgeNs, T1 * S);
});

test("no live-edge mark is drawn: the active-capture list is never even read here", () => {
  const { preview } = harness();
  const f = preview.frame();
  assert.equal(f.quads.filter((q) => q.kind === "live-segment").length, 0);
  assert.ok(f.quads.length > 0, "…but the pane rectangles ARE drawn: the map is a viewport on this surface");
  assert.ok(f.quads.every((q) => q.kind === "pane-outline"));
});

test("the minimap shows one rectangle per pane, and follows a pane that moves", () => {
  const { preview } = harness();
  preview.frame();
  preview.split("columns");
  const before = preview.frame();
  const ids = new Set(before.quads.map((q) => q.id));
  assert.equal(ids.size, 2, "two panes, two rectangles on the map");
  // Move one pane; its rectangle moves and the other's does not.
  const [a, b] = preview.view.panes.list().map((p) => p.id);
  preview.view.panes.panFreq(a, 1e9);
  const after = preview.frame();
  const x = (q: readonly { clip: readonly number[]; id: string }[], id: string) => q.filter((o) => o.id === id).map((o) => o.clip[0]);
  assert.notDeepEqual(x(after.quads, a), x(before.quads, a));
  assert.deepEqual(x(after.quads, b), x(before.quads, b));
});

test("pan and zoom work on BOTH axes, at levels that move INDEPENDENTLY", () => {
  const { preview } = harness();
  const id = preview.activePane;
  const base = preview.frame().reports.find((r) => r.id === id)!;

  // ALT is the time axis (T-456), and it leaves the frequency level alone.
  preview.wheel(id, { x: 600, y: 400 }, 64, wheelAxes({ altKey: true }));
  const timeOnly = preview.frame().reports.find((r) => r.id === id)!;
  assert.ok(timeOnly.levelT > base.levelT, "time did not zoom");
  assert.equal(timeOnly.levelF, base.levelF, "an alt wheel moved the FREQUENCY level: the axes are welded again");

  // Shift is the frequency axis, and it leaves the time level alone.
  preview.wheel(id, { x: 600, y: 400 }, 8, wheelAxes({ shiftKey: true }));
  const freqToo = preview.frame().reports.find((r) => r.id === id)!;
  assert.ok(freqToo.levelF > timeOnly.levelF);
  assert.equal(freqToo.levelT, timeOnly.levelT);

  assert.ok(zoomFactor(100) > 1 && zoomFactor(-100) < 1, "scrolling down zooms out");
  assert.equal(zoomFactor(1e9), 4, "a flung trackpad cannot zoom a whole pyramid in one event");
});

test("T-456: a plain wheel is a UNIFORM zoom that still leaves the two axes independently levelled", () => {
  const { preview } = harness();
  const id = preview.activePane;
  const before = preview.frame();
  const b = before.views.find((v) => v.id === id)!.box;
  const bl = before.reports.find((r) => r.id === id)!;

  // One factor, both axes, anchored on the cursor. `64` is a big step so both levels have to move.
  preview.wheel(id, { x: 600, y: 400 }, 64, wheelAxes({}));
  const after = preview.frame();
  const a = after.views.find((v) => v.id === id)!.box;
  const al = after.reports.find((r) => r.id === id)!;
  assert.ok(a.f1Hz - a.f0Hz > b.f1Hz - b.f0Hz, "a plain wheel must zoom FREQUENCY as well as time");
  assert.ok(a.t1Ns - a.t0Ns > b.t1Ns - b.t0Ns, "a plain wheel must zoom TIME as well as frequency");
  assert.ok(al.levelF > bl.levelF && al.levelT > bl.levelT, "both levels should have moved on a 64x step");

  // **The gesture is uniform; the LEVELS are not welded by it.** The two axes are different physical
  // quantities with different cell sizes, so one factor lands them on different level indices — and
  // that is the property T-434 de-welded for. A uniform gesture that collapsed them to one level
  // would make `levelF === levelT` here, whatever the surface looked like.
  assert.notEqual(al.levelF, al.levelT,
    "one uniform gesture produced ONE level for both axes: the levels have been re-welded");
  // …and each axis is still clamped on its own: zooming far out pins frequency to the surface's
  // whole extent while time is pinned to the record, two independent floors reached separately.
  preview.wheel(id, { x: 600, y: 400 }, 1e6, wheelAxes({}));
  const w = preview.frame().views.find((v) => v.id === id)!.box;
  assert.equal(w.f1Hz - w.f0Hz, BOUNDS.f1Hz - BOUNDS.f0Hz);
  assert.equal(w.t1Ns - w.t0Ns, BOUNDS.t1Ns - BOUNDS.t0Ns);
});

test("T-456: the modifier table, and the shift-held wheel that arrives as a HORIZONTAL scroll", () => {
  // Plain — including a trackpad pinch, which Chrome and Safari deliver as ctrl+wheel — is uniform.
  assert.deepEqual(wheelAxes({}), { freq: true, time: true });
  assert.deepEqual(wheelAxes({ ctrlKey: true }), { freq: true, time: true },
    "ctrl is deliberately unbound: a pinch arrives as ctrl+wheel and must zoom both axes");
  assert.deepEqual(wheelAxes({ metaKey: true }), { freq: true, time: true });
  assert.deepEqual(wheelAxes({ shiftKey: true }), { freq: true, time: false });
  assert.deepEqual(wheelAxes({ altKey: true }), { freq: false, time: true });
  // Both modifiers names both axes, which is the uniform gesture — not a fourth, undefined one.
  assert.deepEqual(wheelAxes({ shiftKey: true, altKey: true }), { freq: true, time: true });

  // macOS delivers a shift-held wheel as a horizontal scroll: the scroll is in `deltaX` and
  // `deltaY` is 0. Reading `deltaY` alone would make the frequency axis inert on a real Mac.
  assert.equal(wheelDelta({ deltaX: -240, deltaY: 0, shiftKey: true }), -240);
  assert.equal(wheelDelta({ deltaX: 0, deltaY: -240, shiftKey: true }), -240,
    "…and a shift wheel that did NOT get swapped must still work");
  // The dominant delta, never the sum: T-407's `clientX + clientY` mistake, not repeated.
  assert.equal(wheelDelta({ deltaX: -240, deltaY: -30, shiftKey: true }), -240);
  assert.equal(wheelDelta({ deltaX: -30, deltaY: -240, shiftKey: true }), -240);
  // Without shift, a horizontal swipe is a scroll and not a zoom, so `deltaX` is ignored outright.
  assert.equal(wheelDelta({ deltaX: -240, deltaY: 0 }), 0);
  assert.equal(zoomFactor(wheelDelta({ deltaX: -240, deltaY: 0 })), 1, "…which is a zoom of exactly nothing");

  // `wheelZoom` is the one call a host makes, so the whole gesture is decided in one place.
  const mac = wheelZoom({ deltaX: -240, deltaY: 0, deltaMode: 0, shiftKey: true });
  assert.deepEqual(mac.axes, { freq: true, time: false });
  assert.ok(mac.factor < 1, "a macOS shift+wheel must zoom IN on frequency, not stand still");
  assert.deepEqual(wheelZoom({ deltaY: -240 }).axes, { freq: true, time: true });
  assert.deepEqual(wheelZoom({ deltaY: -240, altKey: true }).axes, { freq: false, time: true });
  // Line and page deltaModes are normalised the same way for every host.
  assert.equal(wheelZoom({ deltaY: 15, deltaMode: 1 }).factor, zoomFactor(15, 1));
});

test("a drag keeps the data under the pointer: both axes, both signs", () => {
  const { preview } = harness();
  const id = preview.activePane;
  const box0 = preview.frame().views.find((v) => v.id === id)!.box;
  const w = preview.view.panes.rects().get(id)!.w;
  // Drag right by a quarter of the pane: the window walks a quarter-span DOWN in frequency, so the
  // energy that was under the pointer is still under it.
  preview.drag(id, w / 4, 0);
  const box1 = preview.frame().views.find((v) => v.id === id)!.box;
  const span = box0.f1Hz - box0.f0Hz;
  assert.ok(Math.abs((box0.f0Hz - box1.f0Hz) - span / 4) < span * 1e-6, "a drag right must reveal LOWER frequencies");

  // Drag toward the top of the drawing buffer (GL y up): the window walks BACK in time.
  const h = preview.view.panes.rects().get(id)!.h;
  preview.drag(id, 0, h / 4);
  const box2 = preview.frame().views.find((v) => v.id === id)!.box;
  assert.ok(box2.t1Ns < box1.t1Ns, "dragging up must show OLDER data, not newer");
});

test("the pane's ground is PENDING before a tile arrives, and a tile is fetched for it", async () => {
  const h = harness();
  h.preview.frame();
  await flush();
  assert.ok(h.asked.length > 0, "the mount never asked for a tile: nothing is being read from the pyramid");
  assert.ok(h.asked.every((a) => a.scheme === "view" && a.cells === LAT.cells));
  h.g.reset();
  h.preview.frame();
  // The pane clear is PENDING (a memory fact), never the grey (a claim about the radio).
  const clears = h.g.clears();
  assert.ok(clears.length >= 2, "the backdrop and at least one pane ground are cleared each frame");
  const grey = GREY.map((v) => Math.round(v * 1000) / 1000);
  for (const c of clears.slice(1)) {
    assert.notDeepEqual(c.args.slice(0, 3).map((v) => Math.round(v * 1000) / 1000), grey,
      "a pane ground cleared to THE grey would spell 'the radio never looked' for a tile that merely has not loaded");
  }
});

test("fit-to-coverage returns the active pane to the opened window after a pan into the grey", () => {
  const { preview } = harness();
  const id = preview.activePane;
  preview.frame();
  preview.view.panes.panFreq(id, 3e9);
  preview.view.panes.zoomFreq(id, 100);
  preview.fitToCoverage();
  const p = preview.view.panes.get(id)!;
  assert.ok(Math.abs(p.freq.centerHz - 100.8e6) < 1, "the pane did not come back to the observed region");
  assert.ok(Math.abs(p.freq.spanHz - 2.4e6) < 1);
  assert.equal(p.time.live, false, "coming back must not re-pin the pane to an edge this view does not read");
});

test("split gives two viewports onto ONE surface, and the last pane never closes", () => {
  const { preview } = harness();
  preview.frame();
  const first = preview.activePane;
  preview.split("rows");
  assert.equal(preview.view.panes.count, 2);
  const [a, b] = preview.view.panes.list();
  assert.deepEqual(a.freq, b.freq, "a split shows the IDENTICAL box until one is moved");
  preview.closeActive();
  assert.equal(preview.view.panes.count, 1);
  assert.equal(preview.activePane, first);
  preview.closeActive();
  assert.equal(preview.view.panes.count, 1, "with no viewport there is nowhere to look from");
});

// ——— the legend is painted by the rule it describes ———

test("the legend's swatches come from cellPixel, so the key cannot drift from the shader", () => {
  const entries = legendEntries();
  const keys = entries.map((e) => e.key);
  for (const k of ["unobserved", "observed", "no-level", "unknown", "awaiting", "tier-history", "tier-survey", "fallback", "pending"]) {
    assert.ok(keys.includes(k), `the key omits ${k}`);
  }
  // THE grey is in the key, exactly once, and reads as a claim rather than as a loading state.
  const greyRow = entries.find((e) => e.key === "unobserved")!;
  assert.deepEqual(greyRow.pixel({ x: 3, y: 3 }, 0.5), [0.155, 0.16, 0.18]);
  assert.match(greyRow.note, /not a loading state/);
  assert.equal(entries.filter((e) => e.note.includes("only grey")).length, 1);
  // Every swatch is distinguishable from every other at the same pixel — five states, five marks.
  const at = (i: number) => swatchPixels(entries[i], 8, 8).slice(0, 4).join(",");
  const seen = new Set(entries.map((_, i) => at(i)));
  assert.ok(seen.size >= 6, "marks that share a first pixel AND a pattern would be one mark wearing two names");
});

// ——— 4. the architectural claims ———

const SRC = (f: string) => readFileSync(f, "utf8");
const T450_FILES = ["src/surface/preview.ts", "src/surface/preview-main.ts", "src/surface/bootstrap.ts", "src/surface/legend.ts"];

test("no gesture can reach the radio: the preview names no device route and never imports retune", () => {
  const DEVICE_ROUTES = ["/api/control/center", "/api/control/rate", "/api/control/gains", "/api/control/bias_tee", "/api/control/baseband_filter"];
  for (const f of T450_FILES) {
    const src = SRC(f);
    for (const r of DEVICE_ROUTES) assert.ok(!src.includes(`"${r}"`), `${f} names the device route ${r}`);
    assert.ok(!/from "\.\/retune"|from "\.\.\/app\/centre\/view"/.test(src),
      `${f} imports a module that can command the front end; a pan here is a pan`);
  }
  // And the routes it DOES name are the three read-only ones, so a fourth has to be argued for.
  const named = new Set<string>();
  // Both quote styles: a template literal is how a route with a query string is built.
  for (const f of T450_FILES) for (const m of SRC(f).matchAll(/["`](\/api\/[a-z/]+)/g)) named.add(m[1]);
  assert.deepEqual([...named].sort(), ["/api/coverage", "/api/navigation"],
    "`/api/tiles` is built by lattice.ts's tileUrl, which is where the tile address lives");
});

/** Every file the app's entry point can reach, following static and dynamic relative imports. */
function importGraph(entry: string): Set<string> {
  const seen = new Set<string>();
  const stack = [normalize(entry)];
  while (stack.length) {
    const f = stack.pop()!;
    if (seen.has(f) || !existsSync(f)) continue;
    seen.add(f);
    for (const m of SRC(f).matchAll(/(?:from|import)\s*\(?\s*"(\.[^"]+)"/g)) {
      const base = normalize(join(dirname(f), m[1]));
      for (const c of [base, `${base}.ts`, join(base, "index.ts")]) {
        if (existsSync(c) && c.endsWith(".ts")) { stack.push(c); break; }
      }
    }
  }
  return seen;
}

test("THE ADDITIVE CLAIM: the app cannot reach the preview, and the preview is not on its bundle", () => {
  const app = importGraph("src/app/main.ts");
  assert.ok(app.has("src/app/shell.ts"), "the graph walk found nothing: the assertion below would pass vacuously");
  assert.ok(app.size > 30, `the graph walk found only ${app.size} files; it is not walking the app`);
  for (const f of T450_FILES) {
    assert.ok(!app.has(normalize(f)), `${f} is reachable from the app's entry point — this ticket is additive`);
  }
  // The preview has its own entry, its own page and its own bundle: the app's build line is
  // untouched and its output cannot change because of anything here.
  const pkg = JSON.parse(SRC("package.json")) as { scripts: Record<string, string> };
  assert.match(pkg.scripts["build:surface"], /preview-main\.ts.*--outfile=dist\/surface\.js/s);
  assert.ok(!pkg.scripts.build.includes("preview-main"),
    "the preview must not join the app's --splitting entry list: shared chunks would change app.js");
  assert.ok(existsSync("src/surface/preview.html"));
  assert.ok(!SRC("src/app/index.html").includes("surface.js"), "the app's page must not load the preview's bundle");
});

/**
 * **T-456: whoever hosts this surface, the wheel means the same thing.**
 *
 * This is a claim about *every* host, not about the one that exists today, and it is written as a
 * source assertion for that reason: the cutover adds a second host, and two hosts each doing their
 * own wheel arithmetic is T-412's wheel-zoom mismatch rebuilt from parts. The failure mode is
 * silent — `zoomFactor(e.deltaY, e.deltaMode)` compiles, runs, and makes shift+wheel inert on
 * macOS, where a shift-held wheel arrives in `deltaX` — so nothing else in this suite would catch
 * it. The same goes for `preventDefault`: a host that forgets it hands ctrl+wheel to the browser's
 * page zoom, which is the one thing a `{ passive: false }` listener exists to stop.
 */
test("T-456: every wheel listener on the surface goes through wheelZoom, and preventDefaults", () => {
  const dir = "src/surface";
  const hosts = readdirSync(dir)
    .filter((f) => f.endsWith(".ts"))
    .map((f) => [join(dir, f), SRC(join(dir, f))] as const)
    .filter(([, src]) => /addEventListener\(\s*["']wheel["']/.test(src));
  assert.ok(hosts.length > 0, "no wheel listener was found at all, so this guard is asserting nothing");
  for (const [file, src] of hosts) {
    assert.match(src, /wheelZoom\(/,
      `${file} registers a wheel listener without calling wheelZoom(): the gesture's meaning must ` +
      "come from preview.ts, or this host and the other one will disagree about what a wheel does");
    assert.ok(!/\be\.deltaY\b|\be\.deltaX\b|\be\.shiftKey\b|\be\.altKey\b/.test(src),
      `${file} reads the wheel's deltas or modifier bits itself — that decision belongs to ` +
      "wheelZoom(), which is the only copy of it");
    assert.match(src, /passive:\s*false/,
      `${file}'s wheel listener is not registered { passive: false }, so its preventDefault cannot bind`);
    assert.match(src, /preventDefault\(\)/,
      `${file} does not preventDefault its wheel: ctrl+wheel and cmd+wheel would zoom the PAGE`);
  }
});

test("the preview reaches the renderer it was built to mount, rather than a copy of it", () => {
  const graph = importGraph("src/surface/preview-main.ts");
  for (const f of ["src/surface/surface.ts", "src/surface/panes.ts", "src/surface/minimap.ts",
    "src/surface/chrome.ts", "src/surface/view.ts", "src/surface/cellrule.ts", "src/surface/tilecache.ts",
    "src/surface/tile.ts", "src/surface/lattice.ts", "src/surface/overlay.ts"]) {
    assert.ok(graph.has(normalize(f)), `${f} is not mounted: something here is a second implementation`);
  }
  assert.ok(!graph.has(normalize("src/surface/retune.ts")), "T-444's device action is unmounted in a preview");
});
