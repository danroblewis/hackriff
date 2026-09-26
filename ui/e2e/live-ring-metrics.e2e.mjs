// **LSR-7 end to end: sample→pixel latency and per-row fold cost are MEASURED, never assumed, on the
// same live-ring lane LSR-1 proved end to end** (T-1048).
//
// `ui/test/livemetrics.test.ts` proves the arithmetic (a rolling window, a paint-minus-arrival
// subtraction); `ui/test/surface-livering.test.ts` proves the renderer's wiring (one sample per new
// row, `null` with no ring). Neither can prove the numbers are **sane against a real backend and a
// real render loop** — which is exactly what this ticket is for: the first version of this
// measurement, run against this very fixture, reported a **billion-millisecond** latency, because it
// compared the row's own backend capture time against `Date.now()` and the capture clock a replay
// runs on is not the wall clock (`livemetrics.ts`'s header tells that story). So this file's one job
// is the sanity bound a unit test cannot give: **small, finite, and moving**, on the mock SDR, live.
import test from "node:test";
import assert from "node:assert/strict";
import { appUrl, Browser, waitWhileWorking } from "./harness.mjs";

const ORIGIN = process.env.HK_E2E_ORIGIN, TOKEN = process.env.HK_E2E_TOKEN;

/** The page's own statement of its LSR-7 measurements. `null` until the first frame writes it. */
const LIVE_METRICS = `JSON.parse(document.querySelector('.sf-stage')?.dataset.liveMetrics ?? 'null')`;

test("LSR-7: sample→pixel latency and per-row fold cost are measured, small, and moving — never a guess", async (t) => {
  const browser = await Browser.open();
  t.after(() => browser.close());
  const page = await browser.page();

  const url = appUrl(ORIGIN, TOKEN, { liveRing: true });
  assert.equal(await page.goto(url), "load");
  await page.waitFor("the app's surface to draw",
    `!!document.querySelector('.sf-canvas') && document.querySelector('.sf-canvas').width > 200`,
    { timeoutMs: 60000 });

  // Both windows report at least one sample. Bounded by whether the page is still WORKING (T-491),
  // not a wall-clock guess: a slow backend under a loaded gate is a slow start, not a dead lane.
  const first = await waitWhileWorking(page, () => page.eval(LIVE_METRICS),
    (m) => !!m && m.latency.n > 0 && m.fold.n > 0,
    { everyMs: 300, stallMs: 15000, timeoutMs: 90000 });
  t.diagnostic(`live metrics after ${first.ms} ms: ${JSON.stringify(first.value)}`);
  assert.ok(first.ok,
    `the dashboard tile never reported a sample of both windows: ${JSON.stringify(first.value)}. ` +
    "Either the ring never painted a row (LSR-1's own lane, not this ticket's) or neither meter fired.");

  // **The sanity bound this ticket exists to hold.** A row travels this stream's socket, the ring's
  // fold and one render pass in low single-digit milliseconds on a healthy machine; a page open for
  // well under a minute cannot honestly have waited an hour for one, let alone the 3.5 DAYS a capture
  // clock mismatch produced the one time this measurement was wrong. 60 000 ms is generous on either
  // side precisely so this assertion is about the CLOCK being right, not about a load-flake's tenth
  // of a second.
  const SANE_MS = 60000;
  assert.ok(first.value.latency.meanMs >= 0 && first.value.latency.meanMs < SANE_MS,
    `latency.meanMs was ${first.value.latency.meanMs} ms — outside [0, ${SANE_MS}) ms means the two ` +
    "clocks this subtraction reads are not the same clock (the capture-time bug this ticket fixed).");
  assert.ok(first.value.latency.maxMs >= 0 && first.value.latency.maxMs < SANE_MS,
    `latency.maxMs was ${first.value.latency.maxMs} ms`);
  assert.ok(first.value.fold.meanMs >= 0 && first.value.fold.meanMs < SANE_MS,
    `fold.meanMs was ${first.value.fold.meanMs} ms`);

  // **Moving**: more rows arrive as the page keeps running, so the sample counts must keep climbing
  // rather than freezing at whatever the first measurement caught — the "measured, never assumed"
  // claim would be empty if the count could plausibly be one stale sample repeated forever.
  await new Promise((r) => setTimeout(r, 4000));
  const later = await page.eval(LIVE_METRICS);
  t.diagnostic(`live metrics 4 s later: ${JSON.stringify(later)}`);
  assert.ok(later.latency.n > first.value.latency.n,
    `latency.n did not advance (${first.value.latency.n} -> ${later.latency.n}) over 4 s of a live page`);
  assert.ok(later.fold.n > first.value.fold.n,
    `fold.n did not advance (${first.value.fold.n} -> ${later.fold.n}) over 4 s of a live page`);

  // The dashboard tile's own words state the same numbers, not a second calculation of them.
  const tileText = await page.eval(`document.querySelector('.sf-ring-metrics')?.textContent ?? ''`);
  t.diagnostic(`dashboard tile: "${tileText}"`);
  assert.match(tileText, /^fold .+ · latency .+$/, "the dashboard tile did not state both windows");
  assert.doesNotMatch(tileText, /no samples yet/, "the tile still claims no samples once both windows have some");

  assert.deepEqual(page.exceptions, [], "uncaught exception while LSR-7's meters ran");
});
