// **T-1034: after a nudge, the departed band is fog all the way to the tuned window — no blank.**
//
// The user, 2026-09-25: "If I have the waterfall going for a while, then I nudge it +1/2 to the
// right, the fog-of-war applies correctly for the 99.5 to 100.2 tiles, but the 100.2 to 100.8 tiles
// do not load. Also happens after a refresh. The issue goes away if I zoom out."
//
// ——— THE DEFECT THIS FILE WAS WRITTEN AGAINST ———
//
// Server-side, reproduced on staging and on the mock: the finest tiles over the departed half
// nearest the new window answered `shadow.runs = 0` with one store time block `unsearched`. The
// tile's own-level last-known search (T-911) skips a time block holding no tile over its frequency,
// but one store block spans 600 kHz at the default FFT, the new window's edge bin lands in that
// block in every time block after the retune, and each was read in full for a band it holds nothing
// of — ~8 blocks (~20 s) spent the budget before the band's last live row. So those tiles came back
// unobserved with no shadow, and the pane drew them as the grey: a blank strip between the fog and
// the tuned window. The older departed half sat in a block the new window never touches, so it
// fogged correctly — exactly the split the user saw. The Rust half is asserted in `hk-api`
// (`a_nudged_away_band_carries_its_last_live_row_through_every_block_the_new_window_filled`) and
// through the mock in `hk-cli`'s api_contract (`a_nudged_away_band_is_fog_at_the_finest_level_…`);
// this file is the user's own claim, in pixels.
//
// ——— WHAT IS MEASURED ———
//
// The mock SDR (through the device route, never files into the pipeline — CLAUDE.md) opens on its
// recording, 100.8 MHz ± 1.2 MHz. After a dwell it is nudged +½ span to 102.0 MHz, and capture runs
// on for a minute — well past the ~20 s the old search could reach. `/surface.html` at 1280×800,
// following live, is then put over 99.9–101.1 MHz: the departed half nearest the new window, and the
// new window's first 300 kHz. Over the rows recorded ≥ 45 s after the nudge (and before the
// server's data edge), every screen column over the departed band is sampled. Blank is the product's
// own marks, HARVESTED from its legend rather than hard-coded (fog-of-war.e2e.mjs's technique): THE
// grey (`unobserved`) and the pane's not-loaded ground (`pending`). The claims:
//
//  1. no sampled pixel over the departed band is THE grey — the radio looked there, then left;
//  2. no column over the departed band is blank (grey or pending) in every sampled row;
//  3. premise: the tuned window's columns at the same rows are drawn (not blank), so the time
//     mapping lands on rows the pane really drew, and the view is the one asked for.
//
// Run it against a FRESH `hk` (`cargo build -p hk-cli --bin hk`, or HK_BIN): a stale seeded binary
// is the old server.
import test from "node:test";
import assert from "node:assert/strict";
import path from "node:path";
import { Browser, clipToUnoccluded } from "./harness.mjs";
import { UI_DIR, startBackend } from "./backend.mjs";

const ART = process.env.HK_E2E_ARTIFACTS ?? path.join(UI_DIR, "e2e", "artifacts");
// This file's own backend, at a fixed offset inside its lane's port band (fog-of-war.e2e.mjs's PORT
// comment). `startBackend` steps past a busy port, so a shared offset costs a step, never a crosstalk.
const PORT = Number(process.env.HK_E2E_PORT ?? 8791) + 4;

const FIXTURE_CENTRE_HZ = 100.8e6;
const NUDGE_HZ = 1.2e6; // half the recording's 2.4 MHz span
const NEW_LO_HZ = FIXTURE_CENTRE_HZ + NUDGE_HZ - 1.2e6; // the new window's lower tuned edge: 100.8 MHz
// The view: the departed half nearest the new window, and the new window's first 300 kHz.
const VIEW_CENTRE_HZ = 100.5e6, VIEW_SPAN_HZ = 1.2e6;
const DEPARTED_HZ = [99.95e6, NEW_LO_HZ - 5e3];
const TUNED_HZ = [NEW_LO_HZ + 50e3, 101.05e6];
// Capture before the nudge, so the band has a history; after it, long past the old search's reach.
const DWELL_S = 12;
const AFTER_S = 60;
const SAMPLE_FROM_S = 45; // rows at least this long after the nudge
const TIME_SPAN_TARGET_S = 20;
const MINIMAP_PX = 120; // `/surface.html`'s map strip (preview.ts's default), device px

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

/** `GET`, riding out the route's backpressure (`503` is "ask again" — T-690). */
async function get(backend, p, { tries = 60, waitMs = 200 } = {}) {
  for (let i = 0; ; i++) {
    const r = await fetch(`${backend.origin}${p}`, { headers: { authorization: `Bearer ${backend.token}` } });
    if (r.ok) return r.json();
    assert.ok(r.status === 503 && i < tries, `GET ${p} -> ${r.status}`);
    await sleep(waitMs);
  }
}

/** The one gated device action: the mock to `centerHz`, keeping its span (shadow-level.e2e.mjs). */
async function retune(backend, centerHz) {
  const nav = await get(backend, "/api/navigation");
  const w = nav.windows?.[0];
  assert.ok(w, `the mock reported no capture window: ${JSON.stringify(nav.windows)}`);
  for (let i = 0; i < 20; i++) {
    const r = await fetch(`${backend.origin}/api/control/window`, {
      method: "POST",
      headers: { authorization: `Bearer ${backend.token}`, "content-type": "application/json" },
      body: JSON.stringify({ center_hz: centerHz, sample_rate_hz: w.span_hz }),
    });
    const body = await r.text();
    if (r.ok) return { from: w };
    assert.ok(r.status === 409 || r.status === 503, `retune -> ${r.status}: ${body}`);
    await sleep(250);
  }
  assert.fail("the mock never accepted the retune");
}

/** The store's newest folded frame (capture time, s), from a finest tile's own answer. */
async function dataEdge(backend) {
  const t = await get(backend, "/api/tiles?level_f=0&level_t=0&f_index=0&t_index=0&cells=256");
  const tileHz = t.axes.frequency.tile_hz, tileS = t.axes.time.tile_s;
  const v = await get(backend,
    `/api/tiles?level_f=0&level_t=0&f_index=${Math.floor(VIEW_CENTRE_HZ / tileHz)}&t_index=${Math.floor(Date.now() / 1000 / tileS)}&cells=256`);
  return v.shadow?.edge_s ?? null;
}

async function waitEdge(backend, pastS, what, timeoutMs) {
  const t0 = Date.now();
  for (;;) {
    const e = await dataEdge(backend);
    if (Number.isFinite(e) && e > pastS) return e;
    assert.ok(Date.now() - t0 < timeoutMs, `${what}: the data edge never passed ${pastS.toFixed(2)} (edge ${e})`);
    await sleep(500);
  }
}

// ——— the page ———

const PANE_ROWS = `JSON.stringify([...document.querySelectorAll('.hk-surface-viewport[data-viewport="pane"]')].map((v) => ({
  where: v.querySelector('.hk-surface-where')?.textContent ?? '',
  counts: v.querySelector('.hk-surface-counts')?.textContent ?? '',
  following: v.getAttribute('data-following'),
  tier: v.getAttribute('data-tier'),
  t0Ns: Number(v.getAttribute('data-t0-ns')),
  t1Ns: Number(v.getAttribute('data-t1-ns')),
})))`;
const panes = async (page) => JSON.parse(await page.eval(PANE_ROWS));

function windowOf(where) {
  const m = /^([\d.]+) MHz ± ([\d.]+) (Hz|kHz|MHz|GHz)/.exec(where);
  assert.ok(m, `the pane readout is not a frequency window: ${JSON.stringify(where)}`);
  const mult = { Hz: 1, kHz: 1e3, MHz: 1e6, GHz: 1e9 }[m[3]];
  const centerHz = Number(m[1]) * 1e6, halfHz = Number(m[2]) * mult;
  return { centerHz, halfHz, loHz: centerHz - halfHz, hiHz: centerHz + halfHz, spanHz: 2 * halfHz };
}

async function paneRect(page) {
  const box = await page.$rect('[data-slot="canvas"]');
  assert.ok(box && box.w > 400 && box.h > 300, `canvas has no box: ${JSON.stringify(box)}`);
  const dpr = await page.eval("window.devicePixelRatio || 1");
  return { x: box.x, y: box.y, w: box.w, h: box.h - MINIMAP_PX / dpr };
}

const xOf = (rect, win, hz) => rect.x + ((hz - win.loHz) / win.spanHz) * rect.w;
const yOf = (rect, row, tS) => rect.y + ((row.t1Ns / 1e9 - tS) / ((row.t1Ns - row.t0Ns) / 1e9)) * rect.h;

/** Frequency-only zoom and pan onto `centerHz ± spanHz/2` (shadow-level.e2e.mjs's `gotoFreq`). */
async function gotoFreq(page, rect, centerHz, spanHz) {
  const y = rect.y + rect.h / 2;
  const trail = [];
  for (let i = 0; i < 80; i++) {
    const win = windowOf((await panes(page))[0].where);
    trail.push(`${(win.centerHz / 1e6).toFixed(4)}±${(win.halfHz / 1e3).toFixed(1)}k`);
    const off = centerHz - win.centerHz;
    const inside = centerHz > win.loHz + win.spanHz * 0.05 && centerHz < win.hiHz - win.spanHz * 0.05;
    if (inside && win.spanHz > spanHz * 1.02) {
      const delta = Math.max(-400, Math.log(spanHz / win.spanHz) / 0.0015);
      await page.wheel({ x: xOf(rect, win, centerHz), y }, delta, { shift: true });
      await page.frames(2);
      continue;
    }
    if (Math.abs(off) > win.spanHz * 0.03) {
      const dx = Math.max(-rect.w * 0.42, Math.min(rect.w * 0.42, -off * rect.w / win.spanHz));
      const at = { x: rect.x + rect.w / 2, y };
      await page.drag(at, { x: at.x + dx, y }, 8);
      await page.frames(2);
      continue;
    }
    if (win.spanHz <= spanHz * 1.02) return { win, trail };
    await page.wheel({ x: rect.x + rect.w / 2, y }, 200, { shift: true });
    await page.frames(2);
  }
  assert.fail(`never reached ${centerHz} ± ${spanHz / 2}: ${trail.join(" -> ")}`);
}

/** Alt-wheel (time only) near the live edge until the pane spans at most `targetS`. */
async function zoomTime(page, rect, targetS) {
  for (let k = 0; k < 40; k++) {
    const row = (await panes(page))[0];
    const spanS = (row.t1Ns - row.t0Ns) / 1e9;
    if (spanS <= targetS) return row;
    await page.wheel({ x: rect.x + rect.w / 2, y: rect.y + rect.h * 0.1 }, -300, { alt: true });
    await page.frames(2);
  }
  assert.fail(`the pane never zoomed in time to ${targetS} s`);
}

/** Wait until the pane holds its own tiles: tiles in hand, no stand-ins, nothing pending. */
async function waitResident(page, { timeoutMs = 90000 } = {}) {
  const t0 = Date.now();
  let rows = [];
  for (;;) {
    rows = await panes(page);
    const ok = rows.length > 0 && rows.every((r) => {
      const m = /(\d+) tiles · (\d+) coarse stand-in\S* · (\d+) pending/.exec(r.counts);
      return m && Number(m[1]) > 0 && Number(m[2]) === 0 && Number(m[3]) === 0;
    });
    if (ok) { await page.frames(4); return { rows, ms: Date.now() - t0 }; }
    assert.ok(Date.now() - t0 < timeoutMs, `the pane never became resident: ${JSON.stringify(rows.map((r) => r.counts))}`);
    await sleep(300);
  }
}

/** A legend swatch's single flat colour (fog-of-war.e2e.mjs's `flatFromSwatch`). */
async function flatSwatch(page, mark) {
  const json = await page.eval(`(() => {
    const c = document.querySelector('.sp-legend-row[data-mark="${mark}"] canvas');
    if (!c) return null;
    const img = c.getContext('2d').getImageData(0, 0, c.width, c.height);
    return JSON.stringify({ w: c.width, h: c.height, data: Array.from(img.data) });
  })()`);
  assert.ok(json, `no legend swatch for mark="${mark}"`);
  const { w, h, data } = JSON.parse(json);
  const rgb = [data[0], data[1], data[2]];
  for (let i = 0; i < w * h * 4; i += 4) {
    assert.ok(data[i] === rgb[0] && data[i + 1] === rgb[1] && data[i + 2] === rgb[2],
      `the "${mark}" swatch is not a flat colour at pixel ${i / 4}`);
  }
  return rgb;
}

const pixelAt = (img, x, y) => { const i = (Math.round(y) * img.width + Math.round(x)) * 4; return [img.data[i], img.data[i + 1], img.data[i + 2]]; };
const near = (a, b, tol = 2) => Math.abs(a[0] - b[0]) <= tol && Math.abs(a[1] - b[1]) <= tol && Math.abs(a[2] - b[2]) <= tol;

test("T-1034: after a +½-span nudge the departed band is fog up to the tuned window — no blank column", { timeout: 540000 }, async (t) => {
  const backend = await startBackend({ port: PORT, mockDevice: true });
  t.after(() => backend.stop());

  // ——— the band's history, then the nudge, through the device route ———
  const began = await waitEdge(backend, 0, "the first frame", 60000);
  await waitEdge(backend, began + DWELL_S, `a ${DWELL_S} s dwell`, 90000);
  const armed = await dataEdge(backend);
  const dep = await retune(backend, FIXTURE_CENTRE_HZ + NUDGE_HZ);
  t.diagnostic(`nudged ${dep.from.center_hz} Hz -> ${FIXTURE_CENTRE_HZ + NUDGE_HZ} Hz; data edge at the nudge ${armed.toFixed(3)}`);
  const edgeAfter = await waitEdge(backend, armed + AFTER_S, `${AFTER_S} s after the nudge`, 240000);
  t.diagnostic(`data edge ${edgeAfter.toFixed(3)} (${(edgeAfter - armed).toFixed(1)} s after the nudge)`);

  // ——— the page, at the user's size ———
  const browser = await Browser.open();
  t.after(() => browser.close());
  const page = await browser.page(undefined, { width: 1280, height: 800 });
  assert.equal(await page.goto(`${backend.origin}/surface.html#token=${backend.token}`), "load");
  await page.waitForSurfaceMounted();
  await page.waitFor("the legend's unobserved and pending swatches",
    `!!document.querySelector('.sp-legend-row[data-mark="unobserved"] canvas') && !!document.querySelector('.sp-legend-row[data-mark="pending"] canvas')`,
    { timeoutMs: 20000 });
  const grey = await flatSwatch(page, "unobserved");
  const pending = await flatSwatch(page, "pending");
  t.diagnostic(`harvested: grey rgb(${grey}), pending rgb(${pending})`);
  await page.waitFor("the pane readout", `document.querySelectorAll('.hk-surface-viewport[data-viewport="pane"] .hk-surface-where').length > 0`, { timeoutMs: 20000 });

  const rect = await paneRect(page);
  const nav = await gotoFreq(page, rect, VIEW_CENTRE_HZ, VIEW_SPAN_HZ);
  t.diagnostic(`view: ${nav.trail.slice(-3).join(" -> ")}`);
  await zoomTime(page, rect, TIME_SPAN_TARGET_S);
  const res = await waitResident(page);
  const row = res.rows[0];
  t.diagnostic(`pane resident after ${res.ms} ms: ${row.tier} [${row.counts}] following=${row.following}, ` +
    `${((row.t1Ns - row.t0Ns) / 1e9).toFixed(1)} s tall`);

  // The rows to sample: recorded ≥ SAMPLE_FROM_S after the nudge, before the server's data edge.
  const edge = await dataEdge(backend);
  const img = await page.shot(path.join(ART, "nudge-fog.png"));
  const rows = await panes(page);
  const r0 = rows[0];
  const win = windowOf(r0.where);
  assert.ok(win.loHz <= DEPARTED_HZ[0] && win.hiHz >= TUNED_HZ[1],
    `the view ${JSON.stringify(win)} does not hold the departed band and the tuned window's edge`);
  const dpr = await page.eval("window.devicePixelRatio || 1");
  const S = (v) => v * dpr;
  const tFrom = Math.max(armed + SAMPLE_FROM_S, r0.t0Ns / 1e9 + 0.5), tTo = Math.min(edge - 1.0, r0.t1Ns / 1e9 - 0.5);
  assert.ok(tTo > tFrom + 1, `no sample rows: [${tFrom.toFixed(2)}, ${tTo.toFixed(2)}] (pane ${r0.t0Ns / 1e9}..${r0.t1Ns / 1e9}, edge ${edge})`);
  const ys = Array.from({ length: 7 }, (_, i) => yOf(rect, r0, tFrom + ((tTo - tFrom) * (i + 0.5)) / 7));
  // Only columns nothing foreign covers (T-801).
  const unocc = await page.unoccludedColumns('[data-slot="canvas"]', { y0: Math.min(...ys), y1: Math.max(...ys) });
  const clip = clipToUnoccluded(rect, rect, unocc);
  const columns = (lo, hi) => {
    const xs = [];
    for (let x = Math.ceil(Math.max(xOf(rect, win, lo), clip.x)); x <= Math.floor(Math.min(xOf(rect, win, hi), clip.x + clip.w - 1)); x++) xs.push(x);
    return xs;
  };
  const sample = (xs) => xs.map((x) => {
    const px = ys.map((y) => pixelAt(img, S(x) + dpr / 2, S(y)));
    return { x, px, grey: px.filter((c) => near(c, grey)).length, blank: px.filter((c) => near(c, grey) || near(c, pending)).length };
  });
  const departed = sample(columns(...DEPARTED_HZ));
  const tuned = sample(columns(...TUNED_HZ));
  const hzOf = (x) => win.loHz + ((x - rect.x) / rect.w) * win.spanHz;
  // What the server holds at the sampled rows, finest node, one tile each side of the new edge.
  {
    const ax = await get(backend, "/api/tiles?level_f=0&level_t=0&f_index=0&t_index=0&cells=256");
    const tileHz = ax.axes.frequency.tile_hz, tileS = ax.axes.time.tile_s;
    for (const hz of [NEW_LO_HZ - tileHz / 2, NEW_LO_HZ + tileHz * 1.5]) {
      const v = await get(backend, `/api/tiles?level_f=0&level_t=0&f_index=${Math.floor(hz / tileHz)}&t_index=${Math.floor((tFrom + tTo) / 2 / tileS)}&cells=256`);
      t.diagnostic(`server at ${(hz / 1e6).toFixed(3)} MHz: observed_cells ${v.grid?.observed_cells}, short_circuit ${v.resolution?.short_circuit?.applied}, ` +
        `shadow runs ${v.shadow?.runs}, unsearched ${JSON.stringify(v.shadow?.search?.unsearched)}`);
    }
  }
  t.diagnostic(`sampled ${departed.length} departed and ${tuned.length} tuned columns x ${ys.length} rows ` +
    `(${(tFrom - armed).toFixed(1)}..${(tTo - armed).toFixed(1)} s after the nudge)`);

  // Premise: the tuned window at the same rows is drawn, so these rows are rows the pane drew.
  assert.ok(departed.length >= 200, `only ${departed.length} departed columns on screen`);
  assert.ok(tuned.length >= 40, `only ${tuned.length} tuned columns on screen`);
  const tunedBlank = tuned.filter((c) => c.blank > 0);
  assert.equal(tunedBlank.length, 0,
    `the tuned window is blank at the sampled rows in ${tunedBlank.length} columns: the rows are not ones the pane drew ` +
    `(${tunedBlank.slice(0, 4).map((c) => `${(hzOf(c.x) / 1e6).toFixed(4)} MHz ${JSON.stringify(c.px)}`).join("; ")})`);

  // 1. Never THE grey over the departed band.
  const greyCols = departed.filter((c) => c.grey > 0);
  // 2. No blank column.
  const blankCols = departed.filter((c) => c.blank === ys.length);
  const describe = (cs) => {
    if (cs.length === 0) return "none";
    return `${cs.length} columns, ${(hzOf(cs[0].x) / 1e6).toFixed(4)}–${(hzOf(cs[cs.length - 1].x) / 1e6).toFixed(4)} MHz, ` +
      `e.g. ${JSON.stringify(cs[0].px[0])}`;
  };
  t.diagnostic(`departed: grey ${describe(greyCols)}; blank ${describe(blankCols)}`);
  assert.equal(greyCols.length, 0,
    `the departed band is drawn as THE grey (never observed) in ${describe(greyCols)} — the radio looked there, then left: that is fog`);
  assert.equal(blankCols.length, 0,
    `the departed band is BLANK (grey or not-loaded in every sampled row) in ${describe(blankCols)} — the user's gap between the fog and the tuned window`);
});
