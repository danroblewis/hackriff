// **Does the live view add rows for recent samples?** (T-460, and T-479 beside it.)
//
// The user's words: *"the live view still does not add waterfall rows for recent samples."* The
// backend was measured innocent — the live-edge tile grows, 33 024 → 43 520 observed cells over 40 s,
// and builds from partial data in ~145 ms. The client had no refresh at all: `TileCache.acquire`
// answered a resident tile unconditionally and the only path that could drop one was the retune, so
// a following pane redrew the same tile for the 256 s it took to scroll into a new address.
//
// **This file is the only tier that can say the defect is fixed, and it has to assert the right
// thing.** The failure this repo keeps hitting is a sound proof of an adjacent claim — T-454's
// counter measured a pump rather than a cancellation; T-448's gate guaranteed every counter except
// the one asserted. Here the adjacent claim is *"the tile was re-fetched"*, which is not the user's
// complaint: a request is not a row on screen. So the assertion below is over **pixels in the
// newest part of a following pane**, and the request counts appear only as diagnostics.
//
// Two things it deliberately does not do:
//
//  - **It does not assert a readout string.** T-478: a following pane's chrome ends in its offset
//    from the live edge, which drifts with wall-clock lag under load with no gesture touching it, so
//    an `equal` goes red on load and a `notEqual` goes green on drift alone. Following is read off
//    the `data-following` attribute the chrome sets, which is a fact and not a measurement.
//  - **It does not pass on one lucky frame.** "Keeps up" is a claim about a run of samples, so the
//    strip is measured five times across twenty seconds of capture and the verdict is over all five.
//    A single `waitForCanvas` would be satisfied by the first fill, which the DEFECT also produces.
import test from "node:test";
import assert from "node:assert/strict";
import path from "node:path";
import { Browser, census } from "./harness.mjs";
import { UI_DIR } from "./backend.mjs";

const ORIGIN = process.env.HK_E2E_ORIGIN, TOKEN = process.env.HK_E2E_TOKEN;
const ART = process.env.HK_E2E_ARTIFACTS ?? path.join(UI_DIR, "e2e", "artifacts");
// The canvas is one drawing buffer with three tenants, and this test measures the middle one. Both
// numbers are `app/centre/surface.ts`'s own, in device px; the arithmetic below is `view.ts`'s.
/** The minimap strip along the BOTTOM of the canvas (`MINIMAP_PX`). */
const MINIMAP_PX = 110;
/** The spectrum-trace strip carved off the TOP of each pane (`TRACE_PX`, T-457). */
const TRACE_PX = 96;

/**
 * The rectangle a pane draws its MEASUREMENT into, in page coordinates.
 *
 * Not the canvas: the trace strip is taken out of the pane's rectangle rather than painted over it
 * (T-457), so the pane's data starts below it. Getting this wrong would be the T-457 composition
 * failure again — a decoration changing the geometry another ticket's assertions were measured in —
 * except here it would make the test *pass* on the trace's own colours, which is worse than red.
 */
function paneRectOf(rect, dpr) {
  const paneH = rect.h * dpr - MINIMAP_PX;
  const traceH = Math.max(0, Math.min(TRACE_PX, Math.floor(paneH / 3)));
  return { x: rect.x, w: rect.w, y: rect.y + traceH / dpr, h: (paneH - traceH) / dpr };
}

/**
 * The band of the pane this test is about: the **newest rows**, minus the very edge.
 *
 * The top few per cent are legitimately not a measurement — the newest second or so has been
 * sampled but not yet folded, and the surface says so rather than colouring it (T-441). Asserting
 * over it would make this test a race against ingest. Everything below it is rows that were recorded
 * seconds ago: if those are not on screen, the live edge is not keeping up, which is the complaint.
 */
const FRESH_FROM = 0.06, FRESH_TO = 0.30;

/** Is this strip a real render rather than a flat fill? The same shape of claim as `app-surface`. */
const isRender = (c) => c.distinct >= 32 && c.dominantShare < 0.9;

test("a FOLLOWING pane keeps drawing rows as they are recorded", async (t) => {
  const browser = await Browser.open();
  t.after(() => browser.close());
  const page = await browser.page();
  const tiles = page.watchConcurrency("tiles", (u) => u.includes("/api/tiles"));

  assert.equal(await page.goto(`${ORIGIN}/#token=${TOKEN}`), "load");
  await page.waitFor("the app's surface to draw",
    `!!document.querySelector('.sf-canvas') && document.querySelector('.sf-canvas').width > 200`,
    { timeoutMs: 60000 });

  // The subject has to be a pane that is following the growing edge; a frozen one is a view over
  // data that cannot change, and this test would then be asserting nothing.
  await page.waitFor("the chrome to report a viewport",
    `document.querySelectorAll('.hk-surface-viewport[data-viewport="pane"]').length > 0`, { timeoutMs: 30000 });
  assert.equal(
    await page.$count('.hk-surface-viewport[data-viewport="pane"][data-following="true"]'), 1,
    "no pane is following the live edge, so there is no live view to test");

  // Wait for the first fill. THE DEFECT PRODUCES THIS TOO — it is the starting line, not the claim.
  const { rect, ms } = await page.waitForCanvas(".sf-canvas", isRender, { timeoutMs: 90000 });
  t.diagnostic(`first fill after ${ms} ms; canvas ${rect.w}x${rect.h}`);
  const dpr = await page.eval("window.devicePixelRatio || 1");
  const pane = paneRectOf(rect, dpr);
  const strip = {
    x: Math.round(pane.x), w: Math.round(pane.w),
    y: Math.round(pane.y + pane.h * FRESH_FROM), h: Math.round(pane.h * (FRESH_TO - FRESH_FROM)),
  };
  t.diagnostic(`pane data rect ${JSON.stringify(pane)}; fresh strip ${JSON.stringify(strip)}`);
  assert.ok(strip.h > 8, `the fresh strip is ${strip.h} px; the window is too small to measure`);

  // Five samples over twenty further seconds of capture. Under the defect the newest rows scroll off
  // the top and are replaced by nothing, so every sample after the first few seconds is a flat fill.
  const samples = [];
  for (let i = 0; i < 5; i++) {
    await new Promise((r) => setTimeout(r, i === 0 ? 8000 : 4000));
    const img = await page.shot(i === 4 ? path.join(ART, "live-edge.png") : null);
    const c = census(img, strip);
    samples.push({ at: 8 + i * 4, c, ok: isRender(c) });
  }
  for (const s of samples) {
    t.diagnostic(`t+${s.at}s  newest rows: ${s.c.distinct} distinct, dominant ${s.c.dominant} at ` +
      `${(s.c.dominantShare * 100).toFixed(0)} % — ${s.ok ? "drawn" : "FLAT"}`);
  }
  const drawn = samples.filter((s) => s.ok).length;
  assert.ok(drawn >= 4,
    `the newest rows of a following pane were a real render in only ${drawn} of 5 samples over 20 s ` +
    "of capture. This is T-460: the rows were recorded and served, and the client stopped asking.\n  " +
    samples.map((s) => `t+${s.at}s ${s.c.distinct} distinct / dominant ${(s.c.dominantShare * 100).toFixed(0)} %`).join("\n  "));

  // Diagnostics, NOT the claim: a tile being re-fetched is not a row appearing on screen.
  const asked = page.requests.filter((r) => r.url.includes("/api/tiles"));
  const repeats = new Map();
  for (const r of asked) repeats.set(r.url, (repeats.get(r.url) ?? 0) + 1);
  const top = [...repeats.values()].sort((a, b) => b - a)[0] ?? 0;
  t.diagnostic(`${asked.length} tile requests, the most-asked address ${top} times, peak ${tiles.peak} in flight`);
  assert.ok(tiles.peak <= Number(process.env.HK_E2E_TILE_LIMIT || 4),
    `peak ${tiles.peak} tile requests in flight, above the route's cap — the refresh lane took slots it may not`);
  assert.deepEqual(page.exceptions, [], "uncaught exception while the live view ran");
});

test("a refused place is asked for ONCE: a 4xx is terminal, and the console stays quiet", async (t) => {
  // T-479. The user watched the console flood: the canvas asked for an out-of-range node, the route
  // answered 400, and the client asked again on the very next frame — forever. The address
  // arithmetic that produces the bad node is T-480's; this is the claim that a permanent refusal is
  // never re-asked, which is worth holding even once nothing asks for a bad address again.
  const browser = await Browser.open();
  t.after(() => browser.close());
  const page = await browser.page();

  assert.equal(await page.goto(`${ORIGIN}/#token=${TOKEN}`), "load");
  await page.waitFor("the app's surface to draw",
    `!!document.querySelector('.sf-canvas') && document.querySelector('.sf-canvas').width > 200`,
    { timeoutMs: 60000 });
  // Long enough that a frame-rate retry would be in the thousands rather than in the ones.
  await new Promise((r) => setTimeout(r, 15000));

  const refused = page.requests.filter((r) => r.url.includes("/api/tiles") && r.status !== null && r.status >= 400 && r.status !== 503);
  const per = new Map();
  for (const r of refused) per.set(r.url, (per.get(r.url) ?? 0) + 1);
  t.diagnostic(`${refused.length} permanent refusals over ${per.size} distinct place(s)`);
  for (const [url, n] of per) {
    assert.equal(n, 1, `${url} was refused ${n} times — asking again cannot change the answer, ` +
      "so a non-503 status must be terminal (T-479)");
  }
  // And the retryable one still is: a 503 is T-454's backpressure, a statement about *now*.
  const busy = page.requests.filter((r) => r.status === 503).length;
  t.diagnostic(`${busy} backpressure refusals (503), which stay retryable`);
  assert.deepEqual(page.exceptions, [], "uncaught exception while the surface ran");
});
