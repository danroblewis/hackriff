// **T-1057: a VISIBLE tile is never abandoned.**
//
// The user, 2026-09-25 (via the supervisor): *"Sometimes there are black bars in the waterfall,
// representing tiles that haven't been loaded yet; sometimes those never load. If a tile fails to load
// at all it should be re-requested. It seems like they are getting abandoned. Left alone long enough,
// all tiles on the screen should load. I don't think we should ever see the black tiles."*
//
// The invariant, which `ui/src/surface/retry.ts` and `docs/23` also state:
//
// > Every pending VISIBLE address is re-requested with jittered backoff until it is served or the
// > route states it does not exist. The coverage survey may turn a place GREY; it may never leave it
// > PENDING. A retune refreshes the survey before it may veto a request.
//
// Four abandonment paths were found and are pinned here, each in the terms of the surface the user
// looks at rather than in terms of a private field:
//
//  1. `tilecache.ts`'s `failed()` — every status the route could emit was permanent (T-479), and the
//     route uses `400` for both "no such address" and "this level cannot be folded *yet*".
//  2. an unreadable answer — including `tilebatch.ts`'s "the batch named no entry for this address",
//     which is a defect in the answer and not a fact about the place.
//  3. the coverage survey settling a place it has no evidence *inside*: neither drawn nor requested.
//  4. a retune, after which the survey in hand is stale about the band just tuned to — and on a
//     surface with nothing following the live edge, no further survey is ever asked for.
//
// Asserted **on the requests the client builds and on what the pane reports drawing** (T-367), on a
// fake clock, with a seeded fault generator: no wall clock and no randomness that a rerun cannot
// reproduce.

import { test } from "node:test";
import assert from "node:assert/strict";
import { CELL, PENDING } from "../src/surface/cellrule";
import { extentOf, keyOf, tilesFor, type Lattice, type TileAddr } from "../src/surface/lattice";
import { Surface, type PaneView } from "../src/surface/surface";
import { TileCache } from "../src/surface/tilecache";
import { TileBusyError, TileDecodeError, type TileData } from "../src/surface/tile";
import { SurfacePreview, type SurfaceProbe } from "../src/surface/preview";
import { RETRY_BASE_MS, RETRY_MAX_MS, retryDelayMs } from "../src/surface/retry";
import { decodeSurvey, surveyUrl, type SurveyResponse } from "../src/surface/survey";
import { batchedTileSource } from "../src/surface/tilebatch";
import { stubGl } from "./surface-glstub";

const LAT: Lattice = { scheme: "view", cells: 256, f0Hz: 6250, t0Ns: 1e9, levelsF: 20, levelsT: 15 };
const MHZ = 1e6, S = 1e9;
const W = 800, H = 600;
const flush = () => new Promise((r) => setImmediate(r));

function data(a: TileAddr): TileData {
  return {
    addr: a, key: keyOf(a), nf: 2, nt: 2, t1Ns: null,
    value: new Float32Array([-90, -80, -70, -60]),
    state: new Uint8Array([CELL.OBSERVED, CELL.OBSERVED, CELL.OBSERVED, CELL.OBSERVED]),
    tier: "spectrum-history", answeredLevel: 0, fold: { frequency: "exact", time: "exact" },
    measured: { nf: 2, nt: 2 }, rangeDb: { lo: -100, hi: -60 }, bytes: 12,
    serverInFlightLimit: null, serverInFlightShare: null,
  };
}

/** A pane over `[0, 6.4 MHz) × [0, 256 s)`. Several tiles wide at whatever level the renderer picks
 * for it, which is the level the assertions below read off the pane's own report. */
const PANE: PaneView = { id: "p", rect: { x: 0, y: 0, w: W, h: H }, box: { f0Hz: 0, f1Hz: 4.8 * MHZ, t0Ns: 0, t1Ns: 512 * S } };

/** A deterministic `[0, 1)` generator — a fault schedule a failing run can be replayed from. */
function prng(seed: number): () => number {
  let x = seed >>> 0;
  return () => {
    x ^= x << 13; x >>>= 0;
    x ^= x >>> 17;
    x ^= x << 5; x >>>= 0;
    return x / 0x1_0000_0000;
  };
}

const httpError = (status: number, msg = "refused") => Object.assign(new Error(msg), { status });

/**
 * The five ways a tile answer goes wrong in the field, one per index — every one of them a thing a
 * real deployment produced, and every one of them a path that used to end the asking:
 * a dropped connection, the route's backpressure, a transient `400`, a `500`, and a body that did
 * not decode (a truncated response, or `tilebatch`'s "named no entry").
 */
const FAULTS: readonly ((k: string) => unknown)[] = [
  () => new TypeError("Failed to fetch"),
  () => new TileBusyError(4, "too many tile reads in flight (limit 4, share 2, held 0)", 2, 0),
  () => httpError(400, "this tile's level cannot be built from the levels below it"),
  () => httpError(500, "history store poisoned"),
  (k) => new TileDecodeError(`batch answer named no entry for ${k}`),
];

// ——— 1. fault injection: 30 % of answers lost, and the pane still fills ———

test("30 % of tile answers dropped/refused/unreadable: every VISIBLE address is served in the end, and no PENDING quad is left", async () => {
  // The acceptance the ticket names. Faults are chosen from a seeded schedule so a red run is
  // reproducible; the loop is a render loop on a fake clock, which is the only clock this may read.
  let clock = 0;
  const rand = prng(0xC0FFEE);
  const asked: string[] = [];
  const faults: string[] = [];
  const g = stubGl(W, H);
  const surface = new Surface(
    g.canvas, LAT,
    (tex) => new TileCache(tex, (a) => {
      const k = keyOf(a);
      asked.push(k);
      if (rand() < 0.3) {
        const e = FAULTS[faults.length % FAULTS.length](k);
        faults.push(`${clock} ${k} ${(e as Error).name}`);
        return Promise.reject(e);
      }
      return Promise.resolve(data(a));
    }, { inFlight: 4, now: () => clock, retryJitter: rand, budgetBytes: 1 << 20 }),
  );
  surface.setScale(-100, -60);

  // **What "every visible address" means is read off the pane's own report**, not guessed: the
  // renderer chooses the level from the box and the rectangle, and asserting over a level it did not
  // draw at would prove nothing about the pixels the user sees.
  surface.render([PANE]);
  await flush();
  const rep0 = surface.lastFrame[0];
  const wanted = new Set([...tilesFor(LAT, PANE.box, rep0.levelF, rep0.levelT, "any")].map(keyOf));
  assert.ok(wanted.size >= 4, `the pane must draw several tiles for this to prove anything: ${wanted.size}`);

  // 20 s of a 20 Hz render loop. The ladder's own ceiling is RETRY_MAX_MS = 30 s, so the assertion is
  // not "it eventually retried once" — it is that nothing is left owed at the end.
  const served = () => [...wanted].every((k) => surface.cache.isResident(parseAddr(k)));
  let frames = 0;
  for (; frames < 400 && !served(); frames++) {
    surface.render([PANE]);
    await flush();
    clock += 50;
  }
  // The final frame on its own: `g.ops` accumulates over the run, so the pixel-level claim below is
  // made about the ops of this one frame and nothing earlier.
  const mark = g.ops.length;
  surface.render([PANE]);
  await flush();
  const rep = surface.lastFrame[0];
  const missing = [...wanted].filter((k) => !surface.cache.isResident(parseAddr(k)));
  assert.ok(faults.length >= 3,
    `only ${faults.length} answers were dropped in ${asked.length} requests — the fault schedule must ` +
    "actually bite, or this test passes for the wrong reason");
  assert.deepEqual(missing, [],
    `after ${frames} frames (${clock} ms) these VISIBLE places were never served: ${missing.join(", ")} ` +
    `— ${asked.length} requests, ${faults.length} faults [${faults.join("; ")}], ` +
    `${surface.cache.retryingPlaces} still on the ladder, ${surface.cache.terminalPlaces} written off`);
  assert.equal(rep.pending, 0, "a pane left with a PENDING quad is the black bar the user reported");
  assert.equal(rep.refused, 0, "…and a refused mark that never clears is the same defect wearing a label");
  assert.equal(surface.cache.terminalPlaces, 0,
    "nothing here is the route saying a place does not exist, so nothing may be written off");

  // And the last frame drew no flat PENDING quad at all — the pixel-level reading of the same claim.
  // `uKind` as well as the colour: a uniform keeps its value until it is written again, so a tile
  // draw after any flat one still carries the last `uFlat` — the mark is the pair, and `uKind == 1`
  // (`KIND_FLAT`) is what says this quad is a flat mark rather than a tile.
  const pendingQuads = g.ops.slice(mark).filter((o) => o.kind === "draw"
    && o.u?.uKind?.[0] === 1
    && !!o.u.uFlat
    && Math.abs(o.u.uFlat[0] - PENDING[0]) < 1e-6
    && Math.abs(o.u.uFlat[1] - PENDING[1]) < 1e-6
    && Math.abs(o.u.uFlat[2] - PENDING[2]) < 1e-6);
  assert.equal(pendingQuads.length, 0, `the final frame drew ${pendingQuads.length} PENDING quads`);
  const refusedQuads = g.ops.slice(mark).filter((o) => o.kind === "draw" && o.u?.uKind?.[0] === 2);
  assert.equal(refusedQuads.length, 0, `the final frame drew ${refusedQuads.length} REFUSED quads`);
});

/** `keyOf`'s inverse, enough for this file: the key's own fields in order. */
function parseAddr(key: string): TileAddr {
  const p = key.split("|");
  return {
    device: p[0], scheme: p[1], levelF: Number(p[2]), levelT: Number(p[3]),
    fIndex: Number(p[4]), tIndex: Number(p[5]), cells: Number(p[6]),
  };
}

test("the asking is paced, not a flood: a place refused twice is asked at most once per doubling wait", async () => {
  // The other half of the invariant, and the reason T-479 existed. "Never abandoned" must not be
  // bought with the 157-requests-in-700-ms storm; the ladder is what makes both true at once.
  let clock = 0;
  const asked: string[] = [];
  const g = stubGl(W, H);
  const surface = new Surface(
    g.canvas, LAT,
    (tex) => new TileCache(tex, (a) => {
      asked.push(keyOf(a));
      return Promise.reject(httpError(500, "history store poisoned"));
    }, { inFlight: 4, now: () => clock, retryJitter: () => 0 }),
  );
  surface.setScale(-100, -60);
  const visible = new Set([...tilesFor(LAT, PANE.box, 0, 0, "any")].map(keyOf)).size;

  // 10 s of a 60 Hz loop against a route that refuses everything: 600 frames, and the ladder's steps
  // inside 10 s are 0.5 + 1 + 2 + 4 + 8, so a place is attempted at 0, 0.5, 1.5, 3.5 and 7.5 s — five
  // times, against 600 chances to ask. Counted per PLACE asked for (the pane's own tiles plus the
  // parent pin's, which is a lane of its own), because the claim is about the pacing of the ladder.
  for (let i = 0; i < 600; i++) { surface.render([PANE]); await flush(); clock += 16; }
  const places = new Set(asked).size;
  const perPlace = asked.length / places;
  assert.ok(perPlace <= 6, `${perPlace.toFixed(1)} requests per place in 10 s — the render loop is pacing the wire`);
  assert.ok(perPlace >= 3, `only ${perPlace.toFixed(1)} requests per place in 10 s: the ladder stopped asking`);
  // Still owed, still not written off: this is a place that would be served the moment the route was.
  assert.equal(surface.cache.terminalPlaces, 0);
  assert.ok(surface.cache.retryingPlaces >= visible, "every refused place is on the ladder");
});

test("the ladder's arithmetic: doubling, capped, and jitter that can only ever add", () => {
  const zero = () => 0, top = () => 0.999_999;
  assert.equal(retryDelayMs(1, zero), RETRY_BASE_MS);
  assert.equal(retryDelayMs(2, zero), 2 * RETRY_BASE_MS);
  assert.equal(retryDelayMs(8, zero), Math.min(RETRY_MAX_MS, 128 * RETRY_BASE_MS));
  assert.equal(retryDelayMs(40, zero), RETRY_MAX_MS, "capped, so a long outage does not become an hour");
  assert.equal(retryDelayMs(400, zero), RETRY_MAX_MS, "…and the exponent cannot overflow into Infinity");
  assert.ok(retryDelayMs(1, top) > RETRY_BASE_MS && retryDelayMs(1, top) <= 1.5 * RETRY_BASE_MS);
  for (const bad of [() => NaN, () => -1, () => 1, () => Infinity]) {
    assert.equal(retryDelayMs(1, bad), RETRY_BASE_MS,
      "a broken random contributes no jitter — never a shorter wait, never a wild one");
  }
});

// ——— 2. the batch layer: no entry of an answer is ever dropped ———

test("a batch entry the answer never named is RE-ASKED, not written off (and `remaining` is re-issued)", async () => {
  // The ticket's fourth place. `remaining` was already re-queued per address
  // (`surface-tilebatch.test.ts`: "a truncated batch re-asks for exactly what `remaining` named");
  // what was lost is the address a batch answer simply did not mention. It rejects as a decode
  // failure — correctly, because that is not a coverage answer — and a decode failure used to be
  // terminal, so the place was black for the session over a defect in one answer.
  const urls: string[] = [];
  const source = batchedTileSource("tok", (async (url: string) => {
    urls.push(url);
    const s2 = new URLSearchParams(url.split("?")[1]).get("addresses")!.split(",");
    // Answer the first address, leave the second unreached, and say nothing at all about the third.
    const body = urls.length === 1
      ? { requested: 3, returned: 1, truncated: true, remaining: [s2[1]],
          tiles: [{ address: { spelling: s2[0] }, status: 200, tile: tileBody() }] }
      : { requested: s2.length, returned: s2.length, truncated: false, remaining: [],
          tiles: s2.map((x) => ({ address: { spelling: x }, status: 200, tile: tileBody() })) };
    return { ok: true, status: 200, statusText: "OK", json: async () => body };
  }) as never);

  const addrs: TileAddr[] = [0, 1, 2].map((f) =>
    ({ device: "any", scheme: "view", levelF: 0, levelT: 0, fIndex: f, tIndex: 7, cells: 2 }));
  const outcome = addrs.map((a) => source(a).then(() => "served", (e) => `failed: ${String(e)}`));
  for (let i = 0; i < 8; i++) await flush();
  const settled = await Promise.all(outcome);

  assert.equal(settled[0], "served", "the address the batch answered resolves");
  assert.equal(settled[1], "served", "the `remaining` address was re-issued per address, not dropped");
  assert.match(String(settled[2]), /named no entry/,
    "an address the answer never named must be rejected so the cache re-asks it — never resolved to a claim");
  assert.ok(urls.length >= 2, `the unreached address must cost a second request: ${urls.join(" ")}`);
  assert.ok(new URLSearchParams(urls[1].split("?")[1]).get("addresses")!.split(",").length === 1,
    "…and that request names only the address that was not reached");

  // The other end of the same rejection: the cache does not write the place off for it.
  const cache = new TileCache<{ id: number }>(
    { upload: () => ({ id: 1 }), destroy: () => {} },
    () => Promise.reject(new TileDecodeError("batch answer named no entry for any|view|0|0|2|7|2")),
    { inFlight: 2, now: () => 0, retryJitter: () => 0 },
  );
  cache.beginFrame(); cache.acquire(addrs[2]); cache.endFrame();
  await flush();
  assert.equal(cache.terminalPlaces, 0, "a batch answer that named no entry must not write the place off");
  assert.equal(cache.retryingPlaces, 1, "…it puts it on the ladder");
});

/** A tile body complete enough for `decodeTile` — the shape `surface-tilebatch.test.ts` pins. */
function tileBody(n = 2): unknown {
  const cells = n * n;
  return {
    key: { level_f: 0, level_t: 0, f_index: 0, t_index: 7, cells: n },
    extent: { nf: n, nt: n, f_lo_hz: 0, f_hi_hz: 100, f_cell_hz: 50, t0_s: 0, t1_s: 1, t_cell_s: 0.5 },
    axes: { frequency: { cell_hz: 50, levels: 1, max_level: 0, tile_hz: 100 },
            time: { cell_s: 0.5, levels: 1, max_level: 0, tile_s: 1 } },
    grid: { nf: n, nt: n, cells, observed_cells: cells, encoding: { planes: "json" },
            max_db: Array.from({ length: cells }, () => -80) },
    coverage: { states: ["observed"], grid: { nf: n, nt: n }, planes: [{ cells, runs: [0, cells] }],
                any: { plane: 0 }, selected: { plane: 0, present: true } },
    resolution: { source: "live-iq", live: true, answered: 0, statement: "…",
                  fold: { frequency: { direction: "exact" }, time: { direction: "exact" } } },
    cost: { build_ms: 1, source_cells: 4, chunks: 1 },
  };
}

// ——— 3. the coverage survey may turn a place GREY; it may never leave it PENDING ———

/** A `GET /api/coverage` answer over `[0, 1024) s × [0, fHi)`, every cell `unobserved`, with the
 * evidence horizon (`as_of_s`) the caller names — the one number this section turns on. */
function unobserved(fHi: number, asOfS: number, t0 = 0): SurveyResponse {
  const cells = 16, rows = 2, dt = 1024 / rows, df = fHi / cells;
  const list: { state: string }[] = [];
  for (let r = 0; r < rows; r++) for (let c = 0; c < cells; c++) list.push({ state: "unobserved" });
  return {
    window: { t0_s: t0, t1_s: t0 + 1024 },
    grid: { cells, rows, f_lo_hz: 0, f_cell_hz: df, t0_s: t0, t_cell_s: dt },
    any: { cells: list },
    horizon: { oldest_record_s: 0, recording_began_s: 0, forgotten: null, as_of_s: asOfS },
  };
}

function surfaceHarness() {
  const asked: TileAddr[] = [];
  const g = stubGl(W, H);
  const surface = new Surface(
    g.canvas, LAT,
    (tex) => new TileCache(tex, (a) => { asked.push(a); return Promise.resolve(data(a)); },
      { inFlight: 64, now: () => 0 }),
  );
  surface.setScale(-100, -60);
  return { g, surface, asked };
}

test("a survey with no evidence INSIDE a place may not veto its request: grey or fetch, never neither", async () => {
  // The path that produces a black bar from the coverage side. `unobservedThrough` answers the
  // survey's own horizon (`as_of_s`), and a place that begins *after* that horizon is settled as
  // never sampled while the survey can draw no grey for it at all — so before T-1057 it was neither
  // drawn nor requested. A pane over it showed its PENDING ground for as long as the survey stayed
  // that far behind, which on a surface with nothing following is the whole session.
  const { surface, asked } = surfaceHarness();
  // The pane draws [0, 256) s. as_of = 0 s: the survey's evidence stops before the pane begins.
  surface.setSurvey(decodeSurvey(unobserved(12.8 * MHZ, 0)));
  for (let i = 0; i < 4; i++) { surface.render([PANE]); await flush(); }
  assert.ok(asked.length > 0,
    "the survey settled every place in the pane while having no evidence inside any of them, and the " +
    "pane requested nothing: that is a black bar with nothing coming for it");
  assert.equal(surface.lastFrame[0].surveyed, 0, "…and none of them was answered as grey");

  // The control, and the saving T-580 exists for: a survey whose evidence reaches into the pane
  // still vetoes every request and draws the grey itself.
  const { surface: s2, asked: a2 } = surfaceHarness();
  s2.setSurvey(decodeSurvey(unobserved(12.8 * MHZ, 1024)));
  for (let i = 0; i < 4; i++) { s2.render([PANE]); await flush(); }
  assert.equal(a2.length, 0, `never-sampled spectrum must still cost no round trip: ${a2.map(keyOf)}`);
  assert.ok(s2.lastFrame[0].surveyed > 0, "…and the survey draws it");
});

test("a retune: the survey in hand may not veto a request about the band just tuned to", async () => {
  // Four surfaces, one claim each, because the claim is *which requests the client builds* and a
  // place already served is no longer evidence about whether it would be asked for (T-367's rule
  // applied to the setup, not only to the assertion).
  const run = async (prepare: (s: Surface) => void) => {
    const h = surfaceHarness();
    h.surface.setSurvey(decodeSurvey(unobserved(12.8 * MHZ, 1024)));
    prepare(h.surface);
    for (let i = 0; i < 4; i++) { h.surface.render([PANE]); await flush(); }
    return h;
  };

  // A. the precondition and the saving: a survey whose evidence covers the pane vetoes every request.
  const a = await run(() => {});
  assert.equal(a.asked.length, 0, `never-sampled spectrum must cost no round trip: ${a.asked.map(keyOf)}`);
  assert.ok(a.surface.lastFrame[0].surveyed > 0, "…and the survey draws the grey itself");

  // B. the radio is retuned at 100 s. Every place that reaches past that instant is owed a request:
  // the survey was taken while the radio was somewhere else, so it cannot speak for those rows.
  const b = await run((s2) => s2.noteRetune(100 * S));
  assert.equal(b.surface.surveyStaleFrom, 100 * S);
  assert.ok(b.asked.length > 0, "after a retune the newly tuned band's own tiles must be requestable");
  for (const addr of b.asked) {
    assert.ok(extentOf(LAT, addr).t1Ns > 100 * S,
      `requested ${keyOf(addr)}, which ends before the retune — the old survey is still good about ` +
      "that, and re-fetching the whole past is the cost T-580/T-905 exist to avoid");
  }

  // C. a survey that was already in flight when the radio retuned lands afterwards while describing
  // the tuning before it. It is not evidence about the new band and must not restore the veto.
  const c = await run((s2) => {
    s2.noteRetune(100 * S);
    s2.setSurvey(decodeSurvey(unobserved(12.8 * MHZ, 90)));
  });
  assert.equal(c.surface.surveyStaleFrom, 100 * S, "an answer whose evidence predates the retune lifts nothing");
  assert.ok(c.asked.length > 0, "…so the places are still owed their requests");

  // D. a survey whose evidence reaches past the retune is the fresh answer, and the veto is back.
  const d = await run((s2) => {
    s2.noteRetune(100 * S);
    s2.setSurvey(decodeSurvey(unobserved(12.8 * MHZ, 1024)));
  });
  assert.equal(d.surface.surveyStaleFrom, null, "a survey that reaches past the retune lifts the suspension");
  assert.equal(d.asked.length, 0, "and never-sampled spectrum costs no round trip again");
  assert.ok(d.surface.lastFrame[0].surveyed > 0);
});

// ——— 4. the host: a retune asks for a fresh survey NOW, even with nothing following ———

const BOUNDS = { f0Hz: 0, f1Hz: 12.8 * MHZ, t0Ns: 0, t1Ns: 1024 * S };

function probe(): SurfaceProbe {
  return {
    lattice: LAT,
    origin: { bounds: BOUNDS, edgeNs: 1024 * S, provenance: { freq: "test", time: "test" } },
    census: { observed: 0, unobserved: 4096, unknown: 0, total: 4096, box: null },
    opening: { freq: { centerHz: 3.2 * MHZ, spanHz: 6.4 * MHZ }, centerNs: 900 * S, spanNs: 200 * S, onCoverage: false },
    range: { lo: -95, hi: -45, source: "test" },
    note: "test", requests: [], degraded: [],
  } as SurfaceProbe;
}

test("SurfacePreview.retuned(): drops the edge tiles, suspends the veto AND asks for a fresh survey at once", async () => {
  // The half a following pane hides. `maybeSurvey`'s cadence for a surface with nothing following the
  // live edge is POSITIVE_INFINITY — one survey is the whole answer — so "the next survey lifts the
  // suspension" would be "the session ends with it suspended", and the saving would be lost for good.
  const tiles: string[] = [];
  const surveys: string[] = [];
  const g = stubGl(1200, 600);
  const preview = new SurfacePreview({
    canvas: g.canvas, probe: probe(), token: "t", chrome: null, minimapPx: 120,
    fetchFn: async (url: string) => {
      tiles.push(url);
      return { ok: true, status: 200, statusText: "OK", json: async () => ({}) } as unknown as Response;
    },
    survey: async (path: string) => { surveys.push(path); return unobserved(12.8 * MHZ, 1024); },
    now: () => 0,
  });
  for (let i = 0; i < 6; i++) { preview.frame(); await flush(); }
  assert.deepEqual(surveys, [surveyUrl(0, 12.8 * MHZ, 0, 1024 * S)], "one survey, over the surface's bounds");
  assert.equal(tiles.length, 0, "a fully-grey surface asks for no tile");

  preview.retuned();
  for (let i = 0; i < 6; i++) { preview.frame(); await flush(); }
  assert.equal(surveys.length, 2,
    "a retune must ask for a fresh survey immediately: on this surface nothing else ever will");
  preview.dispose();
});
