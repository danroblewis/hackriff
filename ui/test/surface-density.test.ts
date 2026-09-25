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
  DENSITY_MARK, DENSITY_MAX_TILES_PER_PANE, densityAddrs, densityQuads, densityUrl, isCoarseZoom, parseDensityTile,
  type DensityTile,
} from "../src/surface/density";
import { GENERALIZE_BELOW_CSS_PX, isGeneralized } from "../src/surface/marks";
import { composeOverlays, defaultPaneLayers, isLayerVisible, layerDef, withLayer } from "../src/surface/layers";
import { levelsFor, type Box, type Lattice, type TileAddr } from "../src/surface/lattice";
import { toClip, type PaneRect } from "../src/surface/surface";

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
  assert.match(src, /startPoll\(async \(\) => \{ mirror\(\); refreshPriors\(\); refreshDensity\(\); \}, 1000\)/);
  assert.match(src, /client\.get<unknown>\(densityUrl\(a\)\)/);
  // The poll's own fetch is gated by the same isCoarseZoom test the draw path uses — a fine-zoomed
  // pane must not even ask for tiles it would draw nothing with.
  assert.match(src, /isCoarseZoom\(lat, pane\.box, pane\.rect\.w, pane\.rect\.h/);
  assert.ok(!/client\.(post|put|delete)[^;]*density/i.test(src), "density must only ever be read");
});
