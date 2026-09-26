// **LSR-7 end to end: the client's arrival→paint latency and ring-fold cost are MEASURED, never
// assumed, on the same live-ring lane LSR-1 proved end to end** (T-1048).
//
// `ui/test/livemetrics.test.ts` proves the arithmetic (a rolling window, a paint-minus-arrival
// subtraction); `ui/test/surface-livering.test.ts` proves the renderer's wiring (one sample per new
// row, `null` with no ring). Neither can prove the numbers are real against a real backend and a
// real render loop — which is this file's one job.
//
// **Review fix (2026-09-25): no gate assertion on a wall-clock BOUND** (docs/10 §3.6, kind 2 — a
// latency assertion belongs in the `timing` tier, never the gate). The first version of this file
// asserted `meanMs < 60000` and "the count must grow within exactly 4 s", both bounds on real
// elapsed time and both load-sensitive on a busy gate box. What is asserted here instead is
// **structural**: the reported numbers are well-formed (finite, non-negative — the correctness bug
// this ticket actually found, a capture-clock/wall-clock mismatch, produced a literal `NaN`-free but
// enormous finite number, so this file still diagnostically PRINTS the values for a human to read
// against the plausible range described below), and that BOTH windows eventually report a sample,
// waited for by whether the page is still WORKING (T-491) rather than by a deadline on the number
// itself. No bound is asserted on how fast the fold or the paint actually was.
import test from "node:test";
import assert from "node:assert/strict";
import { appUrl, Browser, waitWhileWorking } from "./harness.mjs";

const ORIGIN = process.env.HK_E2E_ORIGIN, TOKEN = process.env.HK_E2E_TOKEN;

/** The page's own statement of its LSR-7 measurements. `null` until the first frame writes it. */
const LIVE_METRICS = `JSON.parse(document.querySelector('.sf-stage')?.dataset.liveMetrics ?? 'null')`;

/** The page's own statement of its live lane, per pane (`live-ring.e2e.mjs`'s LIVE_RING). */
const LIVE_RING = `JSON.parse(document.querySelector('.sf-stage')?.dataset.liveRing ?? '[]')`;

/** A window's numbers are well-formed: finite and non-negative. Not a bound on their SIZE — a
 * `NaN` or a negative duration is an arithmetic bug regardless of the machine's load; a large
 * finite number is a fact about the machine, and this function says nothing about it. */
function isWellFormed(w) {
  return Number.isFinite(w.meanMs) && w.meanMs >= 0
    && Number.isFinite(w.p95Ms) && w.p95Ms >= 0
    && Number.isFinite(w.maxMs) && w.maxMs >= 0
    && w.maxMs >= w.meanMs - 1e-6; // p95/max are over the SAME window mean is; max can't be below it
}

test("LSR-7: arrival→paint latency and ring-fold cost are measured and reported — never a guess", async (t) => {
  const browser = await Browser.open();
  t.after(() => browser.close());
  const page = await browser.page();

  const url = appUrl(ORIGIN, TOKEN, { liveRing: true });
  assert.equal(await page.goto(url), "load");
  await page.waitFor("the app's surface to draw",
    `!!document.querySelector('.sf-canvas') && document.querySelector('.sf-canvas').width > 200`,
    { timeoutMs: 60000 });
  await page.waitFor("a pane's scale bar to say it follows the live edge",
    `document.querySelectorAll('.sf-scale:not([hidden])[data-following="true"]').length > 0`, { timeoutMs: 30000 });
  assert.equal(
    await page.$count('.sf-scale:not([hidden])[data-following="true"]'), 1,
    "no pane is following the live edge, so there is no live row to measure the paint of");

  // **Establish the precondition through the product's own zoom, never trust the opening span**
  // (T-1082, the RC red of 2026-09-26; the same premise T-1052 fixed in `live-ring.e2e.mjs`). The
  // paint-latency meter fires only where a ring row is PAINTED (`surface.ts`: one sample per new
  // row the pane's `cover` reaches) — correctly: a row the ring stood aside for was never painted,
  // and a latency for it would be invented. A following pane's opening span comes from the
  // OBSERVED extent, which grows with the backend's age, so after other specs had driven the same
  // lane's backend the page opened wide enough to push `rowPx` under `livering.ts`'s `MIN_ROW_PX`:
  // the ring stood aside, fold kept counting every arriving row (n=256) and latency honestly
  // stayed at n=0. Reproduced alone on a fresh hk by running app-no-minimap + app-status first.
  // So zoom in, bounded, until the following pane's own ring entry says it is painting rows.
  const rect = await page.$rect(".sf-canvas");
  assert.ok(rect && rect.w > 100 && rect.h > 100, `the canvas has no box to zoom on: ${JSON.stringify(rect)}`);
  const point = { x: rect.x + rect.w / 2, y: rect.y + rect.h / 3 };
  const laneRows = (rings) => Array.isArray(rings) ? Math.max(0, ...rings.map((r) => r.rows), 0) : 0;
  const ringFrame = await waitWhileWorking(page, () => page.eval(LIVE_RING),
    (rings) => Array.isArray(rings) && rings.length > 0,
    { everyMs: 300, stallMs: 15000, timeoutMs: 60000 });
  assert.ok(ringFrame.ok, `the ring diagnostic never reported for any pane: ${JSON.stringify(ringFrame.value)}`);
  let ring = ringFrame.value;
  for (let i = 0; i < 40 && laneRows(ring) === 0; i++) {
    await page.wheel(point, -240, { alt: true });
    await page.frames(3);
    ring = await page.eval(LIVE_RING);
  }
  t.diagnostic(`ring lane before metering: ${JSON.stringify(ring)}`);
  assert.ok(laneRows(ring) > 0,
    `zooming in 40 ticks never brought the following pane's rowPx over MIN_ROW_PX: ${JSON.stringify(ring)}`);

  // Both windows report at least one sample. Bounded by whether the page is still WORKING (T-491),
  // not a wall-clock guess: a slow backend under a loaded gate is a slow start, not a dead lane.
  const first = await waitWhileWorking(page, () => page.eval(LIVE_METRICS),
    (m) => !!m && m.latency.n > 0 && m.fold.n > 0,
    { everyMs: 300, stallMs: 15000, timeoutMs: 90000 });
  t.diagnostic(`live metrics after ${first.ms} ms: ${JSON.stringify(first.value)}`);
  assert.ok(first.ok,
    `the dashboard tile never reported a sample of both windows: ${JSON.stringify(first.value)}. ` +
    "Either the ring never painted a row (LSR-1's own lane, not this ticket's) or neither meter fired.");

  assert.ok(isWellFormed(first.value.latency), `latency window is not well-formed: ${JSON.stringify(first.value.latency)}`);
  assert.ok(isWellFormed(first.value.fold), `fold window is not well-formed: ${JSON.stringify(first.value.fold)}`);
  // Diagnostic only, never asserted: a human reading a red run (or this green one) can see at a
  // glance whether the numbers are the low-single-digit-ms shape a healthy machine produces, or the
  // multi-day shape the capture-clock bug this ticket fixed once produced.
  t.diagnostic(`latency: mean ${first.value.latency.meanMs} ms, p95 ${first.value.latency.p95Ms} ms, max ${first.value.latency.maxMs} ms`);
  t.diagnostic(`fold: mean ${first.value.fold.meanMs} ms, p95 ${first.value.fold.p95Ms} ms, max ${first.value.fold.maxMs} ms`);

  // **Moving**: more rows keep arriving as the page keeps running, so the sample counts keep
  // climbing rather than freezing at whatever the first measurement caught — waited for by
  // WORKING, never by a fixed sleep racing the row rate.
  const grew = await waitWhileWorking(page, () => page.eval(LIVE_METRICS),
    (m) => !!m && m.latency.n > first.value.latency.n && m.fold.n > first.value.fold.n,
    { everyMs: 300, stallMs: 15000, timeoutMs: 30000 });
  t.diagnostic(`live metrics after growth-wait (${grew.ms} ms): ${JSON.stringify(grew.value)}`);
  assert.ok(grew.ok,
    `sample counts never advanced past latency.n=${first.value.latency.n}/fold.n=${first.value.fold.n}: ` +
    `${JSON.stringify(grew.value)}`);

  // The dashboard tile's own words state the same numbers, not a second calculation of them.
  const tileText = await page.eval(`document.querySelector('.sf-ring-metrics')?.textContent ?? ''`);
  t.diagnostic(`dashboard tile: "${tileText}"`);
  assert.match(tileText, /^ring fold .+ · arrival→paint .+$/, "the dashboard tile did not state both windows");
  assert.doesNotMatch(tileText, /no samples yet/, "the tile still claims no samples once both windows have some");

  assert.deepEqual(page.exceptions, [], "uncaught exception while LSR-7's meters ran");
});
