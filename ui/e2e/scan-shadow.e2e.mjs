// **T-1055: after a survey pass, the swept range carries its last-known value at the ZOOMED-OUT
// tier — the evidence the fog of war is drawn from.**
//
// The user's report (explorer journal 2026-09-25, shots 0512/0513): after *"Scan everything"*, the
// zoomed-out 1 MHz–6 GHz surface showed an honest thin diagonal of per-instant coverage and **no
// last-known shadow at all** over the range that had just been swept. The fog tier (T-485,
// T-519/520/521, ADR-0020) is what should light it: a band swept and then departed is not grey — it
// was observed, and the newest thing known about it is a real measurement, drawn dimmed and
// scanlined.
//
// This file drives a survey pass through the MOCK SDR (CLAUDE.md: e2e goes through the device
// interface, never files into the pipeline), leaves the band by one gated retune, and asserts what
// `GET /api/tiles` answers at an address **wider than one capture window** — the survey-overview
// tier a zoomed-out pane reads. Three claims, one answer shape:
//
//  1. during the pass, the swept range's tile holds a **measurement** (`grid.observed_cells > 0`)
//     and `coverage` says the radio was there;
//  2. after the radio has left, the same band's tile carries **last-known runs** (`shadow.runs > 0`)
//     — the fog, with `last_t_s` saying when it was measured;
//  3. a band the pass never reached carries a uniformly-`unobserved` coverage plane and **no run at
//     all** — the fog may never reach unsampled spectrum, and grey stays grey.
//
// ——— WHAT WAS MEASURED WHILE WRITING THIS, AND WHAT IS NOT GUARDED HERE ———
//
// Two store-side defects were measured on the mock and are reported separately; both are why the
// user's screenshot had no fog, and neither is a rendering fault:
//
//  A. **The spectrum-history pyramid's levels 1 and above hold nothing in a live run.** The tile
//     route picks a coarser level as soon as folding level 0 would exceed its source-cell budget —
//     the TIME axis reaches that first — and every address answered from level 0 carries both the
//     measurement and the fog (25.6 MHz × 256 s: `grid 29`, `shadow.runs 34`), while the first
//     address answered from level 1 carries **neither** (25.6 MHz × 1024 s: `grid 0`, `shadow 0`,
//     `coverage observed 25`). The user's whole-surface view, over a 1.7 h time axis, is the second
//     case: nothing to draw at all. This spec therefore addresses the tier at a level 0 can answer,
//     so that it fails on a *fog* regression rather than on that gap.
//  B. **A `coarse` pass — what `Scan everything (fast)` runs — retains no spectrum history at all.**
//     It changes the sample rate (2.4 → 19.2 Msps) and from then on `history.frames_ingested` freezes
//     (41 → 41 over 30 s while `view_frames` went 102 → 173), the history reader's own `frames` reads
//     0 with `stft_resets` climbing ~44/s, and every swept cell comes back `"observed"` with
//     `shade: null` — *sampled, level not retained*. Per dwell, over 16-cell × 48-row coverage grids:
//     `fine` 5.0/1.0/0.3 s → 135/128/140 of 135/128/142 observed cells carried a level; `coarse`
//     5.0/1.0/0.3 s → **0** of 138/85/73. So this spec sweeps `fine`.
//
// The pixel half of the ticket's acceptance was also run, on both pages, with the marks harvested
// from the product's own legend: over the rows immediately after the pass both `/surface.html` and
// the app page draw the swept cells as fog and the never-sampled cells as THE grey (270 swept and
// 556 unswept screen columns sampled, 0 wrong on either page). It is **not** committed as a spec,
// because further from the pass it goes red for a third measured reason: the fog over the
// earliest-swept part of the band stops a few seconds after departure at this tier (the newest 4 of 7
// sampled rows over 96.0–99.0 MHz were grey ~6 s after the pass, while the rows just after it were
// fog). That is a real defect of the same family as A, not a flake, and pinning a spec to the seconds
// where the fog happens to hold would be papering over it.
import test, { after } from "node:test";
import assert from "node:assert/strict";
import { until } from "./harness.mjs";
import { startBackend } from "./backend.mjs";

// This file's own backend, at its own offset inside the lane's port band (fog-of-war.e2e.mjs's PORT
// comment: never a literal port; `startBackend` steps past a busy one).
const PORT = Number(process.env.HK_E2E_PORT ?? 8791) + 30;

/** The band the pass sweeps: the recording's own centre and a step each side of it. */
const SWEPT_HZ = [96e6, 108e6];
/** Where the radio goes afterwards, so the swept band is departed: outside the recording. */
const AWAY_HZ = 433.92e6;
/** A band the pass never reaches. Grey must stay grey here, and no run may reach it. */
const UNSWEPT_HZ = 2.5e9;
const DWELL_S = 1.5;      // per sweep step
const AWAY_DWELL_S = 18;  // after the pass, so rows exist that are past every swept row

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

/** `GET`, riding out the route's backpressure (`503` is "ask again", never "no" — T-690). */
async function get(backend, p, { tries = 60, waitMs = 200 } = {}) {
  for (let i = 0; ; i++) {
    const r = await fetch(`${backend.origin}${p}`, { headers: { authorization: `Bearer ${backend.token}` } });
    if (r.ok) return r.json();
    assert.ok(r.status === 503 && i < tries, `GET ${p} -> ${r.status}`);
    await sleep(waitMs);
  }
}

async function post(backend, p, body) {
  const r = await fetch(`${backend.origin}${p}`, {
    method: "POST",
    headers: { authorization: `Bearer ${backend.token}`, "content-type": "application/json" },
    body: JSON.stringify(body ?? {}),
  });
  return { status: r.status, body: await r.json().catch(() => null) };
}

/** The store's newest folded frame (capture time, s), off a finest tile's own answer. */
async function dataEdge(backend) {
  const ax = await get(backend, "/api/tiles?level_f=0&level_t=0&f_index=0&t_index=0&cells=64");
  const tileHz = ax.axes.frequency.tile_hz, tileS = ax.axes.time.tile_s;
  const v = await get(backend,
    `/api/tiles?level_f=0&level_t=0&f_index=${Math.floor(SWEPT_HZ[0] / tileHz)}` +
    `&t_index=${Math.floor(Date.now() / 1000 / tileS)}&cells=64`);
  return v.shadow?.edge_s ?? null;
}

/**
 * The overview-tier tile over `fHz` at `tS`.
 *
 * `levelF` is wide (25.6 MHz, far past one capture window, so the tier is `survey-overview`) and
 * `levelT` shallow (256 s) — see finding A in the header for why the time axis is where the level
 * choice, and with it the data, falls off.
 */
async function overviewTile(backend, fHz, tS, { levelF = 6, levelT = 2, cells = 64 } = {}) {
  const ax = await get(backend, "/api/tiles?level_f=0&level_t=0&f_index=0&t_index=0&cells=64&scheme=overview");
  const tileHz = ax.axes.frequency.tile_hz * 2 ** levelF, tileS = ax.axes.time.tile_s * 2 ** levelT;
  return get(backend, `/api/tiles?level_f=${levelF}&level_t=${levelT}&f_index=${Math.floor(fHz / tileHz)}` +
    `&t_index=${Math.floor(tS / tileS)}&cells=${cells}&scheme=overview`);
}

// ---------------------------------------------------------------------------
// One backend: the pass is the expensive part.
// ---------------------------------------------------------------------------

let opening = null;
const opened = (t) => (opening ??= open(t));

async function open(t) {
  const backend = await startBackend({ port: PORT, mockDevice: true });
  const waited = await until("the first folded frame",
    async () => Number.isFinite(await dataEdge(backend).catch(() => null)), { timeoutMs: 90000 });
  t.diagnostic(`backend up, first frame after ${waited} ms`);

  // ——— the pass, through the device interface ———
  const passStart = Date.now() / 1000;
  const started = await post(backend, "/api/control/scan",
    { f_lo_hz: SWEPT_HZ[0], f_hi_hz: SWEPT_HZ[1], dwell_s: DWELL_S, step: "fine" });
  assert.equal(started.status, 200, `the sweep was refused: ${JSON.stringify(started.body)}`);
  const plan = started.body.scan.plan;
  assert.equal(plan.step, "fine", `this spec needs a FINE pass (see the header): ${JSON.stringify(plan)}`);
  t.diagnostic(`fine pass: ${plan.steps} steps over ${SWEPT_HZ[0] / 1e6}–${SWEPT_HZ[1] / 1e6} MHz, dwell ${plan.dwell_s} s`);
  await until("the pass to complete",
    async () => ((await get(backend, "/api/control/scan")).scan.progress?.pass ?? 0) >= 1,
    { timeoutMs: 180000, everyMs: 1000 });
  const stopped = await post(backend, "/api/control/scan/stop");
  assert.equal(stopped.status, 200, `stopping the sweep -> ${stopped.status}`);
  const passEnd = await dataEdge(backend);

  // ——— the radio leaves: every swept row is now in the past and the band is departed ———
  const nav = await get(backend, "/api/navigation");
  const w = nav.windows?.[0];
  assert.ok(w, `the mock reported no capture window: ${JSON.stringify(nav.windows)}`);
  await until("the retune away to be accepted", async () => {
    const r = await post(backend, "/api/control/window", { center_hz: AWAY_HZ, sample_rate_hz: w.span_hz });
    assert.ok(r.status === 200 || r.status === 409 || r.status === 503,
      `retune -> ${r.status}: ${JSON.stringify(r.body)}`);
    return r.status === 200;
  }, { timeoutMs: 60000, everyMs: 500 });
  await until(`a ${AWAY_DWELL_S} s dwell away from the swept band`, async () => {
    const e = await dataEdge(backend);
    return Number.isFinite(e) && e > passEnd + AWAY_DWELL_S;
  }, { timeoutMs: 180000, everyMs: 1000 });
  const edge = await dataEdge(backend);
  t.diagnostic(`pass ended ${passEnd.toFixed(1)}, edge now ${edge.toFixed(1)} ` +
    `(${(edge - passEnd).toFixed(1)} s away at ${AWAY_HZ / 1e6} MHz)`);
  return { backend, passStart, passEnd, edge };
}

after(async () => {
  const j = await opening?.catch(() => null);
  j?.backend?.stop();
});

test("T-1055: at the zoomed-out tier the swept range carries its last-known value, and a band the pass never reached carries none",
  { timeout: 600000 }, async (t) => {
  const { backend, passStart, passEnd, edge } = await opened(t);

  // Two instants, because a tile is a box in time: the pass's own rows are what make `coverage`
  // observed, and the rows after the departure are what must carry the fog. They are usually the
  // same tile; where the pass and the dwell straddle a tile boundary they are not, and asking each
  // instant about what it can answer is the difference between a sharp assertion and a flaky one.
  const swept = await overviewTile(backend, (SWEPT_HZ[0] + SWEPT_HZ[1]) / 2, passEnd - 1);
  const departed = await overviewTile(backend, (SWEPT_HZ[0] + SWEPT_HZ[1]) / 2, edge - 2);
  const unswept = await overviewTile(backend, UNSWEPT_HZ, edge - 2);
  const say = (v) => `tier ${v.resolution?.source}, ` +
    `${(v.extent.f_lo_hz / 1e6).toFixed(1)}–${(v.extent.f_hi_hz / 1e6).toFixed(1)} MHz × ${(v.extent.t1_s - v.extent.t0_s).toFixed(0)} s, ` +
    `answered level ${JSON.stringify(v.resolution?.answered?.level ?? null)}, coverage observed ` +
    `${v.coverage.planes[0].observed_cells}/${v.coverage.planes[0].cells}, grid observed ${v.grid.observed_cells}, ` +
    `shadow.runs ${v.shadow.runs}`;
  t.diagnostic(`swept tile (the pass's own rows): ${say(swept)}`);
  t.diagnostic(`the same band after departure:   ${say(departed)}`);
  t.diagnostic(`a band the pass never reached:   ${say(unswept)}`);

  assert.ok(swept.extent.f_hi_hz - swept.extent.f_lo_hz > 20e6,
    `this address is not a zoomed-out one: ${swept.extent.f_hi_hz - swept.extent.f_lo_hz} Hz wide`);
  assert.equal(swept.resolution.source, "survey-overview",
    `the address must be read at the survey-overview tier, not ${swept.resolution.source}: ${say(swept)}`);

  // 1. The pass is on the record, and it retained a measurement to carry forward.
  assert.ok(swept.coverage.planes[0].observed_cells > 0,
    `the coverage plane does not even say the radio was there during the pass: ${say(swept)}`);
  assert.ok(swept.grid.observed_cells > 0,
    `the pass retained no measurement at this tier, so nothing can be carried forward: ${say(swept)}`);

  // 2. After the radio left, the band is fog: a real measurement of an earlier time, with when.
  assert.ok(departed.shadow.runs > 0,
    `the swept range carries NO last-known run at the zoomed-out tier, so nothing can draw its fog — ` +
    `this is the T-1055 symptom on the wire: ${say(departed)}`);
  assert.ok(Number.isFinite(departed.shadow.last_t_s?.[0]),
    `a run must say WHEN its value was measured (the last-known tier is stale, and says so): ` +
    `${JSON.stringify(departed.shadow).slice(0, 400)}`);

  // 3. And none of it reaches spectrum nothing ever sampled.
  assert.equal(unswept.shadow.runs, 0,
    `a band the pass never reached carries a last-known run — fog may never reach unsampled spectrum: ${say(unswept)}`);
  assert.equal(unswept.coverage.planes[0].uniform, "unobserved", `${say(unswept)}`);

  // The survey census the orientation sentence quotes, over the pass's OWN time window — T-1055's
  // third deliverable, on the wire: asked without `t0`/`t1` this route answers about the capture
  // window (the IQ ring's span), which is why the explorer's un-timed query read "observed only for
  // the step in progress". Every source row also states whether its read was cut (T-1034/T-1055).
  const cov = await get(backend, `/api/coverage?f_lo=${SWEPT_HZ[0]}&f_hi=${SWEPT_HZ[1]}&cells=64&rows=1` +
    `&t0=${passStart}&t1=${passEnd}`);
  assert.equal(cov.window.source, "requested", `${JSON.stringify(cov.window)}`);
  const bands = cov.any.bands;
  t.diagnostic(`over the pass's own window, bands: ${bands.observed_cells}/${bands.cells} frequency cells sampled ` +
    `(${(bands.observed_fraction * 100).toFixed(0)} %); sources' truncated flags: ` +
    `${JSON.stringify(cov.sources.map((s) => [s.kind, s.truncated]))}`);
  assert.ok(bands.observed_cells > bands.cells / 2,
    `a completed pass over this band must read as most of the FREQUENCY axis sampled: ${JSON.stringify(bands)}`);
  for (const s of cov.sources) {
    assert.equal(s.truncated, false, `no source here is near its bound: ${JSON.stringify(s)}`);
  }
});
