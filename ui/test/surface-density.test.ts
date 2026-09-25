// T-810 (MAP-10, docs/23 §10.6 rule 6 last bullet): the coarse-zoom DENSITY layer — features per
// cell from `GET /api/tiles/events`, replacing numbered cluster bubbles. Re-scoped 2026-09-24 from
// pin clustering.
//
// The claims, each against the degenerate implementation that would otherwise pass:
//  1. **The tile address(es)** a pane asks about are exactly what the base tile pass would draw at
//     (`levelsFor`/`tilesFor`), bounded.
//  2. **The request** is `/api/tiles/events` with only the route's own allowed parameters — no
//     `planes`, no `client` (the route's ALLOWED list has neither).
//  3. **The wire answer parses**, and malformed answers are dropped rather than drawn.
//  4. **`isCoarseZoom` genuinely flips with zoom** — the review-caught bug (fix 1): gating on the
//     density tile's OWN cell (always ~1 px by the "constant pixel density" pyramid's own design,
//     `levelsFor`) never turns off at a normal working zoom. The gate must instead be a RATIO with
//     no px bound in it to collapse to — level-0 cells compressed per screen pixel — so it reads
//     TRUE over a wide viewport and FALSE at a normal working zoom, proven against the exact
//     removed per-tile check, asserted red here.
//  5. **`densityQuads` draws only when `isCoarseZoom` says so** — a wide pane draws its non-empty
//     cells; the SAME tiles handed to a narrow pane draw nothing, so "drilling in resolves to
//     boxes" is a property of the pane's own zoom, not of the tile shape it happens to be given.
//  6. **Stroke-only** (hatch pattern), never a numbered bubble.
//  7. **Registry**: an overlay layer, on by default, below `detections`.
//  8. **Thin client**: no fetch/client/device-route in the geometry module; the host fetches on
//     the poll, never in the frame, and reaches no device route; the fetch itself is gated by the
//     same `isCoarseZoom` test so a fine-zoomed pane asks for nothing.
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import {
  DENSITY_LIVE_REFRESH_MS, DENSITY_RETRY_BASE_MS, DensityFetches, DENSITY_MARK, sharedDensityReader, DENSITY_MAX_TILES_PER_PANE, densityAddrs, densityQuads, densityUrl, isCoarseZoom, parseDensityTile,
  type DensityTile,
} from "../src/surface/density";
import { DENSITY_POLL_MS, DensityPoll } from "../src/app/centre/density-poll";
import { GENERALIZE_BELOW_CSS_PX, isGeneralized } from "../src/surface/marks";
import { composeOverlays, defaultPaneLayers, isLayerVisible, layerDef, withLayer } from "../src/surface/layers";
import { extentOf, levelsFor, type Box, type Lattice, type TileAddr } from "../src/surface/lattice";
import { toClip, type PaneRect, type PaneView } from "../src/surface/surface";

const S = 1e9;
const T0 = 1_700_000_000 * S;
const LAT = { scheme: "view", cells: 256, f0Hz: 6250, t0Ns: 1e9, levelsF: 20, levelsT: 15 };
const WIDE = { f0Hz: 1e6, f1Hz: 6e9, t0Ns: T0 - 30 * 86_400 * S, t1Ns: T0 };
const RECT: PaneRect = { x: 0, y: 0, w: 1000, h: 500 };

// ---------------------------------------------------------------------------
// 1/2. Addresses and the request
// ---------------------------------------------------------------------------

test("MAP density: the addresses are exactly the base tile pass's own (levelsFor/tilesFor), bounded", () => {
  const addrs = densityAddrs(LAT, WIDE, 1000, 500, "any");
  assert.ok(addrs.length > 0);
  for (const a of addrs) assert.equal(a.scheme, "view");
  assert.ok(addrs.length <= DENSITY_MAX_TILES_PER_PANE);
  // A degenerate box asks nothing.
  assert.deepEqual(densityAddrs(LAT, { ...WIDE, f1Hz: WIDE.f0Hz }, 1000, 500), []);
});

test("MAP density: the request names only the route's own allowed parameters — no planes, no client", () => {
  const a: TileAddr = { device: "any", scheme: "view", levelF: 3, levelT: 2, fIndex: 7, tIndex: 11, cells: 256 };
  assert.equal(densityUrl(a), "/api/tiles/events?level_f=3&level_t=2&f_index=7&t_index=11");
  const dev: TileAddr = { device: "hackrf-0", scheme: "overview", levelF: 1, levelT: 0, fIndex: 0, tIndex: 0, cells: 64 };
  const url = densityUrl(dev);
  assert.match(url, /^\/api\/tiles\/events\?/);
  for (const banned of ["planes=", "client="]) assert.ok(!url.includes(banned), `${url} carries ${banned}`);
  for (const want of ["scheme=overview", "device=hackrf-0", "cells=64"]) assert.ok(url.includes(want));
});

// ---------------------------------------------------------------------------
// 3. Parsing
// ---------------------------------------------------------------------------

const ADDR: TileAddr = { device: "any", scheme: "view", levelF: 4, levelT: 3, fIndex: 0, tIndex: 0, cells: 256 };

test("MAP density: the wire answer parses to a DensityTile; malformed answers are dropped", () => {
  const body = {
    extent: { f_lo_hz: 1e8, f_hi_hz: 1.02e8, f_cell_hz: 1e7, t0_s: 1_700_000_000, t1_s: 1_700_000_020, t_cell_s: 10, nt: 2, nf: 2 },
    counts: [0, 3, "x", 1],
  };
  const t = parseDensityTile(ADDR, body);
  assert.ok(t);
  assert.equal(t!.nt, 2);
  assert.equal(t!.nf, 2);
  assert.deepEqual(t!.counts, [0, 3, 0, 1]); // a non-numeric cell is dropped to 0, never inflated
  assert.equal(t!.fLoHz, 1e8);
  assert.equal(t!.fCellHz, 1e7);
  assert.equal(t!.t0Ns, 1_700_000_000 * S);
  assert.equal(t!.tCellNs, 10 * S);
  for (const junk of [null, {}, { extent: {} }, { extent: { nt: 2, nf: 2 } }, { counts: [1] }, "nope"]) {
    assert.equal(parseDensityTile(ADDR, junk), null);
  }
});

// ---------------------------------------------------------------------------
// 4. `isCoarseZoom`: genuinely flips with zoom (the review-caught bug, fixed)
// ---------------------------------------------------------------------------

const PANE = { f0Hz: 99e6, f1Hz: 101e6, t0Ns: T0 - 20 * S, t1Ns: T0 };
/** A viewport over the addressable spectrum's own order of magnitude, at the SAME lattice as
 * `PANE` — so a difference between the two is genuinely about zoom, not about a different lattice. */
const WIDE_PANE = WIDE;
/** A plain 20 MHz / ~17 min viewport — not extreme either way, the kind an ordinary working
 * session actually uses, where a real detection should draw as a box, not vanish into a glow. */
const DIVERGE_PANE = { f0Hz: 100e6, f1Hz: 120e6, t0Ns: T0 - 1000 * S, t1Ns: T0 };

/** The REMOVED per-tile check (T-810, fix 1): `densityAddrs`' own tile — chosen by `levelsFor` to
 * match screen resolution, exactly as the base tile pass's tile is — measured in px and compared
 * against a px bound the same way a detection box's generalization is. Kept ONLY to prove
 * [[isCoarseZoom]] is not this predicate under another name. */
function removedPerTileGate(lat: Lattice, box: Box, wPx: number, hPx: number): boolean {
  const { levelF, levelT } = levelsFor(lat, box, wPx, hPx);
  const cellWpx = ((lat.f0Hz * 2 ** levelF) / (box.f1Hz - box.f0Hz)) * wPx;
  const cellHpx = ((lat.t0Ns * 2 ** levelT) / (box.t1Ns - box.t0Ns)) * hPx;
  return isGeneralized(cellWpx, cellHpx, GENERALIZE_BELOW_CSS_PX);
}

test("RED PROOF: the removed per-tile-cell gate stays TRUE at a normal working zoom where boxes belong — the exact bug review caught", () => {
  assert.equal(removedPerTileGate(LAT, WIDE_PANE, RECT.w, RECT.h), true, "wide viewport: both gates should agree this is coarse");
  assert.equal(removedPerTileGate(LAT, DIVERGE_PANE, RECT.w, RECT.h), true,
    "the removed gate never turns off here either: `densityAddrs`' own tile is re-picked to match "
    + "screen resolution again, landing back in the same 1-2 px band — density would stay drawn "
    + "alongside real boxes at a completely ordinary zoom, exactly the review's finding");
});

test("MAP density: isCoarseZoom is TRUE over a wide viewport and FALSE at the SAME normal working zoom the removed gate got wrong", () => {
  assert.equal(isCoarseZoom(LAT, WIDE_PANE, RECT.w, RECT.h), true);
  assert.equal(isCoarseZoom(LAT, DIVERGE_PANE, RECT.w, RECT.h), false,
    "unlike the removed gate, this one genuinely turns off at a normal working zoom");
  assert.equal(isCoarseZoom(LAT, PANE, RECT.w, RECT.h), false);
  // Either axis alone is enough (OR): widening only the frequency span, holding time fixed, can
  // push the pane from "boxes" to "coarse" on its own.
  const widerFreq = { ...DIVERGE_PANE, f1Hz: DIVERGE_PANE.f0Hz + 400e6 };
  assert.equal(isCoarseZoom(LAT, widerFreq, RECT.w, RECT.h), true);
  // Degenerate / zero-sized inputs never crash and never read as coarse.
  assert.equal(isCoarseZoom(LAT, { ...PANE, f1Hz: PANE.f0Hz }, RECT.w, RECT.h), false);
  assert.equal(isCoarseZoom(LAT, PANE, 0, 0), false);
});

// ---------------------------------------------------------------------------
// 5. `densityQuads` draws only when `isCoarseZoom` says so
// ---------------------------------------------------------------------------

function tile(): DensityTile {
  // Cells 100 kHz x 1 h — the tile the density fetch would actually be answered with over a wide
  // viewport; irrelevant to whether this draws, which is now purely `isCoarseZoom`'s call.
  return {
    addr: ADDR, fLoHz: 1e6, fCellHz: 1e5, t0Ns: WIDE_PANE.t0Ns, tCellNs: 3600 * S, nt: 1, nf: 1,
    counts: [5],
  };
}

test("MAP density: the SAME tiles draw over a wide pane and draw NOTHING over a narrow one — zoom decides, not tile shape", () => {
  const wide = densityQuads([tile()], WIDE_PANE, RECT, LAT, { dpr: 1 });
  assert.equal(wide.length, 1);
  const narrow = densityQuads([tile()], PANE, RECT, LAT, { dpr: 1 });
  assert.deepEqual(narrow, [], "past the coarse-zoom threshold this layer must draw nothing at all");
});

test("MAP density: a hatch-filled cell per non-empty count, its darkness a function of the count", () => {
  const q = densityQuads([tile()], WIDE_PANE, RECT, LAT, { dpr: 1 });
  assert.equal(q.length, 1);
  const c = q[0];
  assert.equal(c.kind, "density-cell");
  assert.equal(c.part, "fill");
  assert.ok(c.pattern, "a stroke, cut by a screen-door pattern — never a wash");
  assert.equal(c.pattern!.mode, "hatch");
  assert.ok(c.rgba[3] > 0 && c.rgba[3] <= 1);
  assert.deepEqual(c.rgba.slice(0, 3), DENSITY_MARK.slice(0, 3));
});

test("MAP density: a zero-count cell draws nothing, even over a coarse-zoomed pane", () => {
  const zero: DensityTile = { ...tile(), counts: [0] };
  assert.deepEqual(densityQuads([zero], WIDE_PANE, RECT, LAT), []);
});

test("MAP density: alpha rises with count, monotonically, and saturates at capCount — never a numbered bubble", () => {
  const alphaOf = (count: number) =>
    densityQuads([{ ...tile(), counts: [count] }], WIDE_PANE, RECT, LAT, { capCount: 8 })[0].rgba[3];
  const a1 = alphaOf(1), a4 = alphaOf(4), a8 = alphaOf(8), a20 = alphaOf(20);
  assert.ok(a1 < a4 && a4 < a8, "alpha must rise with the count it stands in for");
  assert.equal(a8, a20, "capped — an outlier count cannot blow the ink past the cap");
  // No label, no glyph, no digit anywhere in the quad — density is fill, never a bubble.
  const q = densityQuads([{ ...tile(), counts: [12] }], WIDE_PANE, RECT, LAT);
  assert.ok(!("label" in q[0]));
});

test("MAP density: off the pane draws nothing; only the visible part of a partly-on tile", () => {
  const away = { ...WIDE_PANE, f0Hz: 900e9, f1Hz: 901e9 };
  assert.deepEqual(densityQuads([tile()], away, RECT, LAT), []);
  const half = { ...WIDE_PANE, f0Hz: 1.00005e6, f1Hz: 5e9 };
  const q = densityQuads([tile()], half, RECT, LAT);
  for (const r of q) for (const c of r.clip) assert.ok(c >= -1 && c <= 1, "clipped to the pane");
});

// ---------------------------------------------------------------------------
// 6. Stroke-only, placed through the tiles' own mapping
// ---------------------------------------------------------------------------

test("MAP density: every cell is placed by the tiles' own toClip", () => {
  const q = densityQuads([tile()], WIDE_PANE, RECT, LAT)[0];
  const [x0, y0, x1, y1] = toClip(
    { f0Hz: tile().fLoHz, f1Hz: tile().fLoHz + tile().fCellHz, t0Ns: tile().t0Ns, t1Ns: tile().t0Ns + tile().tCellNs },
    WIDE_PANE,
  );
  assert.ok(Math.abs(q.clip[0] - Math.max(x0, -1)) < 1e-9);
  assert.ok(Math.abs(q.clip[1] - Math.max(y0, -1)) < 1e-9);
  assert.ok(Math.abs(q.clip[2] - Math.min(x1, 1)) < 1e-9);
  assert.ok(Math.abs(q.clip[3] - Math.min(y1, 1)) < 1e-9);
});

// ---------------------------------------------------------------------------
// 7. The registry
// ---------------------------------------------------------------------------

test("MAP density: an overlay layer, on by default, below detections, toggleable per pane", () => {
  const d = layerDef("density")!;
  assert.equal(d.plane, "overlay");
  assert.equal(d.visibleByDefault, true);
  assert.ok(d.z < layerDef("detections")!.z, "density sits under detections — a box always wins the pixel");
  const a = defaultPaneLayers("p1");
  const b = withLayer(defaultPaneLayers("p2"), "density", false);
  assert.equal(isLayerVisible(a, "density"), true);
  assert.equal(isLayerVisible(b, "density"), false);
  const fns = { density: () => densityQuads([tile()], WIDE_PANE, RECT, LAT) };
  const pv = { id: "p", box: WIDE_PANE, rect: RECT } as never;
  assert.ok(composeOverlays(a, fns, pv, T0).length > 0);
  assert.deepEqual(composeOverlays(b, fns, pv, T0), []);
});

// ---------------------------------------------------------------------------
// 8. Thin client
// ---------------------------------------------------------------------------

test("MAP density: no signal logic, no fetch and no device route in the geometry module", () => {
  const src = readFileSync("src/surface/density.ts", "utf8").replace(/^\s*\/\/.*$/gm, "");
  for (const word of ["Date.now", "performance.now", "fetch(", "client.", "DeviceAction", "/api/control", "/api/device"]) {
    assert.ok(!src.includes(word), `density.ts must not contain "${word}"`);
  }
});

test("MAP density: wired into the host's overlay table and its poll — reads only, never in the frame", () => {
  const src = readFileSync("src/app/centre/surface.ts", "utf8");
  const fns = /overlayFns: Partial<Record<LayerId, OverlayLayerFn>> = \{([^}]*)\}/.exec(src);
  assert.ok(fns, "the overlay renderer table");
  assert.match(fns[1], /\bdensity: densityQuadsFn\b/);
  assert.match(src, /startPoll\(async \(\) => \{ mirror\(\); refreshPriors\(\); refreshDensity\(\); \}, DENSITY_POLL_MS\)/);
  // Which address is read, when, and under what deadline is `./density-poll.ts`'s (T-927): the host
  // hands it the one client's GET — with the deadline's signal — and nothing else. The gate the
  // fetch is under, and that it asks by `DensityFetches.due`, are asserted on that module below.
  assert.match(src, /new DensityPoll\(\(url, signal\) => client\.get<unknown>\(url, \{ signal \}\)\)/);
  assert.ok(!/client\.(post|put|delete)[^;]*density/i.test(src), "density must only ever be read");
});

// 9. Refresh (review fixes 2 and 3): the counts are "not a tile channel" — they change with every
// append, and an event is counted at its START although it is usually written when its track
// closes — so while a pane follows, EVERY tile it shows (the edge's and ones the edge has left) is
// asked again as rows arrive; a failure is retried; a frozen pane keeps its own copy.
const LIVE_ADDR: TileAddr = { scheme: "view", device: "any", cells: 256, levelF: 10, levelT: 8, fIndex: 3, tIndex: 20 };
const LIVE_EXT = extentOf(LAT as Lattice, LIVE_ADDR);
const INSIDE = (LIVE_EXT.t0Ns + LIVE_EXT.t1Ns) / 2;
const PAST_END = LIVE_EXT.t1Ns + 1e9;
const counted = (n: number): DensityTile => ({
  addr: LIVE_ADDR, fLoHz: LIVE_EXT.f0Hz, fCellHz: 1, t0Ns: LIVE_EXT.t0Ns, tCellNs: 1, nt: 1, nf: 1, counts: [n],
});
const countsOf = (f: DensityFetches, pane: string) => f.tiles(pane, [LIVE_ADDR]).map((t) => t.counts[0]);

test("MAP density refresh: a live-edge tile's counts update after new events, address unchanged", () => {
  const f = new DensityFetches();
  assert.deepEqual(f.due("p", [LIVE_ADDR], INSIDE, true, 0), [LIVE_ADDR], "first sight is asked for");
  f.succeeded("p", LIVE_ADDR, counted(1), INSIDE, 0);
  // The edge moves on inside the SAME tile; within the cadence nothing is asked (no storm)...
  assert.deepEqual(f.due("p", [LIVE_ADDR], INSIDE + 1e9, true, DENSITY_LIVE_REFRESH_MS - 1), []);
  // ...no rows arrived (edge unmoved): nothing to ask about either...
  assert.deepEqual(f.due("p", [LIVE_ADDR], INSIDE, true, 10 * DENSITY_LIVE_REFRESH_MS), []);
  // ...and once rows arrived and the cadence elapsed, the same address is asked again and shown.
  assert.deepEqual(f.due("p", [LIVE_ADDR], INSIDE + 1e9, true, DENSITY_LIVE_REFRESH_MS), [LIVE_ADDR]);
  f.succeeded("p", LIVE_ADDR, counted(7), INSIDE + 1e9, DENSITY_LIVE_REFRESH_MS);
  assert.deepEqual(countsOf(f, "p"), [7]);
  // A FROZEN pane over the same tile is a view over the past: not revalidated.
  assert.deepEqual(f.due("p", [LIVE_ADDR], INSIDE + 9e9, false, 10 * DENSITY_LIVE_REFRESH_MS), []);
});

test("MAP density refresh: a failed fetch is retried with backoff, then shown", () => {
  const f = new DensityFetches();
  f.failed("p", LIVE_ADDR, 0);
  assert.deepEqual(f.tiles("p", [LIVE_ADDR]), [], "nothing to draw yet");
  // Retried even for a frozen pane (a failure is not a copy), after the backoff — not before.
  assert.deepEqual(f.due("p", [LIVE_ADDR], INSIDE, false, DENSITY_RETRY_BASE_MS - 1), []);
  assert.deepEqual(f.due("p", [LIVE_ADDR], INSIDE, false, DENSITY_RETRY_BASE_MS), [LIVE_ADDR]);
  f.failed("p", LIVE_ADDR, DENSITY_RETRY_BASE_MS);
  // Second failure doubles the wait.
  assert.deepEqual(f.due("p", [LIVE_ADDR], INSIDE, false, 3 * DENSITY_RETRY_BASE_MS - 1), []);
  assert.deepEqual(f.due("p", [LIVE_ADDR], INSIDE, false, 3 * DENSITY_RETRY_BASE_MS), [LIVE_ADDR]);
  f.succeeded("p", LIVE_ADDR, counted(4), INSIDE, 3 * DENSITY_RETRY_BASE_MS);
  assert.deepEqual(countsOf(f, "p"), [4]);
});

test("MAP density refresh: a past tile on screen in a FOLLOWING pane is re-asked as the edge advances — never sealed", () => {
  const f = new DensityFetches();
  // Asked after the edge left the tile — the case a seal would freeze forever.
  f.succeeded("p", LIVE_ADDR, counted(2), PAST_END, 0);
  for (const s of [60, 120, 600, 3600]) {
    assert.deepEqual(f.due("p", [LIVE_ADDR], PAST_END + s * 1e9, true, s * 1000), [LIVE_ADDR], `+${s} s`);
  }
  // Frozen, the same past tile is asked once and kept.
  assert.deepEqual(f.due("p", [LIVE_ADDR], PAST_END + 3600e9, false, 1e7), []);
});

test("MAP density refresh: a back-dated event lands in a tile after the edge passed its end, and is shown", () => {
  const f = new DensityFetches();
  f.succeeded("p", LIVE_ADDR, counted(1), INSIDE, 0);
  f.succeeded("p", LIVE_ADDR, counted(1), PAST_END, DENSITY_LIVE_REFRESH_MS); // the completing ask
  // A track closes 60 s later with a start inside this tile: the ledger now counts 2 there.
  const later = PAST_END + 60e9, now = DENSITY_LIVE_REFRESH_MS + 60_000;
  assert.deepEqual(f.due("p", [LIVE_ADDR], later, true, now), [LIVE_ADDR], "the next refresh must ask again");
  f.succeeded("p", LIVE_ADDR, counted(2), later, now);
  assert.deepEqual(countsOf(f, "p"), [2]);
});

test("MAP density refresh: copies are per pane — a frozen pane never picks up a following pane's refresh", () => {
  const f = new DensityFetches();
  f.succeeded("frozen", LIVE_ADDR, counted(1), INSIDE, 0);
  f.succeeded("live", LIVE_ADDR, counted(1), INSIDE, 0);
  f.succeeded("live", LIVE_ADDR, counted(9), INSIDE + 1e9, DENSITY_LIVE_REFRESH_MS);
  assert.deepEqual(countsOf(f, "live"), [9]);
  assert.deepEqual(countsOf(f, "frozen"), [1]);
  f.retain([["live", LIVE_ADDR]]);
  assert.deepEqual(countsOf(f, "frozen"), [], "a pane no longer showing an address drops its copy");
  assert.deepEqual(countsOf(f, "live"), [9]);
});

test("MAP density refresh: two panes due for one address in one tick send ONE request, stamped at the edge it was ISSUED at", async () => {
  const urls: string[] = [];
  let release!: (v: unknown) => void;
  const gate = new Promise((r) => { release = r; });
  const read = sharedDensityReader((url) => { urls.push(url); return gate; });
  // The second ask JOINS the request in flight — and asks with a LATER edge (its own tick).
  const a = read(LIVE_ADDR, INSIDE), b = read(LIVE_ADDR, INSIDE + 9e9);
  assert.equal(urls.length, 1, "deduped across panes");
  release({ extent: { nt: 1, nf: 1, f_lo_hz: 0, f_cell_hz: 1, t0_s: 0, t_cell_s: 1 }, counts: [3] });
  const [ansA, ansB] = await Promise.all([a, b]);
  assert.deepEqual([ansA.tile?.counts, ansB.tile?.counts], [[3], [3]]);
  // T-927 follow-up 2: the joiner is told the edge the request was ISSUED at, not its own. Recording
  // its own later tick claims a snapshot fresher than the picture in it, and `due`'s "rows arrived
  // since" test then skips the refresh that would catch what arrived in between.
  assert.deepEqual([ansA.edgeAtFetchNs, ansB.edgeAtFetchNs], [INSIDE, INSIDE],
    "both copies are stamped at the issuing ask's edge");
  // Once answered, the next ask goes out again: it shares what is IN FLIGHT, it is not a cache.
  void read(LIVE_ADDR, INSIDE);
  assert.equal(urls.length, 2);
  // A failure resolves a null tile, for the caller to retry.
  const bad = sharedDensityReader(() => Promise.reject(new Error("503")));
  assert.equal((await bad(LIVE_ADDR, INSIDE)).tile, null);
});

test("MAP density refresh: the poll asks by DensityFetches.due per pane, through the shared reader", () => {
  const src = readFileSync("src/app/centre/density-poll.ts", "utf8");
  assert.match(src, /this\.fetches\.due\(pane\.id, addrs, edgeNs, following\(pane\.id\), nowMs\)/);
  assert.match(src, /sharedDensityReader\(\(url\) => withDeadline\(/);
  assert.doesNotMatch(src, /densityKeyByPane/, "a fetched-once address key froze the live edge");
});

// ---------------------------------------------------------------------------
// 10. The HOST side (T-927): the read's deadline, the joined request's edge stamp, the stale addr
//     set, the NaN edge, and one spelling of the poll period. Each claim is stated against the
//     behaviour T-810 shipped, so the test is red on the old rule rather than merely green on the new.
// ---------------------------------------------------------------------------

const HOST_PANE = (id: string, box: Box = WIDE): PaneView => ({ id, box, rect: RECT, device: "any" });
const ON = () => true;
const FOLLOWING = () => true;
const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));
/** The wire body `/api/tiles/events` answers, one cell with `n` events, for whatever tile was asked. */
const wireCount = (n: number) => ({ extent: { nt: 1, nf: 1, f_lo_hz: 0, f_cell_hz: 1, t0_s: 0, t_cell_s: 1 }, counts: [n] });

test("MAP density host: a read that never settles is abandoned at its deadline, and asked again", async () => {
  const asked: string[] = [];
  let aborts = 0;
  // A reader that hangs AND ignores its signal — the worst case: the deadline must still release it.
  const poll = new DensityPoll((url, signal) => {
    asked.push(url);
    signal.addEventListener("abort", () => { aborts++; });
    return new Promise<unknown>(() => {});
  }, 5);
  poll.tick(LAT as Lattice, [HOST_PANE("p1")], T0, ON, FOLLOWING);
  const n = asked.length;
  assert.ok(n > 0, "a coarse-zoomed pane asks");
  assert.deepEqual(poll.inflightPanes, ["p1"], "a batch is in flight while the reads hang");
  await sleep(60);
  assert.deepEqual(poll.inflightPanes, [],
    "the deadline released the pane's batch — with no timeout on the GET it stayed set until the browser gave up, and the shared reader stalled every other pane asking for the same address");
  assert.ok(aborts > 0, "and the request itself was aborted, not merely forgotten");
  // Released means re-askable: the next poll retries on `DensityFetches`' own backoff.
  poll.tick(LAT as Lattice, [HOST_PANE("p1")], T0, ON, FOLLOWING);
  assert.equal(asked.length, 2 * n, "the hung address is asked again; with the stuck in-flight marker it never was");
});

test("MAP density host: a pane JOINING a request in flight keeps the copy at the edge it was issued at", async () => {
  let count = 3;
  let release!: () => void;
  const gate = new Promise<void>((r) => { release = () => r(); });
  const poll = new DensityPoll(() => gate.then(() => wireCount(count)));
  const e1 = T0 - 60 * S;
  const e2 = T0; // the edge has moved on by the time the second pane asks
  poll.tick(LAT as Lattice, [HOST_PANE("p1")], e1, ON, FOLLOWING);
  poll.tick(LAT as Lattice, [HOST_PANE("p1"), HOST_PANE("p2")], e2, ON, FOLLOWING);
  release();
  await sleep(10);
  const firstCounts = poll.tilesFor("p2").map((t) => t.counts[0]);
  assert.ok(firstCounts.length > 0 && firstCounts.every((c) => c === 3), "p2 drew the shared answer");
  count = 9; // new events land in those tiles
  // Stamped at `e1` (when the request went out), p2's copy is stale as of `e2` and is refreshed once
  // the cadence elapses. Stamped at p2's own later tick (`e2`), `e2 < e2` is false: p2 would keep the
  // pre-`e1` picture for as long as the edge stood still, exactly the snapshot-too-fresh defect.
  for (let i = 0; i < 2 + DENSITY_LIVE_REFRESH_MS / DENSITY_POLL_MS; i++) {
    poll.tick(LAT as Lattice, [HOST_PANE("p1"), HOST_PANE("p2")], e2, ON, FOLLOWING);
  }
  await sleep(10);
  assert.ok(poll.tilesFor("p2").every((t) => t.counts[0] === 9),
    "p2's copy was refreshed — with the joining tick's edge recorded instead, it never was");
});

test("MAP density host: a late completion is written against the CURRENT addresses, not the ones it was sent with", async () => {
  let release!: () => void;
  const gate = new Promise<void>((r) => { release = () => r(); });
  const poll = new DensityPoll(() => gate.then(() => wireCount(5)));
  poll.tick(LAT as Lattice, [HOST_PANE("p1")], T0, ON, FOLLOWING);
  // The layer goes off (or the pane pans away) while the batch is in flight.
  poll.tick(LAT as Lattice, [HOST_PANE("p1")], T0, () => false, FOLLOWING);
  assert.deepEqual(poll.tilesFor("p1"), [], "nothing drawn with the layer off");
  release();
  await sleep(10);
  assert.deepEqual(poll.tilesFor("p1"), [],
    "the completion must not restore the old set — the reviewed code wrote the addrs it was SENT with, so they came back for a second");
  // With the layer back on, the answer that did arrive is drawn (it was kept, not thrown away).
  poll.tick(LAT as Lattice, [HOST_PANE("p1")], T0, ON, FOLLOWING);
  assert.ok(poll.tilesFor("p1").length > 0);
});

test("MAP density refresh: a NaN live edge is never stored as the fetch edge — it would seal the address for ever", () => {
  const f = new DensityFetches();
  f.succeeded("p", LIVE_ADDR, counted(1), Number.NaN, 0);
  assert.deepEqual(countsOf(f, "p"), [1], "the copy in hand is kept");
  assert.deepEqual(f.due("p", [LIVE_ADDR], INSIDE, true, DENSITY_LIVE_REFRESH_MS), [LIVE_ADDR],
    "asked again once an edge is known: `NaN < edge` is false, so storing NaN was an accidental seal — the very thing review fix 3 removed");
});

test("MAP density host: the poll period is one constant, and the poll module reads only", () => {
  const host = readFileSync("src/app/centre/surface.ts", "utf8");
  const mod = readFileSync("src/app/centre/density-poll.ts", "utf8");
  // T-927 follow-up 5: one spelling of the period — defined in the poll module, used by startPoll.
  assert.match(mod, /export const DENSITY_POLL_MS = 1000;/);
  assert.doesNotMatch(host, /DENSITY_POLL_MS\s*=/, "the host imports the constant, it does not redefine it");
  assert.match(host, /startPoll\(async \(\) => \{ mirror\(\); refreshPriors\(\); refreshDensity\(\); \}, DENSITY_POLL_MS\)/);
  // The fetch is gated by the same isCoarseZoom test the draw path uses — a fine-zoomed pane must not
  // even ask for tiles it would draw nothing with.
  assert.match(mod, /isCoarseZoom\(lat, pane\.box, pane\.rect\.w, pane\.rect\.h\)/);
  // Read only, no device route, and no browser clock (pacing is in poll ticks).
  assert.ok(!/\.(post|put|del)\(/.test(mod), "density is only ever read");
  for (const word of ["Date.now", "performance.now", "DeviceAction", "/api/control", "/api/device"]) {
    assert.ok(!mod.replace(/^\s*\/\/.*$/gm, "").includes(word), `density-poll.ts must not contain "${word}"`);
  }
});
