// T-893: the three gaps T-890 left between a following pane and the live-rendering invariant
// ("rows append in real time as they are recorded; live is NEVER gated on tile generation").
//
//  1. NEW ROWS ARRIVED BY POLLING. The refresh lane re-asks a live tile at most once per a share of
//     what it costs, so on a busy route the top of a short following pane was drawn seconds behind
//     the rows that existed — with no row crossing at all. `/ws/tiles/rows` pushes each row as it is
//     recorded; nothing used it. Now every column a following pane draws holds a subscription and
//     the pushed rows are written into the tile in hand (or make one), past its horizon.
//  2. COARSE STAND-INS WERE NEVER REFRESHED, and the stand-in search looked only UP. So a time
//     zoom-out on a following pane went blank until the coarser level arrived, although the rows it
//     had just been drawing were resident one level down.
//  3. WIDE PANES: T-890's next-row look-ahead extended only columns already resident, so the columns
//     of a six-column pane still in flight when the row neared its end got no next row at all.
//
// Everything here is driven the way `Surface.render` + `SurfacePreview` drive the cache each frame,
// on an injected clock, against a route whose answers are SLOW (the busy box, made deterministic).

import { test } from "node:test";
import assert from "node:assert/strict";
import { CELL } from "../src/surface/cellrule";
import { extentOf, keyOf, tCellNs, tTileNs, tilesFor, type Box, type Lattice, type TileAddr } from "../src/surface/lattice";
import { TileCache, type Viewport } from "../src/surface/tilecache";
import { LiveRowFeeds, type ColumnAddr, type RowBlock, type RowOpener } from "../src/surface/rowfeed";
import { Surface, type PaneView } from "../src/surface/surface";
import type { TileData } from "../src/surface/tile";
import { stubGl } from "./surface-glstub";

/** The production time floor (T-501): 40 ms cells, so a level-0 tile is 10.24 s. */
const LAT: Lattice = { scheme: "view", cells: 256, f0Hz: 6250, t0Ns: 40e6, levelsF: 20, levelsT: 15 };
const CELL_NS = tCellNs(LAT, 0);
const TILE_NS = tTileNs(LAT, 0);
const TILE_HZ = 6250 * 256;
const ROW = 170_000_000;
const FRAME_MS = 100;
const flush = () => new Promise((r) => setImmediate(r));

/** A full-size tile answered at `asOfNs`, every cell observed. */
function tile(a: TileAddr, asOfNs: number | null): TileData {
  const n = a.cells * a.cells;
  return {
    addr: a, key: keyOf(a), nf: a.cells, nt: a.cells, t1Ns: extentOf(LAT, a).t1Ns, asOfNs,
    value: new Float32Array(n).fill(-90), state: new Uint8Array(n).fill(CELL.OBSERVED),
    tier: "spectrum-history", answeredLevel: a.levelT, fold: { frequency: "exact", time: "exact" },
    measured: { nf: a.cells, nt: a.cells }, rangeDb: { lo: -100, hi: -60 }, bytes: n * 3,
    serverInFlightLimit: null, serverInFlightShare: null,
  };
}

/** A pushed block of `rows` rows from `row0`, every cell observed at -70 dB. */
function block(row0: number, rows: number, cells = LAT.cells): RowBlock {
  return {
    kind: "rows", row0, rows, nf: cells, final: false,
    maxDb: new Float32Array(rows * cells).fill(-70), coverage: new Array<string>(rows * cells).fill("observed"),
  };
}

/** Where a frame's pane is drawn up to: the renderer's own rule (`Surface.drawUpToHorizon`). */
function drawnTop(cache: TileCache<unknown>, box: Box): number {
  let top = Number.NEGATIVE_INFINITY;
  for (const a of tilesFor(LAT, box, 0, 0)) {
    const e = cache.peek(a);
    if (!e) continue;
    const ext = extentOf(LAT, a);
    const reach = Math.min(e.data.asOfNs ?? ext.t1Ns, ext.t1Ns, box.t1Ns);
    if (reach > Math.max(ext.t0Ns, box.t0Ns)) top = Math.max(top, reach);
  }
  return top;
}

/**
 * Follow the live edge on a BUSY route: every answer, a lone revalidation included, takes
 * `routeMs` and states the horizon the route had when it was asked. With `feed`, the columns the
 * cache names ([[TileCache.liveColumns]]) are subscribed through [[LiveRowFeeds]] over a fake
 * transport that pushes every row the moment it is complete — which is what `/ws/tiles/rows` does.
 */
async function followBusy(ms: number, { feed, routeMs = 3000, spanNs = 2.8e9 }: { feed: boolean; routeMs?: number; spanNs?: number }) {
  const clock = { t: 0 };
  const edgeAt = (t: number) => ROW * TILE_NS + 0.5e9 + t * 1e6;
  const waiting: { addr: TileAddr; due: number; asOf: number; resolve: (d: TileData) => void }[] = [];
  const cache = new TileCache<{ id: number }>(
    { upload: () => ({ id: 0 }), destroy: () => {} },
    (addr) => new Promise<TileData>((resolve) => {
      waiting.push({ addr, due: clock.t + routeMs, asOf: edgeAt(clock.t), resolve });
    }),
    { inFlight: 4, now: () => clock.t, serverMsGuess: routeMs },
  );
  // The transport: one fake socket per path, each remembering the next row it will push.
  const sockets = new Map<string, { col: ColumnAddr; next: number; push: (t: string) => void; open: boolean }>();
  const opener: RowOpener = (path, onText) => {
    const q = new URLSearchParams(path.split("?")[1]);
    const col: ColumnAddr = {
      device: "any", scheme: "view", levelF: Number(q.get("level_f")), levelT: Number(q.get("level_t")),
      fIndex: Number(q.get("f_index")), cells: Number(q.get("cells") ?? 256),
    };
    const s = { col, next: Number(q.get("t_from")), push: onText, open: true };
    sockets.set(path, s);
    onText(JSON.stringify({ type: "subscribed", range: { t_from: s.next, t_to: null }, extent: { t_cell_s: CELL_NS / 1e9 } }));
    return { close: () => { s.open = false; } };
  };
  const feeds = feed ? new LiveRowFeeds(opener, (col, b) => cache.applyRows(col, b), { now: () => clock.t }) : null;
  const frames: { t: number; shortNs: number }[] = [];
  for (let step = 0; step * FRAME_MS < ms; step++) {
    clock.t = step * FRAME_MS;
    const edgeNs = edgeAt(clock.t);
    const box: Box = { f0Hz: 0.25 * TILE_HZ, f1Hz: 0.75 * TILE_HZ, t0Ns: edgeNs - spanNs, t1Ns: edgeNs };
    const view: Viewport = { box, levelF: 0, levelT: 0 };
    cache.beginFrame();
    for (const a of tilesFor(LAT, box, 0, 0)) cache.acquire(a);
    cache.setViewports(LAT, [view]);
    cache.endFrame();
    cache.refreshEdge(LAT, edgeNs, [view]);
    feeds?.want(cache.liveColumns(LAT, edgeNs, [view]));
    // The route pushes every row whose end the data edge has passed, in blocks inside one tile.
    const complete = Math.floor(edgeNs / CELL_NS);
    for (const s of sockets.values()) {
      while (s.open && s.next < complete) {
        const n = Math.min(complete - s.next, 64, 256 - (s.next % 256));
        const maxDb = new Array<number>(n * 256).fill(-70);
        s.push(JSON.stringify({
          type: "rows", row0: s.next, rows: n, nf: 256, max_db: maxDb, final: false,
          coverage: { states: ["unobserved", "observed"], nt: n, nf: 256, aligned: true, plane: { runs: [1, n * 256] } },
        }));
        s.next += n;
      }
    }
    const top = drawnTop(cache, box);
    frames.push({ t: clock.t, shortNs: Number.isFinite(top) ? edgeNs - top : spanNs });
    await flush();
    for (let i = waiting.length - 1; i >= 0; i--) {
      const w = waiting[i];
      if (w.due > clock.t) continue;
      waiting.splice(i, 1);
      w.resolve(tile(w.addr, w.asOf));
      await flush();
    }
  }
  return { frames, cache, sockets, feeds };
}

test("T-893 gap 1: on a busy route a following pane's TOP is drawn to within a row or two of the edge, by pushed rows", async (t) => {
  const RUN = 20_000;
  // The control reproduces the defect: polling alone leaves the top of a 2.8 s pane seconds short
  // of the edge on a route that takes 3 s a tile — the gate's "drawn to 2.3 s short of the top".
  const polled = await followBusy(RUN, { feed: false });
  const settle = (fs: typeof polled.frames) => fs.filter((f) => f.t >= 5000);
  const worstPolled = Math.max(...settle(polled.frames).map((f) => f.shortNs));
  assert.ok(worstPolled > 1e9,
    `the control drew its top within ${(worstPolled / 1e9).toFixed(2)} s of the edge by polling alone, so this ` +
    "harness does not reproduce the busy route and proves nothing");

  const pushed = await followBusy(RUN, { feed: true });
  const worst = settle(pushed.frames).reduce((m, f) => (f.shortNs > m.shortNs ? f : m));
  // One row still being recorded plus one frame: the newest COMPLETE row is on screen.
  const bound = 2 * CELL_NS + FRAME_MS * 1e6;
  t.diagnostic(`worst top shortfall after 5 s: polling alone ${(worstPolled / 1e9).toFixed(2)} s, with pushed rows ` +
    `${(worst.shortNs / 1e9).toFixed(3)} s (bound ${(bound / 1e9).toFixed(2)} s); ${pushed.cache.stats.rowsPushed} rows pushed, ` +
    `${pushed.cache.stats.rowTilesSynthesized} tile(s) built from them`);
  assert.ok(worst.shortNs <= bound,
    `a following pane's top was drawn ${(worst.shortNs / 1e9).toFixed(2)} s short of the live edge at t=${worst.t} ms ` +
    `(polling alone: ${(worstPolled / 1e9).toFixed(2)} s): rows reach the pane only when the polling lane comes ` +
    "round, not as they are recorded (T-893)");
  assert.ok(pushed.cache.stats.rowsPushed > 0, "no pushed row was ever filed");
  // The feed crossed a row boundary on its own: the next row's tile was built from its pushed rows
  // before any answer for it could land (the route takes 3 s; the crossing is ~9.7 s in).
  assert.ok(pushed.cache.stats.rowTilesSynthesized > 0,
    "the row the edge entered was never drawn from pushed rows — it waited for a tile read");
});

test("T-893 gap 1: the subscription is an address range at the pane's own column and level, starting where the rows in hand stop", async () => {
  const { sockets } = await followBusy(6000, { feed: true });
  const paths = [...sockets.keys()];
  assert.ok(paths.length >= 1, "a following pane opened no row subscription");
  for (const p of paths) {
    const q = new URLSearchParams(p.split("?")[1]);
    assert.ok(p.startsWith("/ws/tiles/rows?"), p);
    assert.equal(q.get("level_f"), "0");
    assert.equal(q.get("level_t"), "0");
    assert.equal(q.get("t_to"), null, "a following subscription leaves its range open");
    const from = Number(q.get("t_from"));
    // The edge tile was not in hand when it opened, so it starts at that tile's FIRST row: the whole
    // current row arrives pushed, and the pane draws it without waiting for the tile read.
    assert.equal(from % 256, 0, `t_from ${from} is not the first row of the edge tile`);
    assert.equal(Math.floor(from / 256), ROW, `t_from ${from} is not in the tile the edge was in`);
  }
});

test("T-893 gap 1: a pushed row is laid back over a tile answer that is older at its top", async () => {
  const clock = { t: 0 };
  const a: TileAddr = { device: "any", scheme: "view", levelF: 0, levelT: 0, fIndex: 5, tIndex: ROW, cells: 256 };
  const col: ColumnAddr = { device: "any", scheme: "view", levelF: 0, levelT: 0, fIndex: 5, cells: 256 };
  const t0 = extentOf(LAT, a).t0Ns;
  let answer: ((d: TileData) => void) | null = null;
  const cache = new TileCache<{ id: number }>({ upload: () => ({ id: 0 }), destroy: () => {} },
    () => new Promise<TileData>((r) => { answer = r; }), { inFlight: 4, now: () => clock.t });
  cache.setViewports(LAT, [{ box: extentOf(LAT, a), levelF: 0, levelT: 0 }]);
  cache.acquire(a);
  cache.endFrame();
  // Rows 0..99 are pushed while the read is in flight; the tile is built from them at once.
  cache.applyRows(col, block(ROW * 256, 64));
  cache.applyRows(col, block(ROW * 256 + 64, 36));
  assert.equal(cache.peek(a)?.data.asOfNs, t0 + 100 * CELL_NS, "the pushed rows did not put the tile on screen");
  // The answer lands, stating a horizon only 40 rows in (the route's trails the feed).
  answer!(tile(a, t0 + 40 * CELL_NS));
  await flush();
  const e = cache.peek(a)!;
  assert.equal(e.synthetic, undefined, "the real answer did not replace the copy built from pushed rows");
  assert.equal(e.data.asOfNs, t0 + 100 * CELL_NS,
    "the answer took the newest 60 pushed rows back off the screen: a tile read replaced rows it did not hold");
  assert.equal(e.data.value[99 * 256], -70, "row 99 is not the pushed row's level");
  assert.equal(e.data.value[10 * 256], -90, "a row inside the answer's horizon was overwritten by a pushed one");
});

test("T-893 gap 1: a frozen pane holds no feeds, and nothing but /ws/tiles/rows is ever opened", () => {
  const opened: string[] = [];
  const closed: string[] = [];
  const opener: RowOpener = (path) => { opened.push(path); return { close: () => closed.push(path) }; };
  const feeds = new LiveRowFeeds(opener, () => {}, { now: () => 0 });
  const col = (f: number): ColumnAddr => ({ device: "any", scheme: "view", levelF: 0, levelT: 0, fIndex: f, cells: 256 });
  feeds.want([{ col: col(1), fromRow: 10 }, { col: col(2), fromRow: 10 }]);
  assert.equal(feeds.size, 2);
  // The pane is paused: the host's following list is empty, and so is what it wants.
  feeds.want([]);
  assert.equal(feeds.size, 0, "a frozen pane kept a row subscription open");
  assert.deepEqual([...closed].sort(), [...opened].sort(), "not every feed was closed");
  for (const p of opened) assert.ok(p.startsWith("/ws/tiles/rows?"), `opened ${p}`);
  // The client's cap: past it, the rest keep polling rather than being refused by the route.
  feeds.want(Array.from({ length: 20 }, (_, i) => ({ col: col(i), fromRow: 0 })));
  assert.equal(feeds.size, 12);
});

test("T-893 gap 1: a refused or cut column is retried later, not at frame rate", () => {
  let now = 0;
  const opened: string[] = [];
  const handlers: ((t: string) => void)[] = [];
  const feeds = new LiveRowFeeds((p, onText) => { opened.push(p); handlers.push(onText); return { close: () => {} }; },
    () => {}, { now: () => now, retryMs: 5000 });
  const c: ColumnAddr = { device: "any", scheme: "view", levelF: 0, levelT: 0, fIndex: 1, cells: 256 };
  feeds.want([{ col: c, fromRow: 5 }]);
  handlers[0](JSON.stringify({ type: "refused", status: 503, reason: "at the subscription cap" }));
  for (now = 0; now < 4900; now += 100) feeds.want([{ col: c, fromRow: 5 }]);
  assert.equal(opened.length, 1, `a refused column was re-asked ${opened.length - 1} times inside the retry wait`);
  now = 5000;
  feeds.want([{ col: c, fromRow: 5 }]);
  assert.equal(opened.length, 2, "a refused column was never asked again");
});

// ——— gap 2: stand-ins ———

const W = 800, H = 600;

test("T-893 gap 2: a time zoom-out on a following pane draws the FINER rows in hand while the coarser level is pending", async () => {
  const LAT2: Lattice = { scheme: "view", cells: 256, f0Hz: 6250, t0Ns: 1e9, levelsF: 20, levelsT: 15 };
  const g = stubGl(W, H);
  // Level-0 time tiles answer at once; anything coarser in time never does (a busy route).
  const surface = new Surface(g.canvas, LAT2,
    (tex) => new TileCache(tex, (a) => (a.levelT === 0
      ? Promise.resolve({ ...tile(a, null), t1Ns: extentOf(LAT2, a).t1Ns })
      : new Promise<TileData>(() => {})), { inFlight: 16, now: () => 0 }),
    { pinParents: false });
  surface.setScale(-100, -60);
  const T1 = 1000 * 256e9;
  const pane = (spanNs: number): PaneView => ({
    id: "a", rect: { x: 0, y: 0, w: W, h: H },
    box: { f0Hz: 100e6, f1Hz: 100e6 + W * 6250, t0Ns: T1 - spanNs, t1Ns: T1 },
  });
  surface.render([pane(600e9)]);
  await flush();
  const [before] = surface.render([pane(600e9)]);
  assert.equal(before.levelT, 0, "premise: the pane opens at level 0 in time");
  assert.ok(before.tiles > 0 && before.pending === 0, `premise: the pane is resident (${JSON.stringify(before)})`);
  // Zoom OUT in time, the top (the live edge) held: the pane now asks for level 1.
  g.reset();
  const [after] = surface.render([pane(1200e9)]);
  assert.equal(after.levelT, 1, "premise: the zoom-out moved the pane to level 1 in time");
  const fallbackDraws = g.draws().filter((d) => d.u?.uKind?.[0] === 0 && d.u?.uFallback?.[0] === 1);
  assert.ok(after.fallbacks > 0 && fallbackDraws.length > 0,
    `after a time zoom-out the pane drew ${after.tiles} tiles and ${after.fallbacks} stand-ins with every level-0 row ` +
    `it had just drawn still resident: the stand-in search ignores finer tiles in hand (${JSON.stringify(after)})`);
  assert.ok(after.shortNs < 1e9, `the zoomed-out pane is drawn ${(after.shortNs / 1e9).toFixed(1)} s short of its top`);
});

test("T-893 gap 2: a coarse stand-in drawn at a following pane's live edge is REFRESHED", async () => {
  const LAT2: Lattice = { scheme: "view", cells: 256, f0Hz: 6250, t0Ns: 1e9, levelsF: 20, levelsT: 15 };
  const g = stubGl(W, H);
  const clock = { t: 0 };
  const asked: string[] = [];
  const T1 = 1000 * 256e9 + 100e9;
  let edge = T1;
  // Level (0,0) never answers; its parent answers at once, with the horizon the edge had then.
  const surface = new Surface(g.canvas, LAT2,
    (tex) => new TileCache(tex, (a) => {
      asked.push(keyOf(a));
      return a.levelT === 0 && a.levelF === 0 ? new Promise<TileData>(() => {}) : Promise.resolve({ ...tile(a, edge), t1Ns: extentOf(LAT2, a).t1Ns });
    }, { inFlight: 16, now: () => clock.t, serverMsGuess: 50 }),
    { pinParents: true });
  surface.setScale(-100, -60);
  let parentKey = "";
  for (let step = 0; step < 100; step++) {
    clock.t = step * FRAME_MS;
    edge = T1 + clock.t * 1e6;
    const box: Box = { f0Hz: 100e6, f1Hz: 100e6 + W * 6250, t0Ns: edge - 600e9, t1Ns: edge };
    const [r] = surface.render([{ id: "a", rect: { x: 0, y: 0, w: W, h: H }, box }]);
    surface.cache.refreshEdge(LAT2, edge, [{ box, levelF: r.levelF, levelT: r.levelT }]);
    await flush();
    await flush();
    if (!parentKey) parentKey = asked.find((k) => !k.includes("|0|0|")) ?? "";
  }
  assert.ok(parentKey, "premise: no parent pin was ever asked for");
  const times = asked.filter((k) => k === parentKey).length;
  assert.ok(times >= 2,
    `the coarse stand-in ${parentKey} was asked for ${times} time(s) in 10 s while it was all a following pane ` +
    "could draw at its live edge: stand-ins are never refreshed, so they draw nothing past their first horizon");
  assert.ok(surface.cache.refreshedAsStandIn.has(parentKey));
});

// ——— gap 3: wide panes ———

test("T-893 gap 3: EVERY column of a wide following pane gets its next row, including those still in flight", async () => {
  const clock = { t: 0 };
  const edgeAt = (t: number) => ROW * TILE_NS + 0.2e9 + t * 1e6;
  const asked = new Map<string, number>(); // key -> edge at first ask
  const waiting: { addr: TileAddr; due: number; resolve: (d: TileData) => void; asOf: number }[] = [];
  // Six columns; the even ones are slow enough to still be in flight when the row ends.
  const slow = (a: TileAddr) => a.tIndex === ROW && a.fIndex % 2 === 0;
  const cache = new TileCache<{ id: number }>(
    { upload: () => ({ id: 0 }), destroy: () => {} },
    (addr) => new Promise<TileData>((resolve) => {
      if (!asked.has(keyOf(addr))) asked.set(keyOf(addr), edgeAt(clock.t));
      waiting.push({ addr, due: clock.t + (slow(addr) ? 20_000 : 300), resolve, asOf: edgeAt(clock.t) });
    }),
    { inFlight: 16, now: () => clock.t, serverMsGuess: 300 },
  );
  const cols = 6;
  for (let step = 0; step * FRAME_MS < (TILE_NS / 1e6) + 1000; step++) {
    clock.t = step * FRAME_MS;
    const edgeNs = edgeAt(clock.t);
    const box: Box = { f0Hz: 10 * TILE_HZ + 1, f1Hz: (10 + cols) * TILE_HZ - 1, t0Ns: edgeNs - 2.8e9, t1Ns: edgeNs };
    const view: Viewport = { box, levelF: 0, levelT: 0 };
    cache.beginFrame();
    for (const a of tilesFor(LAT, box, 0, 0)) cache.acquire(a);
    cache.setViewports(LAT, [view]);
    cache.endFrame();
    cache.refreshEdge(LAT, edgeNs, [view]);
    await flush();
    for (let i = waiting.length - 1; i >= 0; i--) {
      const w = waiting[i];
      if (w.due > clock.t) continue;
      waiting.splice(i, 1);
      w.resolve(tile(w.addr, w.asOf));
      await flush();
    }
  }
  const late: string[] = [];
  for (let f = 10; f < 10 + cols; f++) {
    const next = keyOf({ device: "any", scheme: "view", levelF: 0, levelT: 0, fIndex: f, tIndex: ROW + 1, cells: 256 });
    const at = asked.get(next);
    if (at === undefined || at >= (ROW + 1) * TILE_NS) {
      late.push(`column ${f}${slow({ device: "any", scheme: "view", levelF: 0, levelT: 0, fIndex: f, tIndex: ROW, cells: 256 }) ? " (in flight)" : ""}: ` +
        (at === undefined ? "never asked" : `asked ${((at - (ROW + 1) * TILE_NS) / 1e9).toFixed(2)} s after the crossing`));
    }
  }
  assert.deepEqual(late, [],
    "the next row was not asked for before the edge entered it, for columns of the pane whose current row was still " +
    "pending — the look-ahead extended only resident columns:\n  " + late.join("\n  "));
});
