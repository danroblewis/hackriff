// **LSR-1 end to end: with `HK_UI_LIVE_RING=1`, a following pane paints its live edge from
// `spectrum/live`** (T-1042), on the mock SDR, in a real browser.
//
// The milestone this file is the only tier that can report on: *"behind the flag, LSR-1 alone shows a
// following pane painting from spectrum/live end-to-end (rows at the edge, tiles below)."* Everything
// under it — where a row lands, which tile the rows make unnecessary, what a gap does — is measured
// exactly in `ui/test/livering.test.ts` and `ui/test/surface-livering.test.ts`, on the fetch path and
// on rasterised pixels. What only this tier can say is that the rows a **real backend** publishes over
// a **real socket** reach the screen through that lane.
//
// So it asserts three things, in this order:
//
//  1. **The lane ran.** The page states what each pane's live lane did this frame
//     (`.sf-stage[data-live-ring]`, written from the `PaneReport` the data pass produced): rows
//     painted from the ring, tile addresses those rows excluded, and one row's height in px. Rows > 0
//     is the end-to-end claim — the socket delivered, the ring filed, the renderer drew.
//  2. **The picture is real, and stays real.** The newest strip of the following pane is a genuine
//     render in four of five samples over sixteen seconds of capture — the same measurement
//     `live-edge.e2e.mjs` makes of the tile lane, made of this one, so a green here means the same
//     thing there. A dead lane produces a flat fill; a lane that draws one lucky frame does not pass
//     five samples.
//  3. **Nothing below the rows is blank.** The whole pane's data rectangle is a real render, which is
//     the honest form of "tiles below": whether the pyramid answers the lower half (the usual shape)
//     or the ring's own rows reach the floor of the window (a short window on a fast stream), the
//     pane is painted all the way down and the flag has not left a hole.
//
// What it deliberately does **not** do:
//
//  - **It does not assert `ringTiles > 0`.** The exclusion is only true of a tile whose visible strip
//    is wholly inside the ring's band, and the pane's opening window is the backend's — a pane wider
//    than the tuned band legitimately needs its tiles. That claim is asserted where it is exact: on
//    the cache's request list, in the unit tier. Here it is a diagnostic.
//  - **It does not assert a readout string.** Following is read off `data-following`, which is a fact
//    (T-478's rule: a following pane's offset from the edge drifts under load with no gesture).
import test from "node:test";
import assert from "node:assert/strict";
import path from "node:path";
import { Browser, appUrl, census, waitWhileWorking } from "./harness.mjs";
import { UI_DIR } from "./backend.mjs";

const ORIGIN = process.env.HK_E2E_ORIGIN, TOKEN = process.env.HK_E2E_TOKEN;
const ART = process.env.HK_E2E_ARTIFACTS ?? path.join(UI_DIR, "e2e", "artifacts");

/** The spectrum-trace strip carved off the TOP of each pane (`TRACE_PX`, T-457). */
const TRACE_PX = 96;
/** The newest rows, minus the very edge: the top few per cent are sampled-but-not-yet-drawn. */
const FRESH_FROM = 0.06, FRESH_TO = 0.30;

/** Is this strip a real render rather than a flat fill? `live-edge.e2e.mjs`'s own rule. */
const isRender = (c) => c.distinct >= 32 && c.dominantShare < 0.9;

/** The rectangle a pane draws its MEASUREMENT into, in page coordinates (the trace strip is taken
 * out of the pane's rectangle, not painted over it — T-457). */
function paneRectOf(rect, dpr, ins = { top: 0, bottom: 0 }) {
  const paneH = (rect.h - ins.top - ins.bottom) * dpr;
  const traceH = Math.max(0, Math.min(TRACE_PX, Math.floor(paneH / 3)));
  return { x: rect.x, w: rect.w, y: rect.y + ins.top + traceH / dpr, h: (paneH - traceH) / dpr };
}

/** The page's own statement of its live lane, per pane. `[]` until the first frame writes it. */
const LIVE_RING = `JSON.parse(document.querySelector('.sf-stage')?.dataset.liveRing ?? '[]')`;

test("LSR-1: with the live ring on, a following pane paints its live edge from spectrum/live", async (t) => {
  const browser = await Browser.open();
  t.after(() => browser.close());
  const page = await browser.page();

  const url = appUrl(ORIGIN, TOKEN, { liveRing: true });
  assert.match(url, /\?live-ring=1#token=/, "the spec did not ask for the flag it is about");
  assert.equal(await page.goto(url), "load");
  await page.waitFor("the app's surface to draw",
    `!!document.querySelector('.sf-canvas') && document.querySelector('.sf-canvas').width > 200`,
    { timeoutMs: 60000 });
  await page.waitFor("the chrome to report a viewport",
    `document.querySelectorAll('.hk-surface-viewport[data-viewport="pane"]').length > 0`, { timeoutMs: 30000 });
  assert.equal(
    await page.$count('.hk-surface-viewport[data-viewport="pane"][data-following="true"]'), 1,
    "no pane is following the live edge, so there is no live view to test");

  // 1. **The lane ran.** Bounded by whether the page is still WORKING rather than by a wall-clock
  //    guess (the T-491/2026-09-22 rule): a backend whose first rows are slow under a loaded gate is
  //    a slow start, not a dead lane, and a page that has stopped asking reports faster than a
  //    deadline would.
  const lane = await waitWhileWorking(page, () => page.eval(LIVE_RING),
    (rings) => Array.isArray(rings) && rings.some((r) => r.rows > 0),
    { everyMs: 300, stallMs: 15000, timeoutMs: 90000 });
  t.diagnostic(`live-ring lane after ${lane.ms} ms: ${JSON.stringify(lane.value)}`);
  assert.ok(lane.ok,
    "no pane painted a single row from the live ring. The flag is on (the URL above), so this is the " +
    "lane itself: either `/ws/spectrum/live` delivered no row with a usable geometry, or the ring " +
    "reported no frame (it needs two rows to measure the cadence), or the pane is zoomed out past " +
    `MIN_ROW_PX and the pyramid is answering instead. The page's own statement: ${JSON.stringify(lane.value)}`);

  const counts = await page.eval(`(() => { const v = document.querySelector('.hk-surface-viewport[data-viewport="pane"]');
    return v ? (v.querySelector('.hk-surface-counts')?.textContent ?? '') : ''; })()`);
  t.diagnostic(`the following pane's residency report: ${counts}`);

  // 2. **The picture is real, and stays real.**
  const { rect, ms } = await page.waitForCanvas(".sf-canvas", isRender, { timeoutMs: 90000 });
  const dpr = await page.eval("window.devicePixelRatio || 1");
  const pane = paneRectOf(rect, dpr, await page.canvasInsets());
  const strip = {
    x: Math.round(pane.x), w: Math.round(pane.w),
    y: Math.round(pane.y + pane.h * FRESH_FROM), h: Math.round(pane.h * (FRESH_TO - FRESH_FROM)),
  };
  t.diagnostic(`first fill after ${ms} ms; pane data rect ${JSON.stringify(pane)}; fresh strip ${JSON.stringify(strip)}`);
  assert.ok(strip.h > 8, `the fresh strip is ${strip.h} px; the window is too small to measure`);

  const samples = [];
  for (let i = 0; i < 5; i++) {
    await new Promise((r) => setTimeout(r, i === 0 ? 6000 : 2500));
    const img = await page.shot(i === 4 ? path.join(ART, "live-ring.png") : null);
    samples.push({
      at: 6 + i * 2.5,
      fresh: census(img, strip),
      whole: census(img, { x: Math.round(pane.x), y: Math.round(pane.y), w: Math.round(pane.w), h: Math.round(pane.h) }),
      ring: await page.eval(LIVE_RING),
    });
  }
  for (const s of samples) {
    t.diagnostic(`t+${s.at}s  newest rows: ${s.fresh.distinct} distinct, dominant ` +
      `${(s.fresh.dominantShare * 100).toFixed(0)} % — ${isRender(s.fresh) ? "drawn" : "FLAT"}` +
      `; lane ${JSON.stringify(s.ring)}`);
  }

  assert.ok(isRender(samples[0].fresh),
    `the newest rows of a following pane were a FLAT fill 6 s after the surface first drew ` +
    `(${samples[0].fresh.distinct} distinct, dominant ${(samples[0].fresh.dominantShare * 100).toFixed(0)} %) ` +
    "with the live ring on. The later samples say whether it recovers; this one says whether it started.");
  const drawn = samples.filter((s) => isRender(s.fresh)).length;
  assert.ok(drawn >= 4,
    `the newest rows of a following pane were a real render in only ${drawn} of 5 samples over ` +
    "16 s of capture, with the live ring painting them.\n  " +
    samples.map((s) => `t+${s.at}s ${s.fresh.distinct} distinct / dominant ${(s.fresh.dominantShare * 100).toFixed(0)} %`).join("\n  "));

  // The lane kept running across the whole run, not only at the instant it was first seen.
  const rows = samples.map((s) => (s.ring.find((r) => r.rows > 0)?.rows ?? 0));
  assert.ok(rows.every((n) => n > 0),
    `the live ring stopped painting mid-run: rows per sample ${JSON.stringify(rows)}`);

  // 3. **Nothing below the rows is blank.**
  const last = samples[samples.length - 1];
  assert.ok(isRender(last.whole),
    `the pane's whole data rectangle was a flat fill (${last.whole.distinct} distinct, dominant ` +
    `${(last.whole.dominantShare * 100).toFixed(0)} %): the ring painted its rows and left the ` +
    "history below them empty, which is the one thing the exclusion may never do.");

  assert.deepEqual(page.exceptions, [], "uncaught exception while the live ring ran");
});
