// T-444 — retune-on-pan. Panning a pane to un-tuned spectrum **offers** a retune; taking the offer
// is the one act that moves the radio, and it goes through T-343's single gate.
//
// Four claims, each with the control that stops a degenerate implementation passing it:
//
//  1. **A pan alone never commands the radio.** T-340's control, restated over the pane vocabulary:
//     drag ±1.0 of the whole 6 GHz surface, wheel-zoom at every scale, and compute the offer after
//     every single step — the call list must be **empty**. Control: the run must actually *produce*
//     an offer, or "it never offers anything" would pass.
//  2. **The explicit act does**, through `applyDeviceAction` — the rate first only when the window
//     in force is the wrong width, then the centre, snapped to the achievable grid, with the
//     backend's `device_id` in the toast rather than a client-invented one.
//  3. **Snapping and off-DC placement are reused, not reinvented** (T-341's `snapCenter`, T-418's
//     `span/4`). Asserted on the *plan*, so a second copy of the arithmetic would have to agree
//     with the first to pass — and asserted on the source, so it cannot be a second copy at all.
//  4. **At the band edge the offer is disabled, never clamped** (T-409's honesty rule), and a pane
//     that moved after the offer was drawn refuses with `"moved"` rather than retuning to the new
//     place (T-407's failure mode, structurally).
//
// Plus the spike's one client-side ask (T-437 §5.2): the growing edge's tiles are invalidated after
// a **successful** retune, and only then.

import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import type { FrequencyGrid } from "../src/navigation";
import type { ActiveWindow } from "../src/navigators";
import { CELL } from "../src/surface/cellrule";
import { keyOf, type Lattice, type TileAddr } from "../src/surface/lattice";
import { PaneModel, type PaneState } from "../src/surface/panes";
import {
  acceptPaneRetune, coveringWindow, offerAcceptable, offerLabel, paneRetuneAction, paneRetuneOffer,
  type PaneRetuneOffer, type PaneRetuneSite,
} from "../src/surface/retune";
import { TileCache } from "../src/surface/tilecache";
import type { TileData } from "../src/surface/tile";
import { createStore } from "../src/app/store";
import { initialState } from "../src/app/state";
import type { AppContext } from "../src/app/context";

const S = 1e9;
const T0 = 1_700_000_000 * S;
const BOUNDS = { f0Hz: 1e6, f1Hz: 6e9, t0Ns: T0 - 120 * S, t1Ns: T0 };
const LAT: Lattice = { scheme: "view", cells: 256, f0Hz: 6250, t0Ns: 1e9, levelsF: 20, levelsT: 15 };
const STEP = 30e6 / 2 ** 20; // HACKRF_ONE_TUNING_STEP_HZ, ≈ 28.6102294921875 Hz

/** The achievable (centre, span) grid a HackRF-class front end reports on `GET /api/navigation`. */
const GRID: FrequencyGrid = {
  device_id: "hackrf:0000000000000000fake0000000000ab", driver: "hackrf-one", controllable: true,
  ranges_hz: [[1e6, 6e9]], center_step: "uniform", center_step_hz: STEP,
  spans_hz: { min: 2e6, max: 20e6 }, max_live_span_hz: 20e6,
  current: { center_hz: 100e6, span_hz: 2.4e6 },
};

/** One front end, tuned where the panes start. */
const WINDOWS: ActiveWindow[] = [{
  deviceId: "hackrf:0000000000000000fake0000000000ab", driver: "hackrf-one",
  centerHz: 100.8e6, spanHz: 2.4e6, loHz: 99.6e6, hiHz: 102e6,
}];

const model = () => new PaneModel({
  bounds: BOUNDS, lattice: LAT, width: 1200, height: 800,
  freq: { centerHz: 100.8e6, spanHz: 1e6 }, spanNs: 20 * S,
});

const offerFor = (p: PaneState, edgeNs = T0, windows = WINDOWS, grid: FrequencyGrid | null = GRID) =>
  paneRetuneOffer(p, windows, grid, edgeNs);

/** A context whose client records every call, so "reached the device" is observable (T-343). */
function deviceSpyCtx(live = true) {
  const calls: { method: string; path: string; body: unknown }[] = [];
  const store = createStore(initialState());
  store.set((s) => ({
    device: {
      ...s.device, loaded: true, live, deviceId: GRID.device_id, sampleRateHz: 2_400_000,
      centerHz: 100.8e6,
      centerGrid: { ranges_hz: GRID.ranges_hz, center_step_hz: GRID.center_step_hz },
    },
  }));
  const client = {
    post: (path: string, body: unknown) => { calls.push({ method: "POST", path, body }); return Promise.resolve({}); },
    get: (path: string) => { calls.push({ method: "GET", path, body: null }); return Promise.resolve({}); },
  } as unknown as AppContext["client"];
  return { ctx: { store, client, token: "t" } as AppContext, calls };
}

/** A site whose `offerNow` re-derives from the live model, which is what makes the guard a guard. */
function siteOver(m: PaneModel, edgeNs = T0): PaneRetuneSite & { invalidations: number } {
  const site = {
    invalidations: 0,
    offerNow: (id: string) => { const p = m.get(id); return p ? offerFor(p, edgeNs) : null; },
    invalidateEdge: () => { site.invalidations++; return 7; },
  };
  return site;
}

// ---------------------------------------------------------------------------
// 1. THE CONTROL: a pan is a pan
// ---------------------------------------------------------------------------

test("T-340's control holds for a pane: pan and wheel across the whole 6 GHz surface reach NO route", () => {
  const calls: unknown[] = [];
  const g = globalThis as { fetch?: unknown };
  const real = g.fetch;
  g.fetch = (...args: unknown[]) => { calls.push(args); return Promise.reject(new Error("a pan must not reach the network")); };
  const offers: (PaneRetuneOffer | null)[] = [];
  try {
    const m = model();
    const a = m.list()[0].id;
    const b = m.split(a, "columns")!;
    // Drags of ±1.0 of the whole bar, in both directions, plus the small ones a finger makes — and
    // the offer is recomputed after EVERY step, because computing it is what a mount does on move.
    const full = BOUNDS.f1Hz - BOUNDS.f0Hz;
    for (const frac of [0.001, 0.05, 0.5, 1, -0.001, -0.5, -1, 0.25]) {
      m.panFreq(a, frac * full);
      offers.push(offerFor(m.get(a)!));
      m.panFreq(b, -frac * full);
      offers.push(offerFor(m.get(b)!));
    }
    for (const factor of [0.01, 0.5, 2, 100, 1000]) {
      m.zoomFreq(a, factor, 0.25);
      offers.push(offerFor(m.get(a)!));
    }
    // …and the time axis, which can never justify a retune at all.
    m.panTime(a, -60 * S); m.zoomTime(a, 4); m.pause(b); m.follow(b);
    offers.push(offerFor(m.get(a)!), offerFor(m.get(b)!));
  } finally { if (real) g.fetch = real; else delete g.fetch; }
  assert.deepEqual(calls, [], "a pane pan reached the network: a pan must never command the radio");
  // The control that stops "it never offers" from passing this.
  assert.ok(offers.some((o) => offerAcceptable(o)), "no acceptable offer was produced, so the run proves nothing");
});

test("a pane already covered by a live window is offered nothing; a pane panned off it is", () => {
  const m = model();
  const p = m.list()[0];
  assert.equal(coveringWindow(WINDOWS, 100.3e6, 101.3e6)?.deviceId, GRID.device_id);
  assert.equal(offerFor(p), null, "the radio is looking here already");
  m.panFreq(p.id, 300e6);
  const o = offerFor(m.get(p.id)!);
  assert.ok(o && o.plan.ok, "panning to un-tuned spectrum must offer a retune");
});

test("a pane straddling the tuned window's edge is un-tuned: coverage is containment, not overlap", () => {
  assert.equal(coveringWindow(WINDOWS, 101.5e6, 102.5e6), null);
  // And a pane pinned to another front end is not covered by this one's window.
  assert.equal(coveringWindow(WINDOWS, 100.3e6, 101.3e6, "hackrf:other"), null);
  assert.ok(coveringWindow(WINDOWS, 100.3e6, 101.3e6, GRID.device_id!));
});

test("a pane frozen in the past is offered nothing: a retune cannot change what was already captured", () => {
  const m = model();
  const p = m.list()[0].id;
  m.panFreq(p, 300e6);
  assert.ok(offerFor(m.get(p)!), "the live pane is offered a retune");
  m.panTime(p, -60 * S);
  assert.equal(offerFor(m.get(p)!), null, "a pane scrubbed into history has nothing to gain from a retune");
});

test("an unreported grid offers nothing: not knowing the front end's range is not evidence it can reach here", () => {
  const m = model();
  const p = m.list()[0].id;
  m.panFreq(p, 300e6);
  assert.equal(offerFor(m.get(p)!, T0, WINDOWS, null), null);
});

// ---------------------------------------------------------------------------
// 2. The explicit act, through the one gate
// ---------------------------------------------------------------------------

test("taking the offer retunes through T-343's gate: the covering rate, then the snapped centre", async () => {
  const m = model();
  const p = m.list()[0].id;
  m.panFreq(p, 300e6);          // ~400.8 MHz, nowhere near the tuned window
  const site = siteOver(m);
  const offer = site.offerNow(p)!;
  const { ctx, calls } = deviceSpyCtx();
  const out = await acceptPaneRetune(ctx, site, offer);
  assert.equal(out.ok, true);
  assert.deepEqual(calls.map((c) => c.path), ["/api/control/rate", "/api/control/center"]);
  assert.deepEqual(calls[0].body, { sample_rate_hz: 2_000_000 }, "the narrowest covering window, not the one in force");
  const posted = (calls[1].body as { center_hz: number }).center_hz;
  // On the achievable grid (T-341), and it is the plan's own centre — re-snapping it in the gate is
  // a no-op, so the number on the button is the number that goes out.
  assert.ok(offer.plan.ok);
  assert.equal(posted, offer.plan.ok ? offer.plan.centerHz : NaN);
  assert.ok(Math.abs(posted / STEP - Math.round(posted / STEP)) < 1e-6, `${posted} is off the synthesiser grid`);
  // The radio it moved is named from the backend's own state, not claimed by the client.
  assert.match(ctx.store.get().toast.text, /Retuning hackrf:0000000000000000fake0000000000ab to/);
});

test("the rate is not re-posted when the window in force is already the right width", async () => {
  const m = new PaneModel({
    bounds: BOUNDS, lattice: LAT, width: 1200, height: 800,
    freq: { centerHz: 400e6, spanHz: 1.8e6 }, spanNs: 20 * S,
  });
  const p = m.list()[0].id;
  const site = siteOver(m);
  const { ctx, calls } = deviceSpyCtx();
  ctx.store.set((s) => ({ device: { ...s.device, sampleRateHz: 2_000_000 } }));
  const out = await acceptPaneRetune(ctx, site, site.offerNow(p)!);
  assert.equal(out.ok, true);
  assert.deepEqual(calls.map((c) => c.path), ["/api/control/center"], "a rate already right is not re-posted");
});

test("a replay is told so, and reaches nothing: the offer is still not a second path to the device", async () => {
  const m = model();
  const p = m.list()[0].id;
  m.panFreq(p, 300e6);
  const site = siteOver(m);
  const { ctx, calls } = deviceSpyCtx(false);
  const out = await acceptPaneRetune(ctx, site, site.offerNow(p)!);
  assert.deepEqual(out, { ok: false, reason: "refused" });
  assert.deepEqual(calls, [], "not live: nothing may be posted");
  assert.equal(site.invalidations, 0, "and nothing is invalidated, because nothing was retuned");
});

// ---------------------------------------------------------------------------
// 3. Snapping and off-DC placement are REUSED
// ---------------------------------------------------------------------------

test("the planned centre is on the grid and places the pane OFF DC, at T-418's derived span/4", () => {
  const m = new PaneModel({
    bounds: BOUNDS, lattice: LAT, width: 1200, height: 800,
    freq: { centerHz: 433.92e6, spanHz: 200e3 }, spanNs: 20 * S,
  });
  const o = offerFor(m.list()[0])!;
  assert.ok(o.plan.ok);
  if (!o.plan.ok) return;
  // The narrowest achievable window (the 2 Msps floor), because detail comes from resolution, not
  // from an impossible narrow capture.
  assert.equal(o.plan.spanHz, 2e6);
  // The ideal offset is span/4; the pane is far narrower than the window, so the ideal is reachable
  // and only the snap's half step separates the two.
  assert.ok(Math.abs(Math.abs(o.plan.dcOffsetHz) - o.plan.spanHz / 4) <= STEP / 2 + 1e-6,
    `off DC by ${o.plan.dcOffsetHz}, not ~${o.plan.spanHz / 4}`);
  assert.equal(o.plan.clearsDc, true, "DC must fall outside the pane, not on it");
  assert.equal(o.plan.snappedCenter, true);
  assert.ok(Math.abs(o.plan.centerHz / STEP - Math.round(o.plan.centerHz / STEP)) < 1e-6);
  // The window holds the whole pane wherever the dodge put it.
  assert.equal(o.shortfallHz, 0);
  assert.match(offerLabel(o), /^Retune to 433\.\d+ MHz at 2\.000 MHz span \(clear of DC/);
});

test("the offer module re-derives no RF arithmetic of its own: it calls the planner", () => {
  const src = readFileSync("src/surface/retune.ts", "utf8");
  for (const route of ["/api/control/center", "/api/control/rate", "/api/control/gains"]) {
    assert.ok(!src.includes(route), `retune.ts must not name ${route}: the one path is applyDeviceAction`);
  }
  // The planner is imported, and neither the tuning step nor the quarter-band offset is restated.
  assert.ok(/import \{[^}]*retunePlan/.test(src), "the capture configuration must come from retunePlan");
  const code = src.split("\n").filter((l) => !/^\s*(\/\/|\*|\/\*)/.test(l)).join("\n");
  // (`1e6`/`1e3` survive: turning Hz into MHz for a label is presentation, not an RF fact.)
  assert.ok(!/28\.61|30e6|2 \*\* 20|2_000_000|20e6/.test(code), "an RF constant appeared in retune.ts");
  assert.ok(!/(dcOffset|snapCenter|smallestCoveringSpan)\s*[=(]/.test(code),
    "the placement and snapping arithmetic was re-derived instead of being left to retunePlan");
});

// ---------------------------------------------------------------------------
// 4. The band edge, and the moved pane
// ---------------------------------------------------------------------------

test("past the band edge the offer is DISABLED with its reason, never clamped to a nearer centre", async () => {
  // A pane whose centre is beyond the top of the device's tunable range. `snapCenter` would walk it
  // inward and call 6 GHz achievable; `containsCenter` is what refuses, and it is the reason T-392
  // kept them separate.
  const grid: FrequencyGrid = { ...GRID, ranges_hz: [[1e6, 500e6]] };
  const m = new PaneModel({
    bounds: BOUNDS, lattice: LAT, width: 1200, height: 800,
    freq: { centerHz: 700e6, spanHz: 1e6 }, spanNs: 20 * S,
  });
  const p = m.list()[0];
  const o = offerFor(p, T0, WINDOWS, grid)!;
  assert.ok(o, "the pane is un-tuned, so the offer exists — it is the ACTION that is refused");
  assert.equal(o.plan.ok, false);
  if (o.plan.ok) return;
  assert.equal(o.plan.reason, "center_out_of_range");
  assert.equal(offerAcceptable(o), false);
  assert.equal(paneRetuneAction(o), null, "a refusal must not become a smaller retune");
  assert.match(offerLabel(o), /Outside the front end's tunable range/);
  const { ctx, calls } = deviceSpyCtx();
  const site: PaneRetuneSite = { offerNow: () => o, invalidateEdge: () => 0 };
  assert.deepEqual(await acceptPaneRetune(ctx, site, o), { ok: false, reason: "not_acceptable" });
  assert.deepEqual(calls, [], "a disabled offer must reach nothing, even when pressed");
});

test("a pane wider than one capture window is survey overview: stated as such, and not retunable", () => {
  const m = new PaneModel({
    bounds: BOUNDS, lattice: LAT, width: 1200, height: 800,
    freq: { centerHz: 3e9, spanHz: 400e6 }, spanNs: 20 * S,
  });
  const o = offerFor(m.list()[0])!;
  assert.equal(o.plan.ok, false);
  if (o.plan.ok) return;
  assert.equal(o.plan.reason, "span_too_wide");
  assert.match(offerLabel(o), /survey overview/);
});

test("T-407's failure mode, structurally: a pane that MOVED refuses rather than retuning elsewhere", async () => {
  const m = model();
  const p = m.list()[0].id;
  m.panFreq(p, 300e6);
  const site = siteOver(m);
  const offer = site.offerNow(p)!;
  // The finger keeps dragging while the offer sits under it — the moving-target version of T-407's
  // "a stroke across the bar counted as travel along it".
  m.panFreq(p, 500e6);
  const { ctx, calls } = deviceSpyCtx();
  assert.deepEqual(await acceptPaneRetune(ctx, site, offer), { ok: false, reason: "moved" });
  assert.deepEqual(calls, [], "the radio went where the button never said");
  assert.equal(site.invalidations, 0);
  // The *fresh* offer is takeable: the refusal is about staleness, not about the pane.
  assert.equal((await acceptPaneRetune(ctx, site, site.offerNow(p)!)).ok, true);
});

test("a pan too small to change the snapped configuration is not a moved target", async () => {
  const m = model();
  const p = m.list()[0].id;
  m.panFreq(p, 300e6);
  const site = siteOver(m);
  const offer = site.offerNow(p)!;
  m.panFreq(p, STEP / 8); // far below one tuning step: the plan cannot notice
  const { ctx } = deviceSpyCtx();
  assert.equal((await acceptPaneRetune(ctx, site, offer)).ok, true);
});

// ---------------------------------------------------------------------------
// The growing edge is invalidated after a successful retune (T-437 §5.2)
// ---------------------------------------------------------------------------

function tileData(a: TileAddr): TileData {
  return {
    addr: a, key: keyOf(a), nf: 1, nt: 1, value: new Float32Array([-90]),
    state: new Uint8Array([CELL.OBSERVED]), tier: "live-iq", answeredLevel: a.levelF,
    fold: { frequency: "exact", time: "exact" }, rangeDb: { lo: -100, hi: -60 },
    bytes: 1024, serverInFlightLimit: null,
  };
}

/** A cache with `n` tile rows loaded, the last of which holds the live edge. */
async function loadedCache(edgeNs: number) {
  const destroyed: string[] = [];
  const asked: TileAddr[] = [];
  const cache = new TileCache<string>(
    { upload: (d) => d.key, destroy: (t) => { destroyed.push(t); } },
    (a) => { asked.push(a); return Promise.resolve(tileData(a)); },
    { inFlight: 64, now: () => 0 },
  );
  const tTile = LAT.t0Ns * LAT.cells;
  const tEdge = Math.floor(edgeNs / tTile);
  const addrs: TileAddr[] = [];
  for (let dt = 0; dt <= 3; dt++) {
    for (const levelT of [0, 2]) {
      const w = LAT.t0Ns * 2 ** levelT * LAT.cells;
      addrs.push({ device: "any", scheme: "view", levelF: 0, levelT, fIndex: 400, tIndex: Math.floor(edgeNs / w) - dt, cells: LAT.cells });
    }
  }
  cache.beginFrame();
  for (const a of addrs) cache.acquire(a);
  cache.endFrame();
  await new Promise((r) => setImmediate(r));
  return { cache, destroyed, asked, addrs, tEdge };
}

test("after a retune the growing edge's tiles are dropped — at every level, not only the finest", async () => {
  const { cache, addrs } = await loadedCache(T0);
  assert.equal(cache.residentTiles, addrs.length, "the run needs every tile resident to prove anything");
  const dropped = cache.invalidateEdge(LAT, T0);
  assert.ok(dropped >= 2, `only ${dropped} edge tiles dropped`);
  // One per level ladder: the edge tile at level_t 0 and the one at level_t 2 both hold the edge.
  assert.equal(cache.residentTiles, addrs.length - dropped);
  // …and the older rows, which describe spectrum the retune cannot rewrite, are kept.
  assert.ok(cache.residentTiles > 0, "invalidating the edge must not clear the whole cache");
});

test("a tile on another lattice is untouched: its scheme is part of its key", async () => {
  const { cache } = await loadedCache(T0);
  const before = cache.residentTiles;
  const other: Lattice = { ...LAT, scheme: "store:1" };
  assert.equal(cache.invalidateEdge(other, T0), 0);
  assert.equal(cache.residentTiles, before);
});

test("a fetch the retune overtook is dropped on arrival and asked for again, not served stale", async () => {
  let fetches = 0;
  let release: ((d: TileData) => void) | null = null;
  // Each answer is stamped with the fetch that produced it, so "the old tuning's tile is on screen"
  // is observable rather than inferred.
  const stamped = (a: TileAddr, n: number): TileData => ({ ...tileData(a), value: new Float32Array([n]) });
  const cache = new TileCache<string>(
    { upload: (d) => d.key, destroy: () => {} },
    (a) => {
      const n = ++fetches;
      return n === 1 ? new Promise<TileData>((res) => { release = () => res(stamped(a, 1)); }) : Promise.resolve(stamped(a, n));
    },
    { inFlight: 4, now: () => 0 },
  );
  const addr: TileAddr = { device: "any", scheme: "view", levelF: 0, levelT: 0, fIndex: 400, tIndex: Math.floor(T0 / (LAT.t0Ns * LAT.cells)), cells: LAT.cells };
  cache.beginFrame();
  cache.acquire(addr);
  cache.endFrame();
  assert.equal(cache.inFlightCount, 1);
  cache.invalidateEdge(LAT, T0);       // the retune lands while the tile is still in flight
  release!();                          // …and the old tuning's answer arrives afterwards
  for (let i = 0; i < 4; i++) await new Promise((r) => setImmediate(r));
  assert.equal(fetches, 2, "the overtaken tile was not re-requested, so the pane would go blank instead");
  assert.equal(cache.peek(addr)?.data.value[0], 2, "the tile of the tuning that just ended was served");
});

test("the invalidation happens ONLY after the front end took the retune", async () => {
  const m = model();
  const p = m.list()[0].id;
  m.panFreq(p, 300e6);
  const site = siteOver(m);
  const { ctx } = deviceSpyCtx();
  const out = await acceptPaneRetune(ctx, site, site.offerNow(p)!);
  assert.equal(out.ok && out.invalidated, 7);
  assert.equal(site.invalidations, 1, "invalidated once, after the retune, and not before it");
});
