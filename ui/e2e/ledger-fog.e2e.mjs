// **T-1058: after a retune, the departed band is fog at every zoom — no black, no grey.**
//
// The user, 2026-09-25: "If I zoom in close enough that the most recent sample for a region is
// outside of the viewport, it does not render. If I pan down so that the last seen sample is in the
// viewport, SOME of the tiles resolve, but not all. What we really need is to render all of the
// fog-of-war tiles."
//
// ——— THE DEFECT THIS FILE WAS WRITTEN AGAINST ———
//
// The tile route found a departed band's last-known value by a bounded search per tile. At a fine
// zoom the band's last row is many search steps below the viewport, the budget ran out, and the
// tile carried no shadow — drawn as THE grey; tiles nearer the last row came within budget, which is
// the "some, not all". T-1058 keeps a last-known LEDGER in the store (per front end and finest
// frequency cell, updated as each row arrives) and the tile's `shadow` reads it: no search, no
// budget. The Rust half is asserted in `hk-cli`'s api_contract
// (`a_departed_band_is_fog_at_every_zoom_from_the_ledger_and_survives_a_restart`); this file is the
// user's own claim, in pixels.
//
// ——— WHAT IS MEASURED ———
//
// The mock SDR (through the device route, never files into the pipeline — CLAUDE.md) opens on its
// recording, 100.8 MHz ± 1.2 MHz; after a dwell it is retuned 10 MHz away, and capture runs on for
// 40 s. `/surface.html` at 1280×800, following live, is put over the departed band and then zoomed
// in FOUR levels (frequency span halved each time, time span with it down to a floor that still
// leaves folded rows to sample). At every level, once the pane is resident, every unoccluded screen
// column over the departed band is sampled at rows recorded after the retune and before the
// server's data edge. The blank marks are the product's own, HARVESTED from its legend
// (fog-of-war.e2e.mjs's technique): THE grey (`unobserved`) and the not-loaded ground (`pending`,
// the black the user saw). The claim, at every level: **no sampled pixel over the departed band is
// grey or pending** — fog everywhere. Premise, also per level: the pane really is past the band's
// last row (the ledger's `last_t_s`, from `/api/lastknown`), so the fog is being carried from
// outside the viewport, which is the case that failed.
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
const PORT = Number(process.env.HK_E2E_PORT ?? 8791) + 5;

const FIXTURE_CENTRE_HZ = 100.8e6;
const AWAY_HZ = 10e6;
// The departed band, inset from its rolled-off edges.
const BAND_HZ = [FIXTURE_CENTRE_HZ - 1.1e6, FIXTURE_CENTRE_HZ + 1.1e6];
const DWELL_S = 12;
const AFTER_S = 40;
const SAMPLE_AFTER_RETUNE_S = 3;
const BASE_SPAN_HZ = 2.4e6;
const BASE_SPAN_S = 32;
const MIN_SPAN_S = 6;
const LEVELS = 4;
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
    `/api/tiles?level_f=0&level_t=0&f_index=${Math.floor(FIXTURE_CENTRE_HZ / tileHz)}&t_index=${Math.floor(Date.now() / 1000 / tileS)}&cells=256`);
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

test("T-1058: after a retune, the departed band is fog at every one of four zoom-ins — no black, no grey", { timeout: 540000 }, async (t) => {
  const backend = await startBackend({ port: PORT, mockDevice: true });
  t.after(() => backend.stop());

  // ——— the band's history, then the retune, through the device route ———
  const began = await waitEdge(backend, 0, "the first frame", 60000);
  await waitEdge(backend, began + DWELL_S, `a ${DWELL_S} s dwell`, 90000);
  const armed = await dataEdge(backend);
  const dep = await retune(backend, FIXTURE_CENTRE_HZ + AWAY_HZ);
  t.diagnostic(`retuned ${dep.from.center_hz} Hz -> ${FIXTURE_CENTRE_HZ + AWAY_HZ} Hz; data edge at the retune ${armed.toFixed(3)}`);
  const edgeAfter = await waitEdge(backend, armed + AFTER_S, `${AFTER_S} s after the retune`, 240000);
  t.diagnostic(`data edge ${edgeAfter.toFixed(3)} (${(edgeAfter - armed).toFixed(1)} s after the retune)`);

  // The ledger's own word on the band: every column known, last seen at its last row.
  const lk = await get(backend, `/api/lastknown?f_lo=${BAND_HZ[0]}&f_hi=${BAND_HZ[1]}&cols=64`);
  assert.equal(lk.known, lk.cols, `the ledger knows every column of the departed band: ${JSON.stringify(lk.state)}`);
  const aLast = Math.max(...lk.last_t_s);
  assert.ok(aLast >= armed - 1 && aLast <= armed + SAMPLE_AFTER_RETUNE_S,
    `the band's last row ${aLast} is at the retune (${armed})`);
  t.diagnostic(`ledger: band last seen ${aLast.toFixed(3)}, ${lk.ledger.cells} cells, complete ${lk.complete}`);

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
  const dpr = await page.eval("window.devicePixelRatio || 1");
  const S = (v) => v * dpr;

  const failures = [];
  for (let level = 0; level <= LEVELS; level++) {
    const spanHz = BASE_SPAN_HZ / 2 ** level;
    const spanS = Math.max(MIN_SPAN_S, BASE_SPAN_S / 2 ** level);
    const nav = await gotoFreq(page, rect, FIXTURE_CENTRE_HZ, spanHz);
    await zoomTime(page, rect, spanS);
    const res = await waitResident(page);
    const edge = await dataEdge(backend);
    const img = await page.shot(path.join(ART, `ledger-fog-${level}.png`));
    const r0 = (await panes(page))[0];
    const win = windowOf(r0.where);
    // Premise: the band's last row is below the viewport — the fog comes from outside it.
    assert.ok(r0.t0Ns / 1e9 > aLast,
      `level ${level}: the pane (${(r0.t0Ns / 1e9).toFixed(2)}..${(r0.t1Ns / 1e9).toFixed(2)}) still holds the band's last row ${aLast.toFixed(2)}`);
    const tFrom = Math.max(armed + SAMPLE_AFTER_RETUNE_S, r0.t0Ns / 1e9 + 0.2);
    const tTo = Math.min(edge - 0.3, r0.t1Ns / 1e9 - 0.2);
    assert.ok(tTo > tFrom + 0.5,
      `level ${level}: no sample rows [${tFrom.toFixed(2)}, ${tTo.toFixed(2)}] (pane ${r0.t0Ns / 1e9}..${r0.t1Ns / 1e9}, edge ${edge})`);
    const ys = Array.from({ length: 7 }, (_, i) => yOf(rect, r0, tFrom + ((tTo - tFrom) * (i + 0.5)) / 7));
    const unocc = await page.unoccludedColumns('[data-slot="canvas"]', { y0: Math.min(...ys), y1: Math.max(...ys) });
    const clip = clipToUnoccluded(rect, rect, unocc);
    const lo = Math.max(win.loHz, BAND_HZ[0]), hi = Math.min(win.hiHz, BAND_HZ[1]);
    const xs = [];
    for (let x = Math.ceil(Math.max(xOf(rect, win, lo), clip.x)); x <= Math.floor(Math.min(xOf(rect, win, hi), clip.x + clip.w - 1)); x++) xs.push(x);
    assert.ok(xs.length >= 200, `level ${level}: only ${xs.length} departed columns on screen`);
    let greyPx = 0, pendingPx = 0;
    const bad = [];
    for (const x of xs) {
      for (const y of ys) {
        const c = pixelAt(img, S(x) + dpr / 2, S(y));
        const g = near(c, grey), p = near(c, pending);
        greyPx += g; pendingPx += p;
        if ((g || p) && bad.length < 4) bad.push(`${((win.loHz + ((x - rect.x) / rect.w) * win.spanHz) / 1e6).toFixed(4)} MHz @ ${y.toFixed(0)} ${JSON.stringify(c)}`);
      }
    }
    // What the server answered for the finest tile at the sampled rows.
    const ax = await get(backend, "/api/tiles?level_f=0&level_t=0&f_index=0&t_index=0&cells=256");
    const v = await get(backend,
      `/api/tiles?level_f=0&level_t=0&f_index=${Math.floor(FIXTURE_CENTRE_HZ / ax.axes.frequency.tile_hz)}&t_index=${Math.floor((tFrom + tTo) / 2 / ax.axes.time.tile_s)}&cells=256`);
    t.diagnostic(`level ${level}: ${nav.trail.slice(-1)} × ${((r0.t1Ns - r0.t0Ns) / 1e9).toFixed(1)} s, ${r0.tier} [${res.rows[0].counts}] ` +
      `resident after ${res.ms} ms; ${xs.length} cols × ${ys.length} rows: grey ${greyPx}, pending ${pendingPx}; ` +
      `server finest tile: ledger known ${v.shadow?.ledger?.columns_known}, searched ${v.shadow?.ledger?.columns_searched}, search ran ${v.shadow?.search?.ran}`);
    if (greyPx + pendingPx > 0) failures.push(`level ${level}: grey ${greyPx}, pending ${pendingPx} px, e.g. ${bad.join("; ")}`);
  }
  assert.deepEqual(failures, [],
    "the departed band is fog at every zoom — no pixel of it may be THE grey (never observed) or the not-loaded ground");
});
