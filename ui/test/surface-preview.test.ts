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
//  4. **~~The existing UI is unchanged.~~ INVERTED BY T-445.** T-450 was additive, and asserted it
//     by walking the app's import graph from `src/app/main.ts` and requiring that it never reached
//     a T-450 file. The cutover is the ticket that was waiting for that preview to be looked at, so
//     the claim it must now hold up is the opposite one — **the app mounts this same host**, and
//     there is not a second copy of it under `src/app/`. The assertion was inverted rather than
//     deleted (T-347's precedent): the guarantee it protected — one host, not two — is the whole
//     anti-divergence argument, and it needs a guard on the other side of the cutover more than it
//     needed one before.

import assert from "node:assert/strict";
import { readFileSync, readdirSync, existsSync } from "node:fs";
import { dirname, join, normalize } from "node:path";
import test from "node:test";

import {
  coverageUrl, fmtShare, latticeSpanHz, observedExtent, openingWindow, orientationNote, recentObservedExtent,
  surfaceBounds,
} from "../src/surface/bootstrap";
import { GREY } from "../src/surface/cellrule";
import { legendEntries, swatchPixels } from "../src/surface/legend";
import { keyOf, type Lattice, type TileAddr } from "../src/surface/lattice";
import { ControlError } from "../src/controls/client";
import { MAX_BACKOFF_MS, ORIENT_CELLS, ORIENT_ROWS, RETRY_BUDGET_MS, SurfacePreview, isBackpressure, probeSurface, retriesForBudget, wheelAxes, wheelDelta, wheelZoom, zoomFactor, type SurfaceProbe } from "../src/surface/preview";
import { parseKey } from "../src/surface/tilecache";
import type { RowOpener } from "../src/surface/rowfeed";
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

/** A coverage answer with TWO disjoint observed rectangles, so the whole-history census's bounding
 * box spans the empty gap between them the way a session holding two different tunings would. */
function twoBandCoverage(nf: number, nt: number, old_: { f0: number; f1: number; t1: number },
  recent: { f0: number; f1: number; t0: number; t1: number }) {
  const cells: { state: string }[] = [];
  for (let t = 0; t < nt; t++) {
    for (let f = 0; f < nf; f++) {
      const inOld = f >= old_.f0 && f <= old_.f1 && t <= old_.t1;
      const inRecent = f >= recent.f0 && f <= recent.f1 && t >= recent.t0 && t <= recent.t1;
      cells.push({ state: inOld || inRecent ? "observed" : "unobserved" });
    }
  }
  return {
    grid: { cells: nf, rows: nt, f_lo_hz: 1e6, f_cell_hz: 1e6, t0_s: T0, t_cell_s: 1 },
    any: { cells },
  };
}

test("T-955: a session holding two disjoint tunings — recentObservedExtent boxes only the one it holds NOW", () => {
  // The measured shape: 100.7 MHz (index ~100) tuned for a long stretch, then retuned to 10.5 MHz
  // (index ~10) five rows ago. `observedExtent`'s bounding box spans both, plus the untuned gap
  // between them — the "100–1100 MHz" bug. `recentObservedExtent` must box only the recent one.
  const cov = twoBandCoverage(ORIENT_CELLS, ORIENT_ROWS, { f0: 100, f1: 102, t1: 20 }, { f0: 10, f1: 12, t0: 29, t1: 31 });
  const whole = observedExtent(cov);
  assert.ok(whole.box && whole.box.f0Hz <= 11e6 && whole.box.f1Hz >= 103e6,
    "the whole-history census spans the gap between the two tunings — this IS the bug, reproduced");

  const recent = recentObservedExtent(cov);
  assert.ok(recent.box, "the recent rows are not empty");
  assert.ok(recent.box!.f0Hz >= 9e6 && recent.box!.f1Hz <= 14e6,
    `recentObservedExtent boxed the OLD band too: ${JSON.stringify(recent.box)}`);
  assert.ok(recent.box!.f1Hz <= whole.box!.f1Hz, "recent is never wider than the whole-history census");
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

test("probeSurface asks exactly five read-only routes, in dependency order", async () => {
  const asked: string[] = [];
  const surfaceWide = observedExtent(coverage(ORIENT_CELLS, ORIENT_ROWS, { f0: 10, f1: 20, t0: 28, t1: 31 }));
  const p = await probeSurface(async (path) => {
    asked.push(path);
    if (path.startsWith("/api/tiles")) return tileProbeResponse(path.includes("scheme=overview") ? "overview" : "view");
    if (path === "/api/navigation") return { frequency: { ranges_hz: [[1e6, 6e9]], center_step_hz: 28.6 }, time: { latest_s: T1 } };
    // The coarse map first, then the refinement over the box it returned — the same route, the same
    // question, at the resolution the first answer made available.
    return asked.filter((a) => a.startsWith("/api/coverage")).length === 1
      ? coverage(ORIENT_CELLS, ORIENT_ROWS, { f0: 10, f1: 20, t0: 28, t1: 31 })
      : coverage(ORIENT_CELLS, ORIENT_ROWS, { f0: 60, f1: 62, t0: 0, t1: 31 });
  });
  assert.deepEqual(asked, [
    "/api/tiles?level_f=0&level_t=0&f_index=0&t_index=0&cells=8&planes=compact",
    // T-505: the second tier, probed the same cheap way. Both lattices are READ OFF an answer;
    // neither is ever chosen here.
    "/api/tiles?level_f=0&level_t=0&f_index=0&t_index=0&scheme=overview&cells=8&planes=compact",
    "/api/navigation",
    coverageUrl({ f0Hz: 1e6, f1Hz: 6e9, t0Ns: T0 * S, t1Ns: T1 * S }, ORIENT_CELLS, ORIENT_ROWS),
    coverageUrl(surfaceWide.box!, ORIENT_CELLS, ORIENT_ROWS),
  ]);
  assert.equal(p.lattice.cells, 256, "the probe is cheap (8 cells); the surface renders at the route's default");
  assert.equal(p.lattice.f0Hz, 6250, "level 0 is read off the answer, never chosen here");
  assert.equal(p.opening.onCoverage, true);
  assert.equal(p.lattices.detail, p.lattice);
  assert.equal(p.lattices.overview.scheme, "overview", "the overview tier is its own lattice and its own cache key");
  assert.deepEqual(p.degraded, []);

  // The refinement decides where to OPEN; the note's share stays the surface-wide one, because it
  // is a statement about the whole surface and the refined pass measures inside coverage.
  assert.deepEqual(p.census, surfaceWide);
  assert.match(p.note, new RegExp(`${surfaceWide.observed} of ${surfaceWide.total} coverage cells`));
  assert.ok(p.opening.freq.spanHz < surfaceWide.box!.f1Hz - surfaceWide.box!.f0Hz,
    "the refinement did not narrow the opening window: a coarse cell is 51.2 MHz on a 6.5 GHz surface");
});

test("T-955: probeSurface opens on the RECENTLY tuned band, not the union of everything ever observed", async () => {
  // The exact shape found live 2026-09-25: a long tuning near 101 MHz, then a retune to ~11 MHz a
  // few rows ago. Before this fix `p.opening` centred somewhere between the two — on current code
  // (revert the `recentObservedExtent` narrowing in `probeSurface` to see it) this assertion is red:
  // `openingWindow(origin.bounds, observedExtent(cov).box)` puts the centre near 56 MHz, off the
  // tuned band entirely, and a span wide enough to draw it as a sliver.
  const cov = twoBandCoverage(ORIENT_CELLS, ORIENT_ROWS, { f0: 100, f1: 102, t1: 20 }, { f0: 10, f1: 12, t0: 29, t1: 31 });
  const p = await probeSurface(async (path) => {
    if (path.startsWith("/api/tiles")) return tileProbeResponse(path.includes("scheme=overview") ? "overview" : "view");
    if (path === "/api/navigation") return { frequency: { ranges_hz: [[1e6, 6e9]], center_step_hz: 28.6 }, time: { latest_s: T1 } };
    return cov;
  });
  assert.equal(p.opening.onCoverage, true);
  assert.ok(p.opening.freq.centerHz >= 9e6 && p.opening.freq.centerHz <= 14e6,
    `opened away from the currently-tuned band: centre ${p.opening.freq.centerHz / 1e6} MHz`);
  assert.ok(p.opening.freq.spanHz < 20e6, `sliver span left over from the old union box: ${p.opening.freq.spanHz}`);
});

test("a refinement that finds nothing keeps the coarse box: an observed cell has something in it", async () => {
  let n = 0;
  const p = await probeSurface(async (path) => {
    if (path.startsWith("/api/tiles")) return tileProbeResponse(path.includes("scheme=overview") ? "overview" : "view");
    if (path === "/api/navigation") return { frequency: { ranges_hz: [[1e6, 6e9]], center_step_hz: 28.6 }, time: { latest_s: T1 } };
    return ++n === 1 ? coverage(ORIENT_CELLS, ORIENT_ROWS, { f0: 10, f1: 20, t0: 28, t1: 31 }) : coverage(ORIENT_CELLS, ORIENT_ROWS, null);
  });
  assert.equal(p.opening.onCoverage, true, "an empty refinement must not send the view to nowhere");
  assert.deepEqual(p.degraded, []);
});

test("a route that fails degrades visibly: the fallback is recorded, never silent", async () => {
  const p = await probeSurface(async (path) => {
    if (path.startsWith("/api/tiles")) return tileProbeResponse(path.includes("scheme=overview") ? "overview" : "view");
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
  const probePath = "/api/tiles?level_f=0&level_t=0&f_index=0&t_index=0&cells=8&planes=compact";
  assert.deepEqual(asked.slice(0, 3), [probePath, probePath, probePath],
    "the request the client builds on a refusal is the SAME request, again");
  assert.deepEqual(slept, [4, 8], "and it waits longer each time rather than re-asking on one cadence");
  assert.deepEqual(p.degraded, [], "a refusal that was answered is not a degradation");
  assert.equal(p.lattice.cells, 256, "and the surface opens exactly as if it had never been refused");
  assert.deepEqual(p.requests.slice(0, 3), [probePath, probePath, probePath],
    "every attempt is on the record: the page shows what it actually asked for");
});

test("the bootstrap's retry budget is stated in TIME, and no one wait swallows it (T-690)", () => {
  // The bound used to be five attempts with an uncapped doubling backoff — 4.65 s, most of it
  // asleep — which is a bet on how fast the tile route answers. Measured in the browser tier the
  // route moves from ~167 ms a tile to ~3612 ms with all four slots held by another tab, and at
  // the slow end a second tab could not open at all. So the budget is a DURATION and the attempts
  // are derived from it, and the property is asserted rather than the number.
  const waits = [];
  const n = retriesForBudget(150, MAX_BACKOFF_MS, RETRY_BUDGET_MS);
  for (let i = 0; i < n; i++) waits.push(Math.min(MAX_BACKOFF_MS, 150 * 2 ** i));
  const total = waits.reduce((a, b) => a + b, 0);
  assert.ok(total <= RETRY_BUDGET_MS, `the derived waits total ${total} ms, over the ${RETRY_BUDGET_MS} ms budget`);
  assert.ok(total > RETRY_BUDGET_MS - MAX_BACKOFF_MS,
    `the derived waits total only ${total} ms of a ${RETRY_BUDGET_MS} ms budget — more than one ` +
    "capped wait is being left unspent, so the attempts and the budget have drifted apart");
  assert.ok(waits.every((w) => w <= MAX_BACKOFF_MS),
    `a single backoff wait grew past the ${MAX_BACKOFF_MS} ms cap: ${waits.join(" ")}`);
  // And the cap is what makes it a cadence rather than a nap: without it, five doublings from
  // 150 ms already spend 4.65 s in four waits, which is the shape that gave up too early.
  assert.ok(n >= 10, `only ${n} retries fit the budget, so the client still gives up in a handful of attempts`);
  assert.equal(retriesForBudget(150, MAX_BACKOFF_MS, 0), 0, "a zero budget buys no retries at all");
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

/**
 * A tile probe answer. `scheme` echoes the address, because the two tiers are told apart by it
 * (T-505) — a stub that answered every probe as `view` would make one lattice look like two.
 * `overview` also reports the SPECTRUM-HISTORY geometry: absolutely coarse cells that do not move
 * when the view pyramid's floor does, which is the whole reason it can answer a device-wide window.
 */
function tileProbeResponse(scheme = "view") {
  const n = 8 * 8;
  const over = scheme === "overview";
  return {
    key: { device: "any", scheme, level_f: 0, level_t: 0, f_index: 0, t_index: 0, cells: 8 },
    extent: { nf: 8, nt: 8 },
    axes: {
      frequency: { levels: 20, cell_hz: 6250, ...(over ? { max_level: 11 } : {}) },
      time: { levels: 15, cell_s: 1, ...(over ? { max_level: 14 } : {}) },
    },
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
    // T-470: the anchored display range, resolved once at open and never from a viewport.
    range: { lo: -95, hi: -45, source: "test" },
    note: "test",
    requests: [],
    degraded: [],
  };
}

function harness() {
  const g = stubGl(1200, 600);
  const asked: TileAddr[] = [];
  const fetchFn = async (url: string) => {
    const addrs = parseTileRequest(url);
    for (const a of addrs) asked.push(a);
    return {
      ok: true, status: 200, statusText: "OK",
      json: async () => answerFor(url, addrs, LAT),
    };
  };
  const preview = new SurfacePreview({
    canvas: g.canvas, probe: probeFor(), token: "t", fetchFn, chrome: null, minimapPx: 120,
  });
  return { g, preview, asked };
}

/**
 * Every address one tile request names — one for `GET /api/tiles`, many for `/api/tiles/batch`
 * (T-573). The client batches, so a fixture that only understood the single-address route would
 * answer nothing and every assertion below it would be about a failed decode.
 */
function parseTileRequest(url: string): TileAddr[] {
  const q = new URLSearchParams(url.slice(url.indexOf("?") + 1));
  const addresses = q.get("addresses");
  if (addresses === null) return [parseTileUrl(url)];
  const base = parseTileUrl(url);
  return addresses.split(",").map((spelling) => {
    const [lf, lt, fi, ti] = spelling.split(".").map(Number);
    return { ...base, levelF: lf, levelT: lt, fIndex: fi, tIndex: ti };
  });
}

/** The body for either spelling: one tile, or a batch entry per address. */
function answerFor(url: string, addrs: TileAddr[], lat: Lattice) {
  if (!url.includes("/api/tiles/batch")) return tileAnswer(addrs[0], lat);
  return {
    requested: addrs.length, returned: addrs.length, truncated: false, remaining: [],
    tiles: addrs.map((a) => ({
      address: { level_f: a.levelF, level_t: a.levelT, f_index: a.fIndex, t_index: a.tIndex,
                 spelling: `${a.levelF}.${a.levelT}.${a.fIndex}.${a.tIndex}` },
      status: 200, tile: tileAnswer(a, lat),
    })),
  };
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

function tileAnswer(a: TileAddr, lat: Lattice = LAT) {
  const n = a.cells * a.cells;
  return {
    key: { device: a.device, scheme: a.scheme, level_f: a.levelF, level_t: a.levelT, f_index: a.fIndex, t_index: a.tIndex, cells: a.cells },
    extent: { nf: a.cells, nt: a.cells },
    axes: { frequency: { levels: lat.levelsF, cell_hz: lat.f0Hz * 2 ** a.levelF }, time: { levels: lat.levelsT, cell_s: (lat.t0Ns * 2 ** a.levelT) / 1e9 } },
    grid: { nf: a.cells, nt: a.cells, max_db: Array<number>(n).fill(-90), range_db: { lo: -100, hi: -60 } },
    // **T-467's wire shape**: a table of distinct run-length-encoded planes, with `any.plane` an
    // INDEX into it. Every cell of this fixture is observed, so that is one run. Spelled out here
    // rather than borrowed from `surface-tile.test.ts` because this file's subject is the host, not
    // the decoder — but it has to track the route, and when it did not (this fixture still emitted
    // the pre-T-467 `any.cells`) every tile in this harness failed to DECODE, which is what the
    // premise assertions below now catch instead of letting it be read as a refresh defect.
    coverage: {
      grid: { nf: a.cells, nt: a.cells },
      states: ["unobserved", "observed", "unknown"],
      planes: [{ runs: [1, n], cells: n, uniform: "observed" }],
      any: { plane: 0 },
      devices: [],
      selected: { device: "any", named: false, present: true },
    },
    resolution: { source: "spectrum-history", answered: { level: a.levelF } },
  };
}

/**
 * **The premise both live-edge guards rest on: the harness's tiles actually arrived.**
 *
 * Stated as its own assertion because of how these two tests failed on the T-467 merge. The fixture
 * above still emitted the old coverage shape, so every fetch threw `TileDecodeError`, no tile ever
 * became resident, and `acquire` re-scheduled each address on every frame — 157 re-asks in 700 ms.
 * The control read that as *"the historical preview is refreshing"* and the live one as *"asking is
 * not arriving"*: two confident, precise, **wrong** diagnoses, because neither test said what its
 * number was a property OF. A repeat count is only evidence about the refresh lane once the
 * ordinary path is known to be working.
 */
function assertTilesArrived(preview: SurfacePreview, d: Drive): void {
  const cache = preview.view.surface.cache;
  assert.equal(cache.stats.failures, 0,
    `${cache.stats.failures} tile fetches failed: the fixture no longer decodes, so nothing below is ` +
    "about the live edge. Check this file's `tileAnswer` against `ui/src/surface/tile.ts`.");
  assert.ok(cache.residentTiles > 0, "no tile is resident, so there is nothing for a refresh to replace");
  // **Not "this frame"** (T-537). `drew > 0` was read off `lastFrame` after a fixed WALL-CLOCK
  // budget, and residency is a property of the limit rather than of an instant — so on a loaded
  // machine the budget bought one frame, the opening one, drawn before any answer could arrive.
  // Measured at load 130: `resident=157 … drew=0`, i.e. the tiles were in hand and the frame that
  // was asked about predated them. `drive()` ends on the property instead, so the frame reported
  // here is the frame the property was read at.
  assert.ok(d.ok,
    `no viewport became fully resident in ${d.frames} frames (${d.ms} ms): ${d.last}\n` +
    `cache: ${d.stats}`);
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

// ——— T-460: the live edge advances, and the wiring is the subject ———
//
// The guard in `surface-cache.test.ts` is about the POLICY; this one is about the CALL. T-460's
// defect was not that `TileCache` refused to refresh — it had no refresh at all, and the only path
// that could drop a live-edge tile had one call site, the retune. A policy nothing invokes is
// exactly the shape of T-450, whose renderer was proved on 114 973 pixels for a module that could
// not load. So this drives the real `SurfacePreview.frame()` loop and asks whether the address was
// asked for twice.
//
// The edge **grows, slowly enough to stay inside one tile** ([[LIVE_EDGE_NS_PER_MS]]), which is the
// window the defect lives in: the address a following pane resolves changes only when the edge
// crosses a tile boundary — at `level_t = 0`, once every 256 seconds — and for all of that time the
// rows being recorded were served and never requested.
//
// **It was held STILL, and since T-495 that would test nothing.** Freshness is no longer "is the edge
// inside this tile" but "was this copy asked for at an edge that already reached everything it could
// hold", so an edge that never moves correctly produces no second request: there is provably no new
// row to fetch. A still edge is not a live edge, and a guard built on one would be measuring the
// harness. Whether the rows then appear on the screen is `ui/e2e/live-edge.e2e.mjs`'s subject — a
// tile being re-fetched is not a row being drawn, and this tier cannot tell the two apart.

/** A lattice whose time cells are milliseconds, so the refresh cadence — one cell — is testable in
 * a test's lifetime rather than in the product's 1 s. Nothing else about it is special. */
const LIVE_LAT: Lattice = { scheme: "view", cells: 64, f0Hz: 6250, t0Ns: 1e6, levelsF: 20, levelsT: 15 };
/**
 * Capture ns the reported edge advances **per frame this test draws** (T-537).
 *
 * It was per millisecond of *this machine's wall clock*, which made the subject of the test — how
 * far the edge moved between two frames — a function of the load the machine happened to be under.
 * The edge is a backend number the client is *told*, never one it reads off its own clock, so a
 * virtual capture clock the test steps once per frame is both the deterministic instrument and the
 * faithful one.
 *
 * Small on purpose. A tile here is 64 ms of capture and the subject is a tile that keeps the same
 * ADDRESS while its newest rows are written, so the edge must move (a still edge is fresh by
 * [[TileCache.behindTheEdge]] and correctly produces no second request) and must not move so far
 * that the pane scrolls into a new tile and the re-ask becomes an ordinary miss. At the level the
 * pane draws at — `level_t = 3`, 8 ms cells, a 512 ms tile — the whole [[MAX_FRAMES]] budget moves
 * it 120 ms, under a quarter of one tile, and a passing run moves it 19 ms (measured, and printed
 * by the test): two cells, inside the one tile throughout.
 */
const LIVE_EDGE_NS_PER_FRAME = 2e5;
/**
 * The frame budget a [[drive]] is bounded by — **frames, not milliseconds** (T-537).
 *
 * Wall time is the wrong budget for "let this converge": under load a `setTimeout(10)` fires 700 ms
 * late, so a 700 ms budget bought a single frame and the test then asked its question of the opening
 * frame. Frames are the unit the thing under test actually advances in, and each one here is
 * followed by a settle, so the count means the same on an idle machine and a saturated one.
 *
 * The real-time rate limits inside `TileCache` (a 250 ms edge scan, a per-lane duty cycle of
 * `REFRESH_DUTY x` the measured service time) are not bypassed and are not meant to be: they are
 * part of the behaviour under test. They are satisfied by the pacing sleep below, and a machine so
 * loaded that frames are slow satisfies them in FEWER frames, never more.
 */
const MAX_FRAMES = 600;
/**
 * Frames the CONTROL keeps drawing after it has filled, to make "nothing was refreshed" a claim
 * about a run rather than about a first fill. It is a fixed count rather than the live arm's own
 * (which stops as soon as its property arrives, so it varies), and comfortably more than the live
 * arm needs — 94 frames idle, and fewer under load, because the gates it waits on are real-time
 * ones that a slow frame satisfies in fewer frames — so the control is never the shorter exercise.
 */
const CONTROL_FRAMES = 250;

function liveHarness({ live = true, rows = null as RowOpener | null } = {}) {
  const g = stubGl(1200, 600);
  const asked: TileAddr[] = [];
  /** The virtual capture clock, in ns advanced. [[drive]] steps it; nothing reads a wall clock. */
  const clock = { ns: 0 };
  const fetchFn = async (url: string) => {
    const addrs = parseTileRequest(url);
    for (const a of addrs) asked.push(a);
    return { ok: true, status: 200, statusText: "OK", json: async () => answerFor(url, addrs, LIVE_LAT) };
  };
  const probe: SurfaceProbe = {
    ...probeFor({ freq: { centerHz: 100.8e6, spanHz: 2.4e6 }, centerNs: T1 * S - S, spanNs: 2 * S, onCoverage: true }),
    lattice: LIVE_LAT,
  };
  const preview = new SurfacePreview({
    canvas: g.canvas, probe, token: "t", fetchFn, chrome: null, minimapPx: 120,
    // The ONE difference between the two arms: whether a growing edge is reported in at all.
    edge: live ? () => probe.origin.edgeNs + clock.ns : null,
    rows,
  });
  return { g, preview, asked, clock };
}

/** What a [[drive]] measured: whether the property arrived, and what was true when it stopped. */
interface Drive {
  /** Did `want()` hold before the frame budget ran out? */
  readonly ok: boolean;
  readonly frames: number;
  /** Wall time the drive took. **Reported, never asserted on** — it is load, not behaviour. */
  readonly ms: number;
  /** The last frame's per-viewport `PaneReport`s, formatted for a failure message. */
  readonly last: string;
  readonly stats: string;
  /**
   * Frames on which a viewport reported `pending` although **no new address was requested that
   * frame** — i.e. it went back to pending for a place it already held.
   *
   * This is the deterministic form of "a refresh must never put a pane back to pending". The old
   * form read `pending` off the last frame, which is also non-zero for the one frame after a
   * following pane's window slides across a tile boundary and discovers a genuinely new address —
   * a legitimate miss that the edge's own motion produces, and which an instant sample cannot tell
   * from the defect. `stats.distinctKeys` is `everRequested.size`, so comparing it across the frame
   * separates the two exactly: a new place explains a pending, a place already in hand does not.
   */
  readonly heldThenPending: string[];
}

/**
 * Draw frames, stepping the virtual capture clock once per frame, **until `want()` holds** or the
 * frame budget runs out. Never throws: the caller states the claim and reports the measurement.
 *
 * The 1 ms pace is what lets `TileCache`'s own real-time gates elapse; it is not a budget, and
 * nothing here is asserted against elapsed time.
 */
async function drive(
  preview: SurfacePreview,
  clock: { ns: number },
  want: () => boolean,
  { frames = MAX_FRAMES, watchPending = false } = {},
): Promise<Drive> {
  const t0 = Date.now();
  const cache = preview.view.surface.cache;
  const heldThenPending: string[] = [];
  let last = "";
  let drawn = 0;
  for (let n = 0; n < frames; n++) {
    drawn++;
    clock.ns += LIVE_EDGE_NS_PER_FRAME;
    const discovered = cache.stats.distinctKeys;
    const f = preview.frame();
    last = `frame ${n + 1}: ` + f.reports.map((r) =>
      `${r.id}[level ${r.levelF}/${r.levelT} · ${r.tiles} tiles · ${r.fallbacks} coarse · ` +
      `${r.pending} pending · ${r.refused} refused]`).join("  ");
    if (watchPending && cache.stats.distinctKeys === discovered) {
      for (const r of f.reports) if (r.pending > 0) { heldThenPending.push(last); break; }
    }
    // Settle every fetch this frame issued. Twice: the stub answers in microtasks, and one
    // `setImmediate` is only guaranteed to be after the microtasks queued before it.
    await flush();
    await flush();
    if (want()) break;
    await new Promise((r) => setTimeout(r, 1));
  }
  const s = cache.stats;
  return {
    ok: want(), frames: drawn, ms: Date.now() - t0, last, heldThenPending,
    stats: `${cache.residentTiles} resident · ${s.requests} requests · ${s.distinctKeys} distinct · ` +
      `${s.edgeRefreshes} refreshes (${s.edgeRefreshApplied} applied) · ${s.failures} failures · ` +
      `${s.cancelled} cancelled · ${s.evictions} evictions`,
  };
}

/** Every viewport is drawing its own tiles and waiting for none: the premise both guards rest on. */
const allResident = (preview: SurfacePreview) => () =>
  !!preview.lastFrame && preview.lastFrame.reports.length > 0 &&
  preview.lastFrame.reports.every((r) => r.tiles > 0 && r.pending === 0);

const repeats = (asked: TileAddr[]) => {
  const seen = new Set<string>(), again = new Set<string>();
  for (const a of asked) { const k = keyOf(a); if (seen.has(k)) again.add(k); seen.add(k); }
  return again;
};

// **Both arms are driven in FRAMES and stopped on the PROPERTY** (T-537). They were driven for a
// fixed 700 ms of wall clock and then asked their question of `lastFrame`, and at load 129 that
// budget bought exactly one frame — the opening one, before any answer could have arrived — so the
// test reported "the renderer drew no resident tile this frame" about a frame drawn before the 157
// tiles it was asking about existed. Residency, a repeat and an applied refresh are all properties
// of the limit, not of an instant; each is now waited for with a stated budget and each failure
// prints the value it measured. See [[MAX_FRAMES]] and [[LIVE_EDGE_NS_PER_FRAME]].

test("a FOLLOWING pane re-asks for the live-edge tile: the rows recorded since are fetched", async (t) => {
  const { preview, asked, clock } = liveHarness();
  assert.equal(preview.view.panes.isFollowing(preview.activePane), true,
    "a reported edge opens the first pane following — otherwise this tests nothing");
  const cache = preview.view.surface.cache;

  // 1. The premise: the ordinary path works and every viewport is drawing its own tiles. A repeat
  //    count is only evidence about the refresh lane once this holds.
  const filled = await drive(preview, clock, allResident(preview));
  assertTilesArrived(preview, filled);

  // 2. The claim: with the edge advancing, the live-edge tile is asked for AGAIN and the answer
  //    replaces the resident copy. Both are waited for, because both are things that arrive.
  const again = () => repeats(asked);
  const refreshed = await drive(preview, clock,
    () => cache.stats.edgeRefreshApplied > 0 && again().size > 0,
    { frames: MAX_FRAMES - filled.frames, watchPending: true });
  assert.ok(refreshed.ok,
    `over ${filled.frames + refreshed.frames} frames on a following pane whose edge advanced ` +
    `${((filled.frames + refreshed.frames) * LIVE_EDGE_NS_PER_FRAME) / 1e6} ms of capture, ` +
    `${again().size} address(es) were asked for twice and ${cache.stats.edgeRefreshApplied} of ` +
    `${cache.stats.edgeRefreshes} refresh(es) replaced a resident tile. This is T-460, the frozen ` +
    `live edge: the rows were recorded and served, and the client stopped asking.\n  ${refreshed.stats}\n  ${refreshed.last}`);
  // **Printed on a PASS too**, because the numbers that drifting would silently make this test
  // vacuous — how few frames of the budget it actually needs, and how far the edge moved inside one
  // 512 ms tile — are invisible in a green tick. [[CONTROL_FRAMES]] is sized off the first of them.
  t.diagnostic(`filled in ${filled.frames} frames (${filled.ms} ms); the live edge was re-asked and ` +
    `applied ${refreshed.frames} frames later (${refreshed.ms} ms), the edge having advanced ` +
    `${((filled.frames + refreshed.frames) * LIVE_EDGE_NS_PER_FRAME) / 1e6} ms of capture out of ` +
    `${MAX_FRAMES} frames budgeted. ${refreshed.stats}`);

  // Every repeat is a live-edge tile at the level the pane was DRAWN at, never a parent pin and
  // never a tile some other viewport wanted — or (T-893) a coarser tile the renderer actually DREW
  // as a stand-in over this following pane, which is on screen at its live edge just the same.
  const drawn = preview.lastFrame!.reports.find((r) => r.id === preview.activePane)!;
  for (const k of again()) {
    const a = parseKey(k)!;
    if (cache.refreshedAsStandIn.has(k)) continue;
    assert.equal(a.levelT, drawn.levelT, `refreshed ${k}, which the pane is not drawing`);
    assert.equal(a.levelF, drawn.levelF, `refreshed ${k}, which the pane is not drawing`);
  }
  assert.deepEqual(refreshed.heldThenPending, [],
    "a refresh must never put a pane back to pending: the stale copy stays drawn until the new one " +
    "lands. These frames reported pending without requesting a new address, so the place was one " +
    "already in hand:\n  " + refreshed.heldThenPending.join("\n  "));
});

test("…and the CONTROL: T-450's historical preview refreshes NOTHING", async () => {
  // Non-vacuity for the test above, and the guarantee this ticket owed the preview page: with no
  // edge reported in, every viewport is frozen, the data under it cannot change, and a refresh
  // would be cost with nothing to show for it.
  const { preview, asked, clock } = liveHarness({ live: false });
  assert.equal(preview.view.panes.isFollowing(preview.activePane), false);
  const filled = await drive(preview, clock, allResident(preview));
  assertTilesArrived(preview, filled);
  // And then the same exercise the live arm gets: frames, and a clock that steps — which nothing
  // here reads, because no edge is reported in. A control that stopped at the first fill would be
  // asserting that nothing happened in less work than the arm it is the control for.
  const idle = await drive(preview, clock, () => false, { frames: CONTROL_FRAMES });
  assert.equal(preview.view.surface.cache.stats.edgeRefreshes, 0,
    "the historical preview issued a refresh, and nothing there is following a growing edge.\n  " + idle.stats);
  assert.equal(repeats(asked).size, 0,
    `the historical preview re-asked for a tile whose data cannot change, over ${idle.frames} idle ` +
    `frames.\n  ${idle.stats}`);
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
  // …and each axis is still clamped on its own — **but a PLAIN wheel now stops when EITHER of them
  // does** (T-472). This assertion is inverted rather than deleted, on T-347's precedent: what it
  // protected is still true and is asserted immediately below (the clamps are per-axis, and shift
  // still reaches the surface's whole extent). What changed is the *gesture*. Letting frequency run
  // on to 6 GHz while time sat pinned at the record is precisely the defect the user reported — the
  // proportions of the picture drift under a gesture that promised to scale both equally, the view
  // jumps, and the frequency axis has to be shift-scrolled back by hand after every wheel.
  const at = preview.frame().views.find((v) => v.id === id)!.box;
  const ratio = (x: typeof at) => (x.f1Hz - x.f0Hz) / (x.t1Ns - x.t0Ns);
  preview.wheel(id, { x: 600, y: 400 }, 1e6, wheelAxes({}));
  const w = preview.frame().views.find((v) => v.id === id)!.box;
  assert.equal(w.t1Ns - w.t0Ns, BOUNDS.t1Ns - BOUNDS.t0Ns, "time should be pinned to the record's extent");
  assert.ok(w.f1Hz - w.f0Hz < BOUNDS.f1Hz - BOUNDS.f0Hz,
    "a plain wheel carried frequency to the surface's whole 6 GHz while time was already pinned at " +
    "the record: the uniform zoom did not stop with the axis that ran out, so the aspect ratio drifted");
  assert.ok(Math.abs(ratio(w) / ratio(at) - 1) < 1e-9,
    `the plain wheel changed the pane's aspect ratio (${ratio(at)} → ${ratio(w)} Hz/ns)`);

  // Shift is the escape hatch the user asked for, and the ONLY way to widen frequency past the lock.
  preview.wheel(id, { x: 600, y: 400 }, 1e6, wheelAxes({ shiftKey: true }));
  const s = preview.frame().views.find((v) => v.id === id)!.box;
  assert.equal(s.f1Hz - s.f0Hz, BOUNDS.f1Hz - BOUNDS.f0Hz, "shift + wheel must still reach the whole surface");
  assert.equal(s.t1Ns - s.t0Ns, w.t1Ns - w.t0Ns, "shift + wheel moved the time axis");
});

/**
 * **T-472, through the host: a plain wheel over a pane is `PaneModel.zoomBoth`, and nothing else.**
 *
 * `ui/test/surface-aspect.test.ts` carries the quantified ratio property over the pane model. What
 * is left to show here is that the wheel path a host actually takes reaches it — the same reason
 * T-456's semantics live in `preview.ts` rather than in `input.ts`, and the same failure mode: the
 * arithmetic can be right while the gesture routes around it.
 *
 * The minimap is asserted too, because it is a viewport and gets the viewport's gesture. Wheeling it
 * with its own pair of clamps would be a second opinion about what a plain wheel means, which is the
 * whole class of defect (T-412) the single-host rule exists to prevent.
 */
test("T-472: a plain wheel through the preview stops with the first axis to run out, on panes and on the map", () => {
  const { preview } = harness();
  const id = preview.activePane;
  preview.frame();
  const boxOfPane = () => preview.frame().views.find((v) => v.id === id)!.box;
  /**
   * The ratio is read from the pane's **spans**, not from `f1 − f0` of its box.
   *
   * The box is `boxOf` applied to exactly these spans, so it is the same window — but reconstructing
   * a span as `(c + s/2) − (c − s/2)` at 100 MHz with a 2 MHz window loses bits to cancellation, and
   * that residue is an artefact of the witness rather than a drift in the view. Measuring it here
   * would be asserting a property of double-precision subtraction, which is the adjacent-question
   * mistake this ticket was warned about.
   */
  const ratio = (p: { freq: { spanHz: number }; time: { spanNs: number } }) => p.freq.spanHz / p.time.spanNs;
  const paneNow = () => preview.view.panes.get(id)!;
  const at = { x: 300, y: 500 }; // deliberately off-centre: the anchors are separate, and stay so

  // Wheel out to the bound, recording the ratio at every step. The claim is about the WHOLE gesture,
  // not its endpoints: a lock that let the ratio drift and then restored it would look identical at
  // the ends and wrong in the hand.
  const r0 = ratio(paneNow());
  const drift: number[] = [];
  for (let i = 0; i < 24; i++) {
    preview.wheel(id, at, zoomFactor(240), wheelAxes({}));
    drift.push(Math.abs(ratio(paneNow()) / r0 - 1));
  }
  assert.ok(Math.max(...drift) < 1e-9,
    `the aspect ratio drifted by up to ${(Math.max(...drift) * 100).toExponential(2)} % across 24 plain wheels`);

  // At the bound the gesture is refused outright — neither axis moves, in either direction.
  const held = boxOfPane();
  preview.wheel(id, at, zoomFactor(240), wheelAxes({}));
  assert.deepEqual(boxOfPane(), held, "a plain wheel at the bound moved the pane");
  assert.equal(preview.view.panes.lockedZoomFactor(id, zoomFactor(240)), 1);
  // …and it is TIME that ran out, with frequency still short of the surface — so the stop is the
  // lock's, not the frequency clamp's. Without this the assertion above would also pass on a pane
  // that had simply zoomed out to everything.
  assert.ok(held.f1Hz - held.f0Hz < BOUNDS.f1Hz - BOUNDS.f0Hz,
    "frequency reached the whole surface, so this is not the case the lock is about");
  assert.equal(held.t1Ns - held.t0Ns, BOUNDS.t1Ns - BOUNDS.t0Ns);

  // The map takes the same gesture through the same model — one opinion about a wheel, not two.
  const mapNow = () => preview.view.minimap.state();
  const m0 = ratio(mapNow()), mSpan0 = mapNow().freq.spanHz;
  for (let i = 0; i < 8; i++) preview.wheelMap({ x: 200, y: 8 }, zoomFactor(-240), wheelAxes({}));
  assert.ok(Math.abs(ratio(mapNow()) / m0 - 1) < 1e-9,
    "a plain wheel over the minimap changed its aspect ratio: the map has its own idea of the gesture");
  assert.ok(mapNow().freq.spanHz < mSpan0,
    "the minimap did not zoom at all, so the assertion above is vacuous");
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

test("T-506: the time floor extends back to the retained capture window, and never moves forward", () => {
  // The canvas absorbed the Capture panel's retention window (T-338). On a young server the record
  // horizon is seconds old, so without this the retention bound and the IQ horizon would sit below
  // the surface's floor where no pane could reach them.
  const { preview } = harness();
  const id = preview.activePane;
  const b0 = preview.bounds;
  assert.equal(preview.extendTimeFloor(b0.t0Ns + 5 * S), false, "a newer floor would shrink the extent");
  assert.deepEqual(preview.bounds, b0);
  assert.equal(preview.extendTimeFloor(Number.NaN), false);
  const fullBefore = (() => { preview.view.panes.zoomTime(id, 1e9); return preview.view.panes.get(id)!.time.spanNs; })();
  assert.equal(preview.extendTimeFloor(b0.t0Ns - 120 * S), true);
  assert.equal(preview.bounds.t0Ns, b0.t0Ns - 120 * S, "the floor moved back to the window's start");
  assert.deepEqual({ ...preview.bounds, t0Ns: b0.t0Ns }, b0, "and nothing else about the extent changed");
  preview.view.panes.zoomTime(id, 1e9);
  assert.ok(preview.view.panes.get(id)!.time.spanNs >= fullBefore + 120 * S - 1,
    "a pane can now zoom out over the whole retained window");
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
  const DEVICE_ROUTES = ["/api/control/center", "/api/control/rate", "/api/control/window", "/api/control/gains", "/api/control/bias_tee", "/api/control/baseband_filter"];
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

test("T-445 INVERTS T-450's ADDITIVE CLAIM: the app mounts THIS host, and there is no second copy of it", () => {
  const app = importGraph("src/app/main.ts");
  assert.ok(app.has("src/app/shell.ts"), "the graph walk found nothing: the assertions below would pass vacuously");
  assert.ok(app.size > 30, `the graph walk found only ${app.size} files; it is not walking the app`);
  // The host, the bootstrap and the shared input handler are all on the app's graph now. Before the
  // cutover this assertion was `!app.has(...)`, for a preview nothing was allowed to reach.
  for (const f of ["src/surface/preview.ts", "src/surface/bootstrap.ts", "src/surface/input.ts",
    "src/surface/view.ts", "src/surface/panes.ts", "src/surface/minimap.ts", "src/surface/marks.ts"]) {
    assert.ok(app.has(normalize(f)), `${f} is NOT reachable from the app — the cutover mounted something else`);
  }
  // The page entry stays the preview page's own: two pages, one host. If the app had grown its own
  // copy of `SurfacePreview` this is where it would show, because the class would have two definers.
  const definers = ["src/surface/preview.ts", "src/app/centre/surface.ts", "src/surface/preview-main.ts"]
    .filter((f) => /class SurfacePreview/.test(SRC(f)));
  assert.deepEqual(definers, ["src/surface/preview.ts"], "the mounted host has exactly one definition");
  const pkg = JSON.parse(SRC("package.json")) as { scripts: Record<string, string> };
  assert.match(pkg.scripts["build:surface"], /preview-main\.ts.*--outfile=dist\/surface\.js/s);
  assert.ok(existsSync("src/surface/preview.html"), "the preview page still exists beside the app");
  assert.ok(!SRC("src/app/index.html").includes("surface.js"), "…and the app still loads its own bundle, not the page's");
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

test("T-893: a FOLLOWING pane subscribes /ws/tiles/rows for the columns it draws, pushed rows reach its tiles, and PAUSING closes them", async () => {
  const opened: string[] = [];
  const open = new Map<string, (t: string) => void>();
  const closed: string[] = [];
  const rows: RowOpener = (path, onText) => {
    opened.push(path);
    open.set(path, onText);
    return { close: () => { closed.push(path); open.delete(path); } };
  };
  const { preview, clock } = liveHarness({ rows });
  const cache = preview.view.surface.cache;
  const filled = await drive(preview, clock, () => allResident(preview)() && opened.length > 0);
  assert.ok(filled.ok, `a following pane opened no row subscription in ${filled.frames} frames (${filled.stats}): ` +
    "new rows reach it only by polling (T-893)");
  const drawn = preview.lastFrame!.reports.find((r) => r.id === preview.activePane)!;
  for (const p of opened) {
    // The request the client builds (CLAUDE.md: assert the request, not only the response).
    const q = new URLSearchParams(p.split("?")[1]);
    assert.ok(p.startsWith("/ws/tiles/rows?"), p);
    assert.equal(Number(q.get("level_f")), drawn.levelF, `${p} is not at the level the pane draws`);
    assert.equal(Number(q.get("level_t")), drawn.levelT, `${p} is not at the level the pane draws`);
    assert.ok(Number.isSafeInteger(Number(q.get("t_from"))), `${p} has no start row`);
    assert.equal(q.get("t_to"), null, `${p} is not open-ended`);
  }
  // A pushed row lands in a tile the pane has in hand.
  const [path, push] = [...open.entries()][0];
  const q = new URLSearchParams(path.split("?")[1]);
  const cells = Number(q.get("cells") ?? 256), from = Number(q.get("t_from"));
  push(JSON.stringify({ type: "subscribed", range: { t_from: from, t_to: null }, extent: { t_cell_s: 0.001 } }));
  const n = Math.min(4, cells - (from % cells));
  push(JSON.stringify({
    type: "rows", row0: from, rows: n, nf: cells, max_db: new Array<number>(n * cells).fill(-50), final: false,
    coverage: { states: ["unobserved", "observed"], nt: n, nf: cells, aligned: true, plane: { runs: [1, n * cells] } },
  }));
  assert.equal(cache.stats.rowsPushed, n, "the pushed rows were not filed");
  // Pausing freezes the view: nothing follows, so every feed closes and no new one opens.
  preview.view.panes.pause(preview.activePane);
  preview.view.minimap.setFollowing(false);
  const before = opened.length;
  await drive(preview, clock, () => false, { frames: 20 });
  assert.equal(open.size, 0, `a paused pane kept ${open.size} row subscription(s) open`);
  assert.equal(opened.length, before, "a paused pane opened a row subscription");
});
