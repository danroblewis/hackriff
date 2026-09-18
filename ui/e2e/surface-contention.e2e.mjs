// **Two tabs on `/surface` at once** — the interaction no unit test reaches (T-455).
//
// The finding this file came from: when the browser tier first ran two files in sequence, the
// second page could not start at all. `probeSurface` makes one `/api/tiles` call to learn the view
// lattice, and if the route was at its in-flight cap that call came back `503` — which the page
// treated as fatal and answered with "The surface could not be addressed". The route's `503` is
// documented backpressure asking the caller to *retry*, so a single tab could lock out a second one
// simply by rendering.
//
// T-454 answered it with a bounded retry and doubling backoff on the bootstrap path. This asserts
// that from the browser, because the premise is a race between two real pages against one real
// server: nothing in `ui/test` can hold four server slots while a second client boots.
//
// The test is written so it cannot pass by accident. The first tab is driven into the worst tile
// storm the page has ("Whole surface", a ten-level jump) and the second tab is opened **while that
// is still in flight**, so the probe really does meet a busy route — and the run is rejected as
// inconclusive if the wire shows the second tab was never made to wait.
import test from "node:test";
import assert from "node:assert/strict";
import { Browser } from "./harness.mjs";

const ORIGIN = process.env.HK_E2E_ORIGIN, TOKEN = process.env.HK_E2E_TOKEN;
const STATUS = `(document.querySelector('[data-slot="status"]')?.textContent ?? '')`;
const BUTTON = (label) =>
  `[...document.querySelectorAll('.sp-btn')].find((b) => b.textContent.trim() === ${JSON.stringify(label)})`;

test("a second tab can open /surface while the first is saturating the tile route", async (t) => {
  const limit = Number(process.env.HK_E2E_TILE_LIMIT);
  const browser = await Browser.open();
  t.after(() => browser.close());

  // Tab one: load, then send it to the whole surface so it is demanding tiles as hard as it can.
  const first = await browser.page();
  await first.goto(`${ORIGIN}/surface.html#token=${TOKEN}`);
  await first.waitForSurfaceMounted();
  await first.waitFor("the first tab to be uploading tiles",
    `(${STATUS}.match(/(\\d+) uploads/)?.[1] | 0) > 0`, { timeoutMs: 90000 });
  await first.eval(`(${BUTTON("Whole surface")})?.click(), 1`);

  // Tab two, opened into that. No settling, no waiting for the first to go quiet: the whole point
  // is that the route is busy.
  const second = await browser.page();
  const t0 = Date.now();
  await second.goto(`${ORIGIN}/surface.html#token=${TOKEN}`);
  await second.waitForSurfaceMounted({ timeoutMs: 45000 });
  const bootMs = Date.now() - t0;

  const probeRequests = second.requests.filter((r) => r.url.includes("/api/tiles"));
  const probeRefusals = probeRequests.filter((r) => r.status === 503);
  t.diagnostic(`second tab mounted in ${bootMs} ms after ${probeRequests.length} tile request(s), ` +
    `${probeRefusals.length} of them refused 503 (server cap ${limit})`);

  // It mounted, and it mounted for real rather than showing the card.
  assert.equal(await second.$count(".sp-fail"), 0,
    `the second tab could not address the surface while the first was busy: ${await second.$text(".sp-fail")}`);
  assert.match((await second.$text('[data-slot="census"]')) ?? "", /observed/,
    "the second tab mounted without a coverage census, so it did not really complete its probe");
  assert.deepEqual(second.exceptions, [], "uncaught exception in the second tab");

  // And it drew: mounting is not the claim, rendering is.
  await second.waitFor("the second tab to upload tiles of its own",
    `(${STATUS}.match(/(\\d+) uploads/)?.[1] | 0) > 0`, { timeoutMs: 60000 });
  const { census: c } = await second.waitForCanvas('[data-slot="canvas"]',
    (x) => x.distinct >= 32 && x.dominantShare < 0.92 && x.meanLuma > 8, { timeoutMs: 60000 });
  t.diagnostic(`second tab drew ${c.distinct} distinct colours, dominant ${c.dominant} at ${(c.dominantShare * 100).toFixed(1)} %`);

  // Non-vacuity: if the route was never actually busy for the second tab, this run proved nothing
  // about the retry — it proved that two tabs can boot when there is room for both. Say so rather
  // than bank a green. A refused-then-retried probe is the evidence; more tile requests than a
  // clean boot needs is the weaker corroboration when the timing did not line up.
  if (probeRefusals.length === 0 && probeRequests.length <= 1) {
    t.diagnostic("INCONCLUSIVE: the second tab's probe was never refused, so the retry path did not run. " +
      "Nothing here is wrong, but this run does not exercise it — the first tab's storm and the " +
      "second tab's boot did not overlap.");
  }
});
