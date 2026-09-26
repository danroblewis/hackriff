// T-1037: **tiles reveal coarse-first, then atomically per row — never black.**
//
// The user, 2026-09-25, on the shipped client: *"tiles come in at random times, they don't all show
// at once … a lot of random black areas, which make it look very broken … we don't prefetch the
// multiple levels of the pyramid … so much UI jankiness, especially on an unstable network
// connection."*
//
// Four claims, each of which the code this replaces fails:
//
//   1. **A viewport change asks for the coarsest level covering it, as ONE batch, and draws it at
//      once.** Only the parent level (one step) was pinned, so a cold viewport had nothing to stand
//      in for its tiles and every one of them was a PENDING quad — the black. Tests 1–3.
//   2. **PENDING is drawn only where no ancestor exists at ANY level.** The reach was three steps per
//      axis; after a deep zoom the copy in hand is further than that, and was passed over in favour
//      of black. Test 4 proves it *and* re-injects the old cap in the same test. Test 5 is the honest
//      case — nothing anywhere — which must still be PENDING.
//   3. **A tile row reveals as a whole: all of it, or (after ~300 ms) what arrived with stand-ins
//      under the rest.** Tests 6–8. A row whose last tile has not landed draws *none* of its own
//      tiles and then *all* of them: one swap per row, never one per tile — and the other row is
//      unaffected, because the row is the unit.
//   4. **The stand-in set is asked for in a request of its OWN** (test 9): a batch answers when its
//      slowest member does, so a stand-in riding beside the cold tiles it stands in for arrives no
//      sooner than they do and the coarse-first order is a no-op.
//
// NOT asserted here, and deliberately: the acceptance's *"requests per viewport change <=
// ceil(tiles/64) + 1"*. Reaching that bound means putting ~64 addresses on the wire at once, and
// `ui/e2e/surface-nav.e2e.mjs`'s T-846 assertion (`addressPeak(tileReqs) <= cost.in_flight_limit`,
// line 571) forbids exactly that: *"The route's cap is per address and so is the client's budget:
// batching changes how many requests carry them, not how many may be asked for."* The two cannot
// both hold, so that half is a decision, not a diff — see the ticket's hand-back.
//
// Every assertion is either a `PaneReport` count or a rasterised pixel, and the PENDING ones are
// pixels: "no black" is a claim about the screen.

import test from "node:test";
import assert from "node:assert/strict";

import { CELL, PENDING } from "../src/surface/cellrule";
import {
  COARSE_COVER_TILES, TILES_BATCH_MAX_ADDRESSES, coarsestCovering, keyOf, oneTier, tierFor,
  type Lattice, type TileAddr,
} from "../src/surface/lattice";
import { Surface, type PaneView } from "../src/surface/surface";
import { TileCache } from "../src/surface/tilecache";
import { batchedTileSource } from "../src/surface/tilebatch";
import type { TileData } from "../src/surface/tile";
import { stubGl } from "./surface-glstub";
import { countColour, rasterize, type Rect } from "./surface-raster";

const LAT: Lattice = { scheme: "view", cells: 256, f0Hz: 6250, t0Ns: 1e9, levelsF: 20, levelsT: 15 };
const TILE_HZ = LAT.f0Hz * LAT.cells;
const TILE_NS = LAT.t0Ns * LAT.cells;
/** The user's own window (the acceptance names 1280x800). */
const W = 1280, H = 800;
const FULL: Rect = { x: 0, y: 0, w: W, h: H };
const LO = -100, HI = -60;

function tile(a: TileAddr, nf = 2, nt = 2): TileData {
  const cells = nf * nt;
  return {
    addr: a, key: keyOf(a), nf, nt, t1Ns: null, asOfNs: null,
    value: Float32Array.from({ length: cells }, (_, i) => LO + ((i % 5) + 1) * 5),
    state: Uint8Array.from({ length: cells }, () => CELL.OBSERVED),
    tier: "spectrum-history", answeredLevel: a.levelF,
    fold: { frequency: "exact", time: "exact" },
    measured: { nf, nt }, rangeDb: null, bytes: 4096,
    serverInFlightLimit: null, serverInFlightShare: null,
  };
}

const flush = (): Promise<void> => new Promise((r) => { setImmediate(r); });

/** A pane filling the canvas, over `fTiles x tTiles` **level-0** tiles of the lattice. */
const paneOver = (fTiles: number, tTiles: number, w = W, h = H): PaneView => ({
  id: "p", rect: { x: 0, y: 0, w, h },
  box: { f0Hz: 0, f1Hz: fTiles * TILE_HZ, t0Ns: 0, t1Ns: tTiles * TILE_NS },
});

/** What the surface itself will address that pane at — derived, never assumed. */
const viewOf = (p: PaneView) => tierFor(oneTier(LAT), p.box, p.rect.w, p.rect.h);

/**
 * A surface over a source that answers **only the addresses `answer` accepts** and leaves the rest
 * outstanding for ever — the cold-viewport state this ticket is about, made deterministic.
 */
function harness(
  answer: (a: TileAddr) => boolean,
  opts: {
    readonly revealHoldMs?: number; readonly now?: () => number;
    readonly maxFallbackSteps?: number; readonly w?: number; readonly h?: number;
  } = {},
) {
  const g = stubGl(opts.w ?? W, opts.h ?? H);
  const asked: TileAddr[] = [];
  /** Requests still outstanding, so a test can land one late — which is what a row completing IS. */
  const outstanding: { addr: TileAddr; resolve: (d: TileData) => void }[] = [];
  const surface = new Surface(
    g.canvas, LAT,
    (tex) => new TileCache(tex, (a) => {
      asked.push(a);
      if (answer(a)) return Promise.resolve(tile(a));
      return new Promise<TileData>((resolve) => { outstanding.push({ addr: a, resolve }); });
    }, { now: () => 0 }),
    {
      // The parent pin is off throughout: it is T-573-era behaviour and would blur which request
      // each assertion is about. What is being tested is the coarse stand-in set and the row gate.
      pinParents: false,
      revealHoldMs: opts.revealHoldMs, now: opts.now, maxFallbackSteps: opts.maxFallbackSteps,
    },
  );
  surface.setScale(LO, HI);
  /** Answer the outstanding requests `pred` picks — the late tile that completes a row. */
  const deliver = (pred: (a: TileAddr) => boolean): number => {
    let n = 0;
    for (let i = outstanding.length - 1; i >= 0; i--) {
      if (!pred(outstanding[i].addr)) continue;
      const w = outstanding.splice(i, 1)[0];
      w.resolve(tile(w.addr));
      n++;
    }
    return n;
  };
  return { g, surface, asked, deliver };
}

/** Render until residency settles, then rasterise one more frame. */
async function settle(h: ReturnType<typeof harness>, panes: readonly PaneView[], frames = 8) {
  for (let i = 0; i < frames; i++) { h.surface.render(panes); await flush(); }
  h.g.reset();
  const reports = h.surface.render(panes);
  return { reports, fb: rasterize(h.g.ops, W, H) };
}

// ---------------------------------------------------------------------------------------------
// 1-3. The coarse stand-in: asked for, kept, and drawn instead of black.

test("the coarsest level covering a viewport is ONE batch of addresses, and covers all of it", () => {
  const p = paneOver(5, 4);
  const v = viewOf(p);
  assert.ok(v.addrs.length > COARSE_COVER_TILES, `${v.addrs.length} addresses — this pane must need a stand-in`);
  const coarse = coarsestCovering(LAT, p.box, v.levelF, v.levelT)!;
  assert.ok(coarse, "a viewport of more than one batch has a coarser level that covers it");
  assert.ok(coarse.addrs.length <= COARSE_COVER_TILES,
    `${coarse.addrs.length} addresses is not one request, so it would not arrive as one picture`);
  // Every address of the viewport is inside one of them: a stand-in that did not cover the whole
  // viewport would leave exactly the black it exists to remove.
  for (const a of v.addrs) {
    const fShift = 2 ** (coarse.levelF - a.levelF), tShift = 2 ** (coarse.levelT - a.levelT);
    assert.ok(
      coarse.addrs.some((c) => c.fIndex === Math.floor(a.fIndex / fShift) && c.tIndex === Math.floor(a.tIndex / tShift)),
      `${keyOf(a)} has no covering stand-in`);
  }
  // A viewport already inside one batch needs no second, coarser request: its own tiles ARE the batch.
  const small = paneOver(2, 1, 512, 256);
  const sv = viewOf(small);
  assert.ok(sv.addrs.length <= COARSE_COVER_TILES);
  assert.equal(coarsestCovering(LAT, small.box, sv.levelF, sv.levelT), null);
});

test("NEVER BLACK: with only the coarse stand-in in hand, not one PENDING pixel is drawn", async () => {
  const p = paneOver(5, 4);
  const v = viewOf(p);
  const coarse = coarsestCovering(LAT, p.box, v.levelF, v.levelT)!;
  // The stand-in lands; the viewport's own tiles never do. This is the cold reload, and it is the
  // state the user was looking at.
  const h = harness((a) => a.levelF === coarse.levelF && a.levelT === coarse.levelT);
  const { reports, fb } = await settle(h, [p]);
  const r = reports[0];

  assert.ok(h.asked.some((a) => a.levelF === coarse.levelF && a.levelT === coarse.levelT),
    "the coarsest covering level was actually requested — before T-1037 only the parent level was");
  assert.equal(r.pending, 0, "a place with a resident ancestor is never PENDING");
  assert.equal(r.fallbacks, v.addrs.length, "every one of the viewport's tiles was drawn from the stand-in");
  assert.equal(countColour(fb, PENDING, FULL), 0,
    "PENDING pixels are the black the user reported; with an ancestor in hand there must be none");
});

test("the coarse stand-in SURVIVES the frame that asked for it — cancellation knows the pane wants it", async () => {
  // `TileCache.setViewports` drops every queued address no viewport wants, and a viewport wants its
  // own level and ONE step coarser. The stand-in set is further up, so unless the viewport names it
  // the set is cancelled at the end of the very frame that asked for it and the black comes straight
  // back — silently, because a cancelled address is simply never drawn.
  const p = paneOver(5, 4);
  const v = viewOf(p);
  const coarse = coarsestCovering(LAT, p.box, v.levelF, v.levelT)!;
  const h = harness(() => false); // nothing answers: what matters is what stays asked for
  for (let i = 0; i < 3; i++) { h.surface.render([p]); await flush(); }
  assert.ok(h.asked.some((a) => a.levelF === coarse.levelF && a.levelT === coarse.levelT),
    "the stand-in set reached the wire");
  assert.equal(h.surface.cache.stats.cancelled, 0,
    "nothing this pane asked for was cancelled, so the stand-in set was not dropped by the predicate");
});

test("a pane on the OVERVIEW tier asks for no stand-in: at the coarse end the stand-in costs more than the tile", async () => {
  // T-450 measured a map-level read at 5.2 s and 9.5 MB against 11.4 ms for a fine tile, and the
  // fetch queue is LIFO, so a stand-in for an overview pane is fetched FIRST and spends the whole
  // budget on tiles coarser than the survey overview the pane is already drawing. Measured in
  // `ui/e2e/surface-nav.e2e.mjs` test 3: a "fit to coverage" pane went `1 tiles · 15 coarse
  // stand-ins` -> `0 tiles · 16` the moment the stand-in was asked for at that tier.
  // A shallow detail ladder, so a wide box CLAMPS at its ceiling and blows the tile budget — which
  // is what `tierFor` moves a pane to the overview tier on (T-505), a budget and never a span.
  const detail: Lattice = { ...LAT, levelsF: 3, levelsT: 2 };
  const overview: Lattice = { ...LAT, scheme: "overview", f0Hz: LAT.f0Hz * 64, t0Ns: LAT.t0Ns * 8 };
  const g = stubGl(W, H);
  const asked: TileAddr[] = [];
  const surface = new Surface(
    g.canvas, { detail, overview },
    (tex) => new TileCache(tex, (a) => { asked.push(a); return Promise.resolve(tile(a)); }, { now: () => 0 }),
    { pinParents: false },
  );
  surface.setScale(LO, HI);
  // Wide enough that the detail tier cannot draw it inside VIEWPORT_TILE_BUDGET, so `tierFor` moves
  // this pane to the overview lattice — the condition the rule is about, derived and not asserted.
  const wide = paneOver(400, 8);
  for (let i = 0; i < 6; i++) { surface.render([wide]); await flush(); }
  assert.equal(surface.lastFrame[0].tier, "overview", "this pane must be on the overview tier");
  const ov = tierFor({ detail, overview }, wide.box, W, H, "any", "overview");
  const coarser = asked.filter((a) => a.scheme === "overview" && a.levelF > ov.levelF + 1);
  assert.deepEqual(coarser.map(keyOf), [],
    "an overview pane asked for a level coarser than its own parent pin — the dearest reads in the " +
    "system, fetched before its own tiles, to stand in for the survey overview it already draws");
});

// ---------------------------------------------------------------------------------------------
// 4-5. PENDING only when no ancestor exists at any level.

test("AFTER A DEEP ZOOM the ancestor in hand stands in however far up it is — and the old cap goes black", async () => {
  // A wide view, then a zoom of several levels into a corner of it: the copies in hand are four to
  // six levels coarser than what the zoomed pane addresses. That is past any per-axis reach of three,
  // so the search passed them over and drew PENDING over places this client was holding an answer
  // for. The zoomed pane's own tiles never arrive here, so the ONLY thing that can be on the screen
  // is the ancestor.
  const wide = paneOver(80, 16);
  const deep = paneOver(5, 1);
  const wv = viewOf(wide), dv = viewOf(deep);
  assert.ok(wv.levelF - dv.levelF >= 4, `${wv.levelF} vs ${dv.levelF}: the zoom must be deeper than the old reach`);

  const answer = (a: TileAddr): boolean => a.levelF >= wv.levelF;
  const h = harness(answer);
  await settle(h, [wide]);              // the wide view's own tiles and its stand-in land
  const { reports, fb } = await settle(h, [deep]);   // …then zoom in, where nothing new answers
  assert.equal(reports[0].pending, 0,
    "a copy several levels up is an answer; drawing black over it is the bug the user saw");
  assert.equal(reports[0].fallbacks, dv.addrs.length);
  assert.equal(countColour(fb, PENDING, FULL), 0);

  // **The defect, re-injected**: the same frames with the old three-step reach, which is what
  // `maxFallbackSteps` still expresses. It must go black — otherwise this test proves nothing.
  const old = harness(answer, { maxFallbackSteps: 3 });
  await settle(old, [wide]);
  const was = await settle(old, [deep]);
  assert.equal(was.reports[0].pending, dv.addrs.length, "the capped reach finds nothing…");
  assert.ok(countColour(was.fb, PENDING, FULL) > 0, "…and paints the pane black over an answer in hand");
});

test("PENDING IS STILL DRAWN when no ancestor exists at any level — the one place it is true", async () => {
  const p = paneOver(5, 4);
  const v = viewOf(p);
  const h = harness(() => false);
  const { reports, fb } = await settle(h, [p]);
  assert.equal(reports[0].fallbacks, 0);
  assert.equal(reports[0].pending, v.addrs.length, "nothing anywhere: every place is waiting, and says so");
  assert.ok(countColour(fb, PENDING, FULL) > 0,
    "'not loaded' stays visible as itself — the fix is not to hide the state, it is to have an answer");
});

// ---------------------------------------------------------------------------------------------
// 6-8. One swap per row.

/** The pane, its addressing, its coarse stand-in, and the one address held back. */
function rowSetup() {
  const p = paneOver(5, 4);
  const v = viewOf(p);
  const coarse = coarsestCovering(LAT, p.box, v.levelF, v.levelT)!;
  const rows = new Map<number, TileAddr[]>();
  for (const a of v.addrs) {
    const row = rows.get(a.tIndex);
    if (row) row.push(a); else rows.set(a.tIndex, [a]);
  }
  assert.ok(rows.size >= 2, `${rows.size} rows — the row must be shown to be the UNIT of reveal`);
  const [firstRow] = [...rows.values()];
  const holdBack = firstRow[0];
  const perRow = firstRow.length;
  return { p, v, coarse, rows, holdBack, perRow };
}

test("ONE SWAP PER ROW: an incomplete row draws NONE of its own tiles, then ALL of them", async () => {
  const { p, v, coarse, holdBack, perRow } = rowSetup();
  const clock = { t: 1000 };
  const h = harness(
    (a) => (a.levelF === coarse.levelF && a.levelT === coarse.levelT) || keyOf(a) !== keyOf(holdBack),
    { now: () => clock.t },
  );

  // One row is short by a tile. It is NOT on screen: the stand-in is, under all of it — and the
  // OTHER row, which is complete, is drawn from its own tiles. The row is the unit.
  let reports = (await settle(h, [p])).reports;
  assert.equal(reports[0].rowsHeld, 1, "exactly the incomplete row is held");
  assert.equal(reports[0].tiles, v.addrs.length - perRow,
    "the complete rows are on screen and the held row contributes none of its own tiles");
  assert.equal(reports[0].fallbacks, perRow, "the held row is the stand-in, whole");
  assert.equal(reports[0].pending, 0, "and never black");

  // The last tile lands — the request that was outstanding, answered, exactly as the row completing
  // happens in the browser. One frame later the row is its own tiles: one swap, 0 -> perRow.
  assert.equal(h.deliver((a) => keyOf(a) === keyOf(holdBack)), 1);
  reports = (await settle(h, [p])).reports;
  assert.equal(reports[0].rowsHeld, 0);
  assert.equal(reports[0].tiles, v.addrs.length, "the whole row swapped in together");
  assert.equal(reports[0].fallbacks, 0);
});

test("the hold is BOUNDED: past ~300 ms what arrived is revealed, stand-ins under the rest", async () => {
  const { p, v, coarse, holdBack, perRow } = rowSetup();
  const clock = { t: 1000 };
  const h = harness(
    (a) => (a.levelF === coarse.levelF && a.levelT === coarse.levelT) || keyOf(a) !== keyOf(holdBack),
    { now: () => clock.t },
  );
  let reports = (await settle(h, [p])).reports;
  assert.equal(reports[0].tiles, v.addrs.length - perRow, "held, while the bound has not expired");

  // A row that never completes must not hold measured data off the screen for ever: "we have it but
  // didn't render it" is the bug this bound exists to make impossible.
  clock.t += 301;
  reports = h.surface.render([p]);
  assert.equal(reports[0].rowsLate, 1, "revealed late, and the report says which");
  assert.equal(reports[0].tiles, v.addrs.length - 1, "everything that arrived is on screen");
  assert.equal(reports[0].fallbacks, 1, "and the one that did not is the stand-in, not black");
  assert.equal(reports[0].pending, 0);
});

test("revealHoldMs = 0 is the old per-tile reveal, so a host can still ask for it", async () => {
  const { p, v, coarse, holdBack } = rowSetup();
  const h = harness(
    (a) => (a.levelF === coarse.levelF && a.levelT === coarse.levelT) || keyOf(a) !== keyOf(holdBack),
    { now: () => 1000, revealHoldMs: 0 },
  );
  const reports = (await settle(h, [p])).reports;
  assert.equal(reports[0].tiles, v.addrs.length - 1);
  assert.equal(reports[0].rowsHeld, 0);
});

// ---------------------------------------------------------------------------------------------
// 9. The transport: the stand-in is its OWN request.

test("SPY: the coarse stand-in set rides in a request of its OWN, never cut up among a viewport's tiles", async () => {
  // A stand-in only removes the black if it arrives as ONE picture. A batch answers when its slowest
  // member does, so four stand-in addresses riding in a chunk of a viewport's own cold tiles would
  // land no sooner than the tiles they stand in for — and the whole coarse-first order would be a
  // no-op. Asserted on the REQUESTS the client builds (T-367's rule), not on what it renders.
  const urls: string[] = [];
  const fetchFn = (async (url: string) => {
    urls.push(url);
    // Nothing is answered: what is measured is how ONE viewport change is cut into requests.
    return await new Promise<never>(() => { /* never settles */ });
  }) as never;

  const p = paneOver(5, 4);
  const v = viewOf(p);
  const coarse = coarsestCovering(LAT, p.box, v.levelF, v.levelT)!;

  const g = stubGl(W, H);
  const surface = new Surface(
    g.canvas, LAT,
    (tex) => new TileCache(tex, batchedTileSource("t", fetchFn), { now: () => 0 }),
    { pinParents: false },
  );
  surface.render([p]);
  for (let i = 0; i < 8; i++) await flush();

  const spelt = urls.map((u) => new URLSearchParams(u.split("?")[1])!.get("addresses")!.split(","));
  assert.ok(spelt.length > 0, "the viewport change reached the wire");
  const coarseSpelling = `${coarse.levelF}.${coarse.levelT}.`;
  const own = `${v.levelF}.${v.levelT}.`;
  assert.equal(
    spelt.find((s) => s.some((x) => x.startsWith(coarseSpelling)) && s.some((x) => x.startsWith(own))),
    undefined,
    "a request mixed the stand-in lane with the viewport's own tiles, so the stand-in waits on them");
  assert.ok(spelt.some((s) => s.length > 0 && s.every((x) => x.startsWith(coarseSpelling))),
    "…and the stand-in lane went out as a request of its own");
  for (const s of spelt) {
    assert.ok(s.length <= TILES_BATCH_MAX_ADDRESSES, "no request exceeds the route's own cap");
  }
});
