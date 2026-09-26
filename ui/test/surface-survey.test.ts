// T-580: the client asks the coverage map FIRST and never requests a tile over spectrum it says was
// never sampled.
//
// **Asserted on the requests the client builds, never on the render** (T-367): no gate covers a
// client asking the backend for the wrong thing, so this file counts what reaches the tile source.
// No wall clock anywhere — request counts only (the user, 2026-09-21).
//
//   - a fully-grey viewport issues ZERO tile requests;
//   - a half-observed viewport issues requests only for the observed half;
//   - a viewport that becomes observed issues them then;
//   - and the honesty half: "unknown", "observed-not-measured" and a band swept EARLIER (whose tile
//     carries the last-known shadow) are all still fetched, because only "nothing was sampled"
//     licenses a skip.

import { test } from "node:test";
import assert from "node:assert/strict";
import { CELL } from "../src/surface/cellrule";
import { extentOf, keyOf, type Lattice, type TileAddr } from "../src/surface/lattice";
import { Surface, type PaneView } from "../src/surface/surface";
import { TileCache } from "../src/surface/tilecache";
import type { TileData } from "../src/surface/tile";
import { SurfacePreview, type SurfaceProbe } from "../src/surface/preview";
import { decodeSurvey, surveyUrl, type SurveyResponse } from "../src/surface/survey";
import { stubGl } from "./surface-glstub";

const LAT: Lattice = { scheme: "view", cells: 256, f0Hz: 6250, t0Ns: 1e9, levelsF: 20, levelsT: 15 };
const MHZ = 1e6, S = 1e9;
const W = 800, H = 600;

/** A `GET /api/coverage` answer over `[t0, t0 + 1024) s × [0, fHi)`: `cells × rows`, each cell's
 * state from `stateAt(fHz, row)`. `recording_began_s: 0`, so the survey sees the whole past.
 * `t0` defaults to 0 — every existing caller gets the same answer as before it existed (T-982). */
function coverage(fHi: number, cells: number, rows: number,
  stateAt: (fHz: number, row: number) => string,
  horizon: SurveyResponse["horizon"] = { oldest_record_s: 0, recording_began_s: 0, forgotten: null, as_of_s: 1024 },
  t0 = 0,
): SurveyResponse {
  const dt = 1024 / rows, df = fHi / cells;
  const list: { state: string }[] = [];
  for (let r = 0; r < rows; r++) for (let c = 0; c < cells; c++) list.push({ state: stateAt((c + 0.5) * df, r) });
  return {
    window: { t0_s: t0, t1_s: t0 + 1024 },
    grid: { cells, rows, f_lo_hz: 0, f_cell_hz: df, t0_s: t0, t_cell_s: dt },
    any: { cells: list },
    horizon,
  };
}

function data(a: TileAddr): TileData {
  return {
    addr: a, key: keyOf(a), nf: 2, nt: 2, t1Ns: null, asOfNs: null,
    value: new Float32Array([-90, -80, -70, -60]),
    state: new Uint8Array([CELL.OBSERVED, CELL.OBSERVED, CELL.OBSERVED, CELL.OBSERVED]),
    tier: "spectrum-history", answeredLevel: 0, fold: { frequency: "exact", time: "exact" },
    measured: { nf: 2, nt: 2 }, rangeDb: { lo: -100, hi: -60 }, bytes: 12,
    serverInFlightLimit: null, serverInFlightShare: null,
  };
}

const flush = () => new Promise((r) => setImmediate(r));

function harness(pinParents = true) {
  const g = stubGl(W, H);
  const asked: TileAddr[] = [];
  const surface = new Surface(
    g.canvas, LAT,
    (tex) => new TileCache(tex, (a) => { asked.push(a); return Promise.resolve(data(a)); }, { inFlight: 64, now: () => 0 }),
    { pinParents },
  );
  surface.setScale(-100, -60);
  return { g, surface, asked };
}

/** A pane over `[0, 6.4 MHz) × [0, 256 s)`: four 1.6 MHz level-0 columns' worth of spectrum. */
const PANE: PaneView = { id: "p", rect: { x: 0, y: 0, w: W, h: H }, box: { f0Hz: 0, f1Hz: 6.4 * MHZ, t0Ns: 0, t1Ns: 256 * S } };

async function frames(surface: Surface, n = 6) {
  for (let i = 0; i < n; i++) { surface.render([PANE]); await flush(); }
}

test("a fully-grey viewport issues ZERO tile requests — and draws THE grey from a state byte", async () => {
  const { g, surface, asked } = harness();
  surface.setSurvey(decodeSurvey(coverage(12.8 * MHZ, 16, 2, () => "unobserved")));
  await frames(surface);
  assert.equal(asked.length, 0, `asked for ${asked.length} tiles over spectrum the coverage map says was never sampled: ${asked.map(keyOf)}`);
  const rep = surface.lastFrame[0];
  assert.ok(rep.surveyed > 0, "the places were answered by the survey");
  assert.equal(rep.pending, 0, "a surveyed place is not 'not loaded yet' — nothing is coming, and it says so");
  // The grey is drawn through the tile path from an UNOBSERVED state byte: the only way this surface
  // can produce grey (ui/test/surface-honesty.test.ts).
  const unobservedDraws = g.ops.filter((o) => o.kind === "draw" && o.units?.[1]?.data?.[0] === CELL.UNOBSERVED
    && o.units[1].w === 1 && o.units[1].h === 1);
  assert.equal(unobservedDraws.length, rep.surveyed * 6, "every surveyed place, every frame, is drawn from the one-cell UNOBSERVED plane");
});

test("CONTROL: without a survey the same viewport fetches every tile (what T-580 removes)", async () => {
  const { surface, asked } = harness();
  await frames(surface);
  assert.ok(asked.length > 0, "the control must fetch, or the zero above proves nothing");
});

test("a half-observed viewport issues requests ONLY for the observed half", async () => {
  const { surface, asked } = harness();
  // Sampled below 3.2 MHz, never above.
  surface.setSurvey(decodeSurvey(coverage(12.8 * MHZ, 16, 2, (f) => (f < 3.2 * MHZ ? "observed" : "unobserved"))));
  await frames(surface);
  assert.ok(asked.length > 0, "the observed half is still owed its tiles");
  for (const a of asked) {
    const e = extentOf(LAT, a);
    assert.ok(e.f0Hz < 3.2 * MHZ, `requested ${keyOf(a)} over ${e.f0Hz / MHZ}–${e.f1Hz / MHZ} MHz, which was never sampled`);
  }
  assert.ok(surface.lastFrame[0].surveyed > 0, "the unobserved half was answered by the survey");
});

test("a viewport that BECOMES observed issues its requests then", async () => {
  const { surface, asked } = harness();
  surface.setSurvey(decodeSurvey(coverage(12.8 * MHZ, 16, 2, () => "unobserved")));
  await frames(surface);
  assert.equal(asked.length, 0);
  // The radio tunes onto the band: the next survey says so.
  surface.setSurvey(decodeSurvey(coverage(12.8 * MHZ, 16, 2, (f, row) => (row === 0 && f < 6.4 * MHZ ? "observed" : "unobserved"))));
  await frames(surface);
  assert.ok(asked.length > 0, "the survey now says these were sampled; the tiles are owed and must be asked for");
});

test("waiting for the survey requests nothing; no survey (or a failed one) requests everything", async () => {
  const { surface, asked } = harness();
  surface.setSurvey("awaiting");
  await frames(surface);
  assert.equal(asked.length, 0, "coverage FIRST: no tile is requested before the survey has answered");
  assert.ok(surface.lastFrame[0].pending > 0, "and meanwhile the pane says 'not loaded', never grey");
  surface.setSurvey(null);
  await frames(surface);
  assert.ok(asked.length > 0, "a surface with no survey loses the saving, never an answer");
});

// ——— honesty: only "nothing was sampled" licenses a skip ———

test("'unknown' and 'observed' (with or without a level) and 'excluded' all keep the tile owed", async () => {
  for (const state of ["unknown", "observed", "excluded", "some-future-state"]) {
    const { surface, asked } = harness(false);
    surface.setSurvey(decodeSurvey(coverage(12.8 * MHZ, 16, 2, () => state)));
    await frames(surface);
    assert.ok(asked.length > 0, `a "${state}" survey must not skip a single tile`);
    assert.equal(surface.lastFrame[0].surveyed, 0, state);
  }
});

test("a band swept EARLIER is still fetched: its tile carries the last-known shadow, which a skip would turn into grey", () => {
  // Row 0 ([0, 512 s)) sampled 0–6.4 MHz; row 1 unobserved everywhere.
  const s = decodeSurvey(coverage(12.8 * MHZ, 16, 2, (f, row) => (row === 0 && f < 6.4 * MHZ ? "observed" : "unobserved")))!;
  // A tile in row 1 over the band swept in row 0: unobserved inside, but a shadow may be carried in.
  assert.equal(s.unobservedThrough({ f0Hz: 0, f1Hz: 1.6 * MHZ, t0Ns: 600 * S, t1Ns: 700 * S }), null);
  // The same span over spectrum never swept at any time: skippable, grey up to the horizon.
  assert.equal(s.unobservedThrough({ f0Hz: 8 * MHZ, f1Hz: 9.6 * MHZ, t0Ns: 600 * S, t1Ns: 700 * S }), 1024 * S);
});

test("a band first swept LATER does not block a tile that ends before it", () => {
  const s = decodeSurvey(coverage(12.8 * MHZ, 16, 2, (f, row) => (row === 1 && f < 6.4 * MHZ ? "observed" : "unobserved")))!;
  assert.equal(s.unobservedThrough({ f0Hz: 0, f1Hz: 1.6 * MHZ, t0Ns: 0, t1Ns: 256 * S }), 1024 * S);
  assert.equal(s.unobservedThrough({ f0Hz: 0, f1Hz: 1.6 * MHZ, t0Ns: 256 * S, t1Ns: 768 * S }), null);
});

test("a survey that cannot see the whole past skips nothing; a row straddling LOST records blocks", () => {
  const never = () => "unobserved";
  // Recording began before the survey window: rows a shadow could come from are outside it.
  const late = decodeSurvey(coverage(12.8 * MHZ, 16, 2, never, { oldest_record_s: 0, recording_began_s: -100, forgotten: null, as_of_s: 1024 }))!;
  assert.equal(late.complete, false);
  assert.equal(late.unobservedThrough({ f0Hz: 8 * MHZ, f1Hz: 9.6 * MHZ, t0Ns: 0, t1Ns: 256 * S }), null);
  assert.equal(late.floorNs, -100 * S, "and it says how far back the next survey must reach");
  // Records between recording_began_s and oldest_record_s were lost; row 0 straddles oldest.
  const lost = decodeSurvey(coverage(12.8 * MHZ, 16, 2, never, { oldest_record_s: 300, recording_began_s: 0, forgotten: null, as_of_s: 1024 }))!;
  assert.equal(lost.unobservedThrough({ f0Hz: 8 * MHZ, f1Hz: 9.6 * MHZ, t0Ns: 0, t1Ns: 256 * S }), null);
  assert.equal(lost.unobservedThrough({ f0Hz: 8 * MHZ, f1Hz: 9.6 * MHZ, t0Ns: 600 * S, t1Ns: 700 * S }), null,
    "a later tile could carry a shadow from the lost interval");
  // Nothing lost: the same row is fine.
  const whole = decodeSurvey(coverage(12.8 * MHZ, 16, 2, never, { oldest_record_s: 0, recording_began_s: 0, forgotten: "observation log pruned", as_of_s: 1024 }))!;
  assert.equal(whole.unobservedThrough({ f0Hz: 8 * MHZ, f1Hz: 9.6 * MHZ, t0Ns: 600 * S, t1Ns: 700 * S }), 1024 * S,
    "oldest_record_s on a row boundary straddles nothing");
  // Outside the surveyed band: no claim.
  assert.equal(whole.unobservedThrough({ f0Hz: 12 * MHZ, f1Hz: 14 * MHZ, t0Ns: 0, t1Ns: S }), null);
  // Unreadable is not an answer.
  assert.equal(decodeSurvey({ grid: { cells: 4, rows: 1, f_lo_hz: 0, f_cell_hz: 1, t0_s: 0, t_cell_s: 1 }, any: { cells: [] } }), null);
});

test("T-982: `complete` tolerates the float64-ns precision loss a real epoch-second `t0`/`began` pair carries, not just a textbook-exact match", () => {
  // Measured live (a second tab opening /surface.html while / already ran against the same mock
  // capture): the SAME instant, described by `t0` (this client's own survey floor, round-tripped
  // through `SurfaceOrigin.edgeNs` as nanoseconds — a plain JS number, and ~1.79e18 ns already
  // exceeds Number.MAX_SAFE_INTEGER, 2^53 ≈ 9.007e15, by ~200x) and by `began`
  // (`horizon.recording_began_s`, echoed straight off the server's own record) came back ~200 ns
  // apart — nowhere near equal by `Number.isEqual`, but the same second down to the precision the
  // representation can carry at all. The pre-fix epsilon was a flat 1e-9 SECONDS (1 ns): three
  // orders of magnitude too tight for this, and BLOCKING, not cosmetic — `complete: false` here is
  // what freezes a historical surface's tile route forever (T-580's "coverage first" gate; see
  // `surface-preview.test.ts`'s host-level case below for the frozen-forever half of the claim).
  const began = 1790369204.4648728;
  const t0 = 1790369204.464873; // ~200 ns later than `began` — exactly the measured drift.
  const s = decodeSurvey(coverage(12.8 * MHZ, 16, 2, () => "unobserved",
    { oldest_record_s: began, recording_began_s: began, forgotten: null, as_of_s: began + 1024 },
    t0))!;
  assert.equal(s.complete, true,
    `t0 (${t0}) is only ~200 ns after began (${began}) — float64-ns round-trip noise, not a real gap; ` +
    "complete must not read false over it");
  // The control: a REAL gap — whole seconds, not float noise — must still read incomplete. The fix
  // is a wider epsilon, not a broken one; this is what keeps rule 3a meaning something.
  const real = decodeSurvey(coverage(12.8 * MHZ, 16, 2, () => "unobserved",
    { oldest_record_s: began, recording_began_s: began, forgotten: null, as_of_s: began + 1024 },
    began + 5))!;
  assert.equal(real.complete, false, "a genuine 5 s gap between t0 and began must still read incomplete");
});

test("grey only up to the survey's horizon: above it the pane stays PENDING, not grey", async () => {
  const { g, surface, asked } = harness(false);
  // as_of 100 s: the survey says nothing about [100, 256) s.
  surface.setSurvey(decodeSurvey(coverage(12.8 * MHZ, 16, 2, () => "unobserved",
    { oldest_record_s: 0, recording_began_s: 0, forgotten: null, as_of_s: 100 })));
  await frames(surface, 1);
  assert.equal(asked.length, 0);
  const draws = g.ops.filter((o) => o.kind === "draw" && o.units?.[1]?.data?.[0] === CELL.UNOBSERVED && o.units[1].w === 1);
  assert.ok(draws.length > 0);
  // uRect's y1 is the top of the grey in the pane's clip space: 100 s of a 256 s pane.
  const top = 2 * (100 / 256) - 1;
  for (const d of draws) assert.ok(Math.abs(d.u!.uRect[3] - top) < 1e-9, `grey drawn to ${d.u!.uRect[3]}, past the survey horizon ${top}`);
  assert.ok(surface.lastFrame[0].shortNs > 0, "the strip above the horizon is reported as not drawn");
});

// ——— the host: the request it builds, and that it builds it FIRST ———

const LIVE_LAT: Lattice = { scheme: "view", cells: 256, f0Hz: 6250, t0Ns: 1e9, levelsF: 20, levelsT: 15 };
const BOUNDS = { f0Hz: 0, f1Hz: 12.8 * MHZ, t0Ns: 0, t1Ns: 1024 * S };

function probe(): SurfaceProbe {
  return {
    lattice: LIVE_LAT,
    origin: { bounds: BOUNDS, edgeNs: 1024 * S, provenance: { freq: "test", time: "test" } },
    census: { observed: 0, unobserved: 4096, unknown: 0, total: 4096, box: null },
    opening: { freq: { centerHz: 3.2 * MHZ, spanHz: 6.4 * MHZ }, centerNs: 900 * S, spanNs: 200 * S, onCoverage: false },
    range: { lo: -95, hi: -45, source: "test" },
    note: "test", requests: [], degraded: [],
  } as SurfaceProbe;
}

function hostHarness(answer: SurveyResponse) {
  const g = stubGl(1200, 600);
  const tiles: string[] = [];
  const surveys: string[] = [];
  let release: () => void = () => {};
  const gate = new Promise<void>((r) => { release = r; });
  const preview = new SurfacePreview({
    canvas: g.canvas, probe: probe(), token: "t", chrome: null, minimapPx: 120,
    fetchFn: async (url: string) => {
      tiles.push(url);
      return { ok: true, status: 200, statusText: "OK", json: async () => ({}) };
    },
    survey: async (path: string) => { surveys.push(path); await gate; return answer; },
    now: () => 0,
  });
  return { preview, tiles, surveys, release };
}

test("the host asks the coverage map FIRST, over the whole surface, and a fully-grey surface then asks for no tile", async () => {
  const { preview, tiles, surveys, release } = hostHarness(coverage(12.8 * MHZ, 16, 2, () => "unobserved"));
  preview.frame(); await flush();
  preview.frame(); await flush();
  assert.deepEqual(surveys, [surveyUrl(0, 12.8 * MHZ, 0, 1024 * S)], "one survey request, over the surface's bounds");
  assert.equal(tiles.length, 0, "no tile request may leave before the coverage map has answered");
  release();
  for (let i = 0; i < 6; i++) { preview.frame(); await flush(); }
  assert.equal(tiles.length, 0, `a fully-grey surface (panes AND minimap) asked for tiles: ${tiles.join(" ")}`);
  assert.equal(surveys.length, 1, "a historical surface's first survey stays true: it asks once");
  preview.dispose();
});

test("the survey request names what docs/api.md's GET /api/coverage takes", () => {
  const u = new URL(surveyUrl(1e6, 6e9, 5 * S, 65 * S), "http://x");
  assert.equal(u.pathname, "/api/coverage");
  assert.deepEqual(Object.fromEntries(u.searchParams), { f_lo: "1000000", f_hi: "6000000000", cells: "2048", rows: "2", t0: "5", t1: "65" });
  assert.ok(2048 * 2 <= 4096, "within the route's cell cap");
});

// ——— T-905: every lane that starts a request consults the survey, not only the renderer ———
//
// The fog-of-war e2e saw the pane request a tile over never-swept band C about 1 run in 9. The
// renderer's own misses were gated by T-580, but T-538's pan look-ahead (`prefetchAhead`) issues
// a speculative read straight to the route — and it fires exactly when the cache is IDLE, which a
// pane over never-sampled spectrum always is, because the survey answered every place it draws.
// So a pan across grey spectrum (or any pan while the survey is still on its way) asked for the
// tile one step ahead of the gesture, over spectrum the coverage map settles as never sampled.

test("T-905: panning a frozen pane over never-sampled spectrum requests nothing — before the survey lands (delayed answer) or after", async () => {
  const { preview, tiles, surveys, release } = hostHarness(coverage(12.8 * MHZ, 16, 2, () => "unobserved"));
  const pane = preview.activePane;
  preview.frame(); await flush();
  preview.frame(); await flush();
  assert.equal(surveys.length, 1);
  // The coverage answer is still on its way: drag the pane frame by frame.
  for (let i = 0; i < 6; i++) {
    preview.view.panes.panFreq(pane, 0.4 * MHZ);
    preview.frame(); await flush();
  }
  assert.equal(tiles.length, 0, `a tile left while the survey was still awaited: ${tiles.join(" ")}`);
  release();
  for (let i = 0; i < 3; i++) { preview.frame(); await flush(); }
  // The survey has answered "never sampled, everywhere": a drag over it is still free.
  for (let i = 0; i < 6; i++) {
    preview.view.panes.panFreq(pane, -0.4 * MHZ);
    preview.frame(); await flush();
  }
  assert.equal(tiles.length, 0, `the pan look-ahead asked for tiles over spectrum the survey settles as never sampled: ${tiles.join(" ")}`);
  preview.dispose();
});
