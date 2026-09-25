// **T-911: a departed band's shadow keeps the colour of its last live row.**
//
// The user, verbatim: "sweep a band with a known emitter, retune away; the shadow's colour for that
// band must equal the colour of its last live row within one ramp step (e2e: compare the framebuffer
// column before/after departure)." What they saw on staging: tune away from a band you were
// watching, and its shadow did not keep the picture — a different-looking band, not the same one
// dimmed.
//
// ——— THE DEFECT THIS FILE WAS WRITTEN AGAINST (red on the code before T-911) ———
//
// The shadow (ADR-0020, T-519/T-520) is carried down a column from two places: inside the tile that
// holds the band's last live row, from that tile's OWN grid (so it matched); in every tile after it,
// from a search over the SPECTRUM-HISTORY ladder (`hk-api` `shadow_store`), whose finest cell is
// 6.25 kHz x 1 s against the view lattice's 586 Hz x 40 ms. A max-hold over a box ~260x larger is a
// different number: measured over the mock, the departed FM band's noise floor came back 10-15 dB
// hotter (up to 28 dB in single columns) from the first tile boundary after the band was left. So
// the shadow changed colour ~10 s after departure and stayed changed. The fix reads the value first
// at the tile's own level in the tile's own store (`Pyramid::last_known_search_at`), so a carried
// value IS the last live row's cell.
//
// ——— WHAT IS MEASURED, AND HOW ———
//
//  1. **On the wire** (the backend's half, and the precise diagnosis): at the finest lattice node,
//     the departure tile's last observed row per column against the NEXT tile's shadow run for the
//     same column. They must be the same number.
//  2. **In the framebuffer** (the user's claim): `/surface.html` split into two panes over the same
//     75 kHz of band A (half over the FM emitter, half over the noise floor beside it), each zoomed
//     past the finest level in TIME so one 40 ms row is many pixels tall. The left pane is centred
//     on the band's last live row; the right one on a time inside the tile AFTER the departure tile
//     (the tile whose shadow comes from the search, which is where the defect lived). Per frequency
//     cell, the last live row's pixel and the right pane's shadow pixel are each inverted through
//     the product's own ramp — harvested from the page's legend (the `range` swatch: the live ramp;
//     the `shadow` swatch: the same ramp dimmed, scanlined) — so the shadow's documented dimming is
//     undone by the product's own definition of it, never by a constant in this file. The two ramp
//     positions must agree within ONE RAMP STEP, defined as one column of that legend swatch: the
//     finest step the product's own key draws the ramp in.
//
// The dimming itself is kept and checked (the shadow must still NOT look live: its pixels are
// dimmer and scanlined), and grey is untouched: no claim here concerns an unobserved cell.
//
// Retune goes through the MOCK SDR's device interface (`POST /api/control/window`), never files
// into the pipeline (CLAUDE.md). The destination is 433.92 MHz, outside the 2.4 MHz recording,
// served by the mock as its synthesised noise floor — a real capture, somewhere else.
import test from "node:test";
import assert from "node:assert/strict";
import path from "node:path";
import { Browser } from "./harness.mjs";
import { UI_DIR, startBackend } from "./backend.mjs";

const ART = process.env.HK_E2E_ARTIFACTS ?? path.join(UI_DIR, "e2e", "artifacts");
// This file's own backend, at a fixed offset inside its lane's port band (see fog-of-war.e2e.mjs's
// PORT comment for why never a literal port). +24 is used by no other spec.
const PORT = Number(process.env.HK_E2E_PORT ?? 8791) + 24;

const A_HZ = 101.3e6;          // the fixture's FM station (pilot + RDS)
const AWAY_HZ = 433.92e6;       // outside the recording: the mock's synthesised floor
// The view: the station's upper half and the quiet spectrum beyond its deviation, so "over the
// emitter" and "over the noise floor" are both on screen at once.
const VIEW_CENTRE_HZ = 101.375e6, VIEW_SPAN_HZ = 75e3;
const EMITTER_EDGE_HZ = 101.375e6;
// Capture on band A before departing: long enough that the band has a history, not a moment.
const DWELL_S = 12;
// Each pane's time span after zooming: one 40 ms row must be several pixels tall, so the sample
// sits well inside the row and away from its edges.
const TIME_SPAN_TARGET_S = 1.2;
const PANE_GAP_PX = 4; // `PaneModel`'s gapPx default
const MINIMAP_PX = 120; // `/surface.html`'s map strip (preview-main.ts mounts the default)

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// ---------------------------------------------------------------------------
// The server
// ---------------------------------------------------------------------------

/** `GET`, riding out the route's backpressure (`503` is "ask again", never "no" — T-690). */
async function get(backend, p, { tries = 60, waitMs = 200 } = {}) {
  for (let i = 0; ; i++) {
    const r = await fetch(`${backend.origin}${p}`, { headers: { authorization: `Bearer ${backend.token}` } });
    if (r.ok) return r.json();
    assert.ok(r.status === 503 && i < tries, `GET ${p} -> ${r.status}`);
    await sleep(waitMs);
  }
}

/** The one gated device action: retune the mock to `centerHz`, keeping its span. Retries the
 * radio's own `device_busy` (one capture at a time, settle gap — surface-retune.e2e.mjs's rule). */
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
    if (r.ok) return { from: w, at: Date.now() / 1000 };
    assert.ok(r.status === 409 || r.status === 503, `retune -> ${r.status}: ${body}`);
    await sleep(250);
  }
  assert.fail("the mock never accepted the retune");
}

/** Wait until the server has recorded band A for `DWELL_S` seconds, observed. Bounded by the
 * server's own record (its `recording_began_s` and coverage), with a backstop. */
async function waitForDwell(backend, { timeoutMs = 90000 } = {}) {
  const t0 = Date.now();
  for (;;) {
    const q = new URLSearchParams({ f_lo: String(A_HZ - 50e3), f_hi: String(A_HZ + 50e3), cells: "8", rows: "8" });
    const cov = await get(backend, `/api/coverage?${q}`).catch(() => null);
    const began = cov?.horizon?.recording_began_s;
    const observed = (cov?.any?.cells ?? []).filter((c) => c?.state === "observed").length;
    if (typeof began === "number" && Date.now() / 1000 - began >= DWELL_S && observed > 0) {
      return { began, observed, ms: Date.now() - t0 };
    }
    assert.ok(Date.now() - t0 < timeoutMs, `band A was never recorded for ${DWELL_S} s: ${JSON.stringify(cov?.horizon)}`);
    await sleep(500);
  }
}

/** The finest lattice node's geometry, from the server's own answer. */
async function finestAxes(backend) {
  const t = await get(backend, "/api/tiles?level_f=0&level_t=0&f_index=0&t_index=0&cells=256");
  return { tileHz: t.axes.frequency.tile_hz, tileS: t.axes.time.tile_s, cellHz: t.axes.frequency.cell_hz, cellS: t.axes.time.cell_s };
}

const tileAt = (backend, ax, fIndex, tIndex) =>
  get(backend, `/api/tiles?level_f=0&level_t=0&f_index=${fIndex}&t_index=${tIndex}&cells=256`);

/**
 * The server's truth about the departure, at the finest node over band A: the tile holding the
 * band's last observed row, that row's end instant (`lastEndS`) and its value per column — the
 * "last live row". Searches back from the retune instant a few tiles. `null` when not found.
 */
async function lastLiveRow(backend, ax, departS) {
  const fIndex = Math.floor(A_HZ / ax.tileHz);
  for (let ti = Math.floor(departS / ax.tileS) + 1; ti >= Math.floor(departS / ax.tileS) - 3; ti--) {
    const t = await tileAt(backend, ax, fIndex, ti);
    const g = t.grid;
    if (!Array.isArray(g?.max_db)) continue;
    const colA = Math.floor((A_HZ - g.f_lo_hz) / g.f_cell_hz);
    for (let r = g.nt - 1; r >= 0; r--) {
      if (g.max_db[r * g.nf + colA] === null) continue;
      return { fIndex, tIndex: ti, row: r, grid: g, lastEndS: g.t0_s + (r + 1) * g.t_cell_s };
    }
  }
  return null;
}

// ---------------------------------------------------------------------------
// The page
// ---------------------------------------------------------------------------

const PANE_ROWS = `JSON.stringify([...document.querySelectorAll('.hk-surface-viewport[data-viewport="pane"]')].map((v) => ({
  id: v.querySelector('.hk-surface-id')?.textContent ?? '',
  where: v.querySelector('.hk-surface-where')?.textContent ?? '',
  counts: v.querySelector('.hk-surface-counts')?.textContent ?? '',
  following: v.getAttribute('data-following'),
  tier: v.getAttribute('data-tier'),
  t0Ns: Number(v.getAttribute('data-t0-ns')),
  t1Ns: Number(v.getAttribute('data-t1-ns')),
})))`;
const panes = async (page) => JSON.parse(await page.eval(PANE_ROWS));

/** The pane's frequency window, parsed from `.hk-surface-where` (fog-of-war.e2e.mjs's reading). */
function windowOf(where) {
  const m = /^([\d.]+) MHz ± ([\d.]+) (Hz|kHz|MHz|GHz)/.exec(where);
  assert.ok(m, `the pane readout is not a frequency window: ${JSON.stringify(where)}`);
  const mult = { Hz: 1, kHz: 1e3, MHz: 1e6, GHz: 1e9 }[m[3]];
  const centerHz = Number(m[1]) * 1e6, halfHz = Number(m[2]) * mult;
  return { centerHz, halfHz, loHz: centerHz - halfHz, hiHz: centerHz + halfHz, spanHz: 2 * halfHz };
}

/** Pane rectangles in CSS px, from the canvas box: one pane, or two side by side (Split ⇔). */
async function paneRects(page, n) {
  const box = await page.$rect('[data-slot="canvas"]');
  assert.ok(box && box.w > 400 && box.h > 300, `canvas has no box: ${JSON.stringify(box)}`);
  const dpr = await page.eval("window.devicePixelRatio || 1");
  const h = box.h - MINIMAP_PX / dpr;
  if (n === 1) return [{ x: box.x, y: box.y, w: box.w, h }];
  const w = (box.w - PANE_GAP_PX / dpr) / 2;
  return [{ x: box.x, y: box.y, w, h }, { x: box.x + w + PANE_GAP_PX / dpr, y: box.y, w, h }];
}

/** Screen x of `hz` in a pane, and screen y of capture instant `tS` (top = newest). */
const xOf = (rect, win, hz) => rect.x + ((hz - win.loHz) / win.spanHz) * rect.w;
const yOf = (rect, row, tS) => rect.y + ((row.t1Ns / 1e9 - tS) / ((row.t1Ns - row.t0Ns) / 1e9)) * rect.h;

/** Frequency-only zooms and pans onto `centerHz ± spanHz/2`. Each shift-wheel is anchored at the
 * target's own screen x (a zoom keeps the frequency under the cursor), so zooming converges on the
 * target even from a view pinned against the 1 MHz floor, where a pan alone cannot move it. */
async function gotoFreq(page, rect, centerHz, spanHz) {
  const y = rect.y + rect.h / 2;
  const trail = [];
  for (let i = 0; i < 80; i++) {
    const win = windowOf((await panes(page))[0].where);
    trail.push(`${(win.centerHz / 1e6).toFixed(4)}±${(win.halfHz / 1e3).toFixed(1)}k`);
    const off = centerHz - win.centerHz;
    const inside = centerHz > win.loHz + win.spanHz * 0.05 && centerHz < win.hiHz - win.spanHz * 0.05;
    if (inside && win.spanHz > spanHz * 1.02) {
      // One shift-wheel notch: zoomFactor = exp(delta * 0.0015); aim at the ratio still needed.
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
    await page.wheel({ x: rect.x + rect.w / 2, y }, 200, { shift: true }); // too narrow: widen
    await page.frames(2);
  }
  assert.fail(`never reached ${centerHz} ± ${spanHz / 2}: ${trail.join(" -> ")}`);
}

/** Alt-wheel (time only) in pane `i`, anchored at the screen y of `tS`, until its span is at most
 * `targetS`. A frozen pane zooms about the cursor, so `tS` stays where it is (PaneModel.zoomTime). */
async function zoomTimeAround(page, i, tS, targetS) {
  for (let k = 0; k < 40; k++) {
    const row = (await panes(page))[i];
    const spanS = (row.t1Ns - row.t0Ns) / 1e9;
    if (spanS <= targetS) return row;
    const rect = (await paneRects(page, 2))[i];
    const y = yOf(rect, row, tS);
    assert.ok(y > rect.y + 2 && y < rect.y + rect.h - 2,
      `pane ${i}: the instant ${tS.toFixed(3)} left the pane (y ${y.toFixed(1)} in [${rect.y}, ${rect.y + rect.h}]) at span ${spanS.toFixed(2)} s`);
    await page.wheel({ x: rect.x + rect.w / 2, y }, -400, { alt: true });
    await page.frames(2);
  }
  assert.fail(`pane ${i} never zoomed in time to ${targetS} s`);
}

/** Wait until every pane holds its own tiles: tiles in hand, no stand-ins, nothing pending. */
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
    assert.ok(Date.now() - t0 < timeoutMs, `the panes never became resident: ${JSON.stringify(rows.map((r) => r.counts))}`);
    await sleep(300);
  }
}

// ---------------------------------------------------------------------------
// The product's own ramp, harvested from its legend
// ---------------------------------------------------------------------------

async function swatch(page, mark) {
  const json = await page.eval(`(() => {
    const c = document.querySelector('.sp-legend-row[data-mark="${mark}"] canvas');
    if (!c) return null;
    const img = c.getContext('2d').getImageData(0, 0, c.width, c.height);
    return JSON.stringify({ w: c.width, h: c.height, data: Array.from(img.data) });
  })()`);
  assert.ok(json, `no legend swatch for mark="${mark}"`);
  return JSON.parse(json);
}

/**
 * The two ramps, as colour per swatch column (ramp position `c / (w - 1)`):
 *
 * - `live` — the `range` swatch: the ordinary ramp, no mark over it.
 * - `shadow` — the `shadow` swatch with its scanline ink removed: the same ramp through the
 *   shadow's dimming. The ink is the one colour every column shares (fog-of-war.e2e.mjs's
 *   `inkFromSwatch` reasoning: the ground varies with position, the ink does not).
 */
function ramps(rangeImg, shadowImg) {
  const px = (img, x, y) => { const i = (y * img.w + x) * 4; return [img.data[i], img.data[i + 1], img.data[i + 2]]; };
  const key = (c) => (c[0] << 16) | (c[1] << 8) | c[2];
  const cols = (img, x) => { const s = new Map(); for (let y = 0; y < img.h; y++) { const c = px(img, x, y); s.set(key(c), c); } return s; };
  const sets = [0, Math.floor(shadowImg.w / 2), shadowImg.w - 1].map((x) => cols(shadowImg, x));
  const common = [...sets[0].keys()].filter((k) => sets.every((s) => s.has(k)));
  assert.equal(common.length, 1, `expected one ink colour common to the shadow swatch's columns, got ${common.length}`);
  const inkKey = common[0];
  const ink = sets[0].get(inkKey);
  const live = [], shadow = [];
  for (let x = 0; x < rangeImg.w; x++) live.push(px(rangeImg, x, Math.floor(rangeImg.h / 2)));
  for (let x = 0; x < shadowImg.w; x++) {
    const g = [...cols(shadowImg, x).entries()].filter(([k]) => k !== inkKey).map(([, c]) => c);
    // A column whose ground rounds to the ink's own colour has only the ink: interpolate over it.
    shadow.push(g[0] ?? null);
  }
  for (let x = 0; x < shadow.length; x++) {
    if (shadow[x]) continue;
    const a = shadow[x - 1] ?? shadow[x + 1], b = shadow[x + 1] ?? shadow[x - 1];
    shadow[x] = a && b ? a.map((v, j) => (v + b[j]) / 2) : ink;
  }
  assert.equal(live.length, shadow.length, "the range and shadow swatches differ in width");
  return { live, shadow, ink, step: 1 / (live.length - 1) };
}

/** Ramp position of `rgb` on `table` (per-column colours), interpolating 16 sub-steps between
 * columns; the squared RGB distance to the nearest point is returned too, as the fit's quality. */
function invert(table, rgb) {
  const n = table.length - 1;
  let best = { x: 0, d: Infinity };
  for (let c = 0; c < n; c++) {
    for (let s = 0; s <= 16; s++) {
      const f = s / 16, a = table[c], b = table[c + 1];
      const p = [0, 1, 2].map((j) => a[j] + (b[j] - a[j]) * f);
      const d = (p[0] - rgb[0]) ** 2 + (p[1] - rgb[1]) ** 2 + (p[2] - rgb[2]) ** 2;
      if (d < best.d) best = { x: (c + f) / n, d };
    }
  }
  return best;
}

const pixelAt = (img, x, y) => { const i = (Math.round(y) * img.width + Math.round(x)) * 4; return [img.data[i], img.data[i + 1], img.data[i + 2]]; };
const luma = (c) => c[0] + c[1] + c[2];
const near = (a, b, tol = 2) => Math.abs(a[0] - b[0]) <= tol && Math.abs(a[1] - b[1]) <= tol && Math.abs(a[2] - b[2]) <= tol;

/** The brightest pixel in a short vertical run at `x`, skipping `skip` colours. Brightest because
 * the only marks drawn over a measurement (the `spectrum-history` stipple) only ever darken it. */
function brightest(img, x, y0, y1, skip = []) {
  let best = null, n = 0, skipped = 0;
  for (let y = Math.ceil(y0); y <= Math.floor(y1); y++) {
    const c = pixelAt(img, x, y);
    n++;
    if (skip.some((s) => near(c, s))) { skipped++; continue; }
    if (!best || luma(c) > luma(best)) best = c;
  }
  return { rgb: best, n, skipped };
}

// ---------------------------------------------------------------------------

test("T-911: a departed band's shadow keeps its last live row's colour (per column, one ramp step)", { timeout: 540000 }, async (t) => {
  const backend = await startBackend({ port: PORT, mockDevice: true });
  t.after(() => backend.stop());

  // ——— sweep band A (the mock opens on the recording, 100.8 MHz ± 1.2 MHz: the FM station) ———
  const dwell = await waitForDwell(backend);
  t.diagnostic(`band A recorded for ${DWELL_S} s (${dwell.ms} ms wait)`);

  // ——— retune away, through the device interface ———
  const dep = await retune(backend, AWAY_HZ);
  t.diagnostic(`retuned ${dep.from.center_hz} Hz -> ${AWAY_HZ} Hz at ${dep.at.toFixed(3)}`);

  // ——— the server's own account of the last live row, at the finest node ———
  const ax = await finestAxes(backend);
  let last = null;
  for (let i = 0; i < 60 && !last; i++) { last = await lastLiveRow(backend, ax, dep.at); if (!last) await sleep(250); }
  assert.ok(last, "the server holds no observed row over band A around the retune");
  const lastEndS = last.lastEndS;
  // A time inside the tile AFTER the departure tile: its shadow is seeded by the search, which is
  // where the defect lived. 1.5 s into it, clear of its boundary.
  const nextTileT0 = (last.tIndex + 1) * ax.tileS;
  const shadowAtS = nextTileT0 + 1.5;
  t.diagnostic(`last live row ends at ${lastEndS.toFixed(3)} (tile ${last.tIndex}, row ${last.row}); ` +
    `finest tile ${ax.tileS.toFixed(2)} s x ${ax.tileHz.toFixed(0)} Hz; shadow sampled at ${shadowAtS.toFixed(3)}`);

  // Wait (on the server's own data edge) until that instant is folded history.
  let next = null;
  for (let i = 0; ; i++) {
    next = await tileAt(backend, ax, last.fIndex, last.tIndex + 1);
    const edge = next.shadow?.edge_s;
    if (Number.isFinite(edge) && edge > shadowAtS + 1.0 && next.shadow.runs > 0) break;
    assert.ok(i < 240, `the server's data edge never passed ${shadowAtS.toFixed(2)} (edge ${edge})`);
    await sleep(250);
  }

  // ——— 1. THE WIRE: the next tile's carried value == the last live row, per column ———
  {
    const g = last.grid, sh = next.shadow;
    const lastRowOf = (f) => {
      for (let r = g.nt - 1; r >= 0; r--) { const v = g.max_db[r * g.nf + f]; if (v !== null) return v; }
      return null;
    };
    const diffs = [];
    for (let i = 0; i < sh.runs; i++) {
      if (sh.row[i] !== 0 || sh.fill[i] !== 0) continue; // the run carried in from before the tile
      const want = lastRowOf(sh.f[i]);
      if (want === null) continue;
      diffs.push({ f: sh.f[i], carried: sh.last_db[i], last: want, d: sh.last_db[i] - want });
    }
    const worst = diffs.reduce((a, b) => (Math.abs(b.d) > Math.abs(a?.d ?? 0) ? b : a), null);
    const mean = diffs.reduce((s, x) => s + x.d, 0) / Math.max(1, diffs.length);
    t.diagnostic(`wire: ${diffs.length} columns carried into the next tile; mean carried - last live = ${mean.toFixed(2)} dB, ` +
      `worst ${JSON.stringify(worst)}; sources ${JSON.stringify(sh.sources.map((s) => [s.from, s.store ?? null, s.level, s.f_cell_hz ?? null, s.t_cell_s ?? null]))}`);
    assert.ok(diffs.length >= g.nf / 2, `only ${diffs.length} of ${g.nf} columns carried a value into the next tile`);
    const off = diffs.filter((x) => Math.abs(x.d) > 0.01);
    assert.equal(off.length, 0,
      `the shadow carried into the tile after the departure is NOT the band's last live row in ${off.length} of ` +
      `${diffs.length} columns (mean ${mean.toFixed(2)} dB, worst ${JSON.stringify(worst)}): the value was read at ` +
      `a different fold than the row the pane drew`);
  }

  // ——— 2. THE FRAMEBUFFER ———
  const browser = await Browser.open();
  t.after(() => browser.close());
  const page = await browser.page();
  assert.equal(await page.goto(`${backend.origin}/surface.html#token=${backend.token}`), "load");
  await page.waitForSurfaceMounted();
  await page.waitFor("the legend's range and shadow swatches",
    `!!document.querySelector('.sp-legend-row[data-mark="range"] canvas') && !!document.querySelector('.sp-legend-row[data-mark="shadow"] canvas')`,
    { timeoutMs: 20000 });
  const rangeText = (await page.$text('.sp-legend-row[data-mark="range"]')) ?? "";
  const rm = /Display range · (-?[\d.]+) … (-?[\d.]+) dBFS/.exec(rangeText);
  assert.ok(rm, `the key states no display range: ${rangeText}`);
  assert.match(rangeText, /Anchored/, "the page opened in a contrast mode that is not the anchored default");
  const dbSpan = Number(rm[2]) - Number(rm[1]);
  const R = ramps(await swatch(page, "range"), await swatch(page, "shadow"));
  t.diagnostic(`ramp harvested from the legend: ${R.live.length} columns, one step = ${(R.step * 100).toFixed(2)} % of the ramp ` +
    `= ${(R.step * dbSpan).toFixed(2)} dB over ${rm[1]} … ${rm[2]} dBFS; shadow ink rgb(${R.ink.join(",")})`);

  await page.waitFor("the pane readout", `document.querySelectorAll('.hk-surface-viewport[data-viewport="pane"] .hk-surface-where').length > 0`, { timeoutMs: 20000 });
  const [whole] = await paneRects(page, 1);
  const nav = await gotoFreq(page, whole, VIEW_CENTRE_HZ, VIEW_SPAN_HZ);
  t.diagnostic(`view on band A: ${nav.trail.slice(-3).join(" -> ")}`);
  // Freeze the pane (a time drag past the follow hold), so a time zoom anchors at the cursor.
  const mid = { x: whole.x + whole.w / 2, y: whole.y + whole.h / 2 };
  await page.drag(mid, { x: mid.x, y: mid.y + 40 }, 8);
  await page.frames(3);
  // Two panes onto the same box, then each zoomed in time about its own instant.
  await page.click(`[...document.querySelectorAll(".sp-btn")].find((b) => b.textContent.includes("Split ⇔"))`);
  await page.frames(4);
  assert.equal((await panes(page)).length, 2, "Split ⇔ did not give two panes");
  await zoomTimeAround(page, 0, lastEndS - 0.02, TIME_SPAN_TARGET_S);
  await zoomTimeAround(page, 1, shadowAtS, TIME_SPAN_TARGET_S);
  const res = await waitResident(page);
  t.diagnostic(`panes resident after ${res.ms} ms: ${res.rows.map((r) => `${r.id} ${r.tier} [${r.counts}]`).join(" | ")}`);

  const rows = await panes(page);
  const rects = await paneRects(page, 2);
  const wins = rows.map((r) => windowOf(r.where));
  const img = await page.shot(path.join(ART, "shadow-level.png"));
  const dpr = await page.eval("window.devicePixelRatio || 1");
  const S = (v) => v * dpr; // CSS px -> screenshot px

  const pxPerRow = (i) => (ax.cellS / ((rows[i].t1Ns - rows[i].t0Ns) / 1e9)) * rects[i].h;
  assert.ok(pxPerRow(0) >= 8, `a 40 ms row is only ${pxPerRow(0).toFixed(1)} px tall in the left pane: not zoomed in enough to sample one row`);
  // The last live row's centre, and a band inside it clear of both its edges.
  const yLive = yOf(rects[0], rows[0], lastEndS - ax.cellS / 2);
  const halfLive = Math.max(1, pxPerRow(0) / 2 - 2.5);
  const yShadow = yOf(rects[1], rows[1], shadowAtS);
  const cell = ax.cellHz;
  const lo = Math.max(wins[0].loHz, wins[1].loHz), hi = Math.min(wins[0].hiHz, wins[1].hiHz);
  const out = [];
  for (let k = Math.ceil(lo / cell); (k + 1) * cell <= hi; k++) {
    const f = (k + 0.5) * cell;
    const xl = xOf(rects[0], wins[0], f), xr = xOf(rects[1], wins[1], f);
    const live = brightest(img, S(xl), S(yLive - halfLive), S(yLive + halfLive), [R.ink]);
    const shadow = brightest(img, S(xr), S(yShadow - 6), S(yShadow + 6), [R.ink]);
    if (!live.rgb || !shadow.rgb) continue;
    const L = invert(R.live, live.rgb), Sh = invert(R.shadow, shadow.rgb);
    out.push({
      f, region: f < EMITTER_EDGE_HZ ? "emitter" : "floor",
      live: live.rgb, shadow: shadow.rgb, xLive: L.x, xShadow: Sh.x,
      fitLive: L.d, fitShadow: Sh.d, inkInLive: live.skipped, inkInShadow: shadow.skipped,
      steps: (Sh.x - L.x) / R.step,
    });
  }
  const fmt = (o) => `${(o.f / 1e6).toFixed(5)} MHz ${o.region}: live rgb(${o.live}) -> x ${o.xLive.toFixed(3)}, ` +
    `shadow rgb(${o.shadow}) -> x ${o.xShadow.toFixed(3)} (${o.steps >= 0 ? "+" : ""}${o.steps.toFixed(2)} steps)`;
  for (const o of out) t.diagnostic(fmt(o));
  const summary = (sel) => {
    const s = out.filter(sel);
    const abs = s.map((o) => Math.abs(o.steps));
    return { n: s.length, meanSteps: s.reduce((a, o) => a + o.steps, 0) / Math.max(1, s.length), maxAbsSteps: Math.max(0, ...abs) };
  };
  const em = summary((o) => o.region === "emitter"), fl = summary((o) => o.region === "floor");
  t.diagnostic(`framebuffer: emitter ${JSON.stringify(em)}, floor ${JSON.stringify(fl)} (1 step = ${(R.step * dbSpan).toFixed(2)} dB)`);

  // ——— premises: a real measurement on both sides, and the shadow is still a shadow ———
  assert.ok(em.n >= 8 && fl.n >= 8, `too few columns sampled: emitter ${em.n}, floor ${fl.n}`);
  assert.ok(new Set(out.map((o) => o.live.join(","))).size >= 6,
    "the last live row reads as fewer than 6 distinct colours across the band: not a real render");
  assert.ok(out.every((o) => o.inkInLive === 0), "the sampled live row carries the shadow's scanline ink: the time mapping is off");
  assert.ok(out.filter((o) => o.inkInShadow > 0).length >= out.length * 0.9,
    "the right pane's samples carry no scanline ink: they are not the shadow tier");
  // The dimming is kept (T-520): undimmed through the shadow ramp, the shadow is DIMMER on screen.
  assert.ok(out.filter((o) => luma(o.shadow) < luma(o.live) || luma(o.live) < 12).length >= out.length * 0.9,
    "the shadow is not drawn dimmer than the live row it carries — the last-known mark would read as live");

  // ——— THE CLAIM: per column, within one ramp step ———
  const bad = out.filter((o) => Math.abs(o.steps) > 1);
  assert.equal(bad.length, 0,
    `${bad.length} of ${out.length} columns' shadow is more than one ramp step (${(R.step * dbSpan).toFixed(2)} dB) from the ` +
    `band's last live row (emitter mean ${em.meanSteps.toFixed(2)} steps, floor mean ${fl.meanSteps.toFixed(2)}):\n` +
    bad.slice(0, 12).map(fmt).join("\n"));
});
