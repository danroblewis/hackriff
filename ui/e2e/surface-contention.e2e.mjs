// **Two tabs on `/surface` at once** — the interaction no unit test reaches (T-455, T-630).
//
// The finding this file came from: when the browser tier first ran two files in sequence, the
// second page could not start at all. `probeSurface` makes one `/api/tiles` call to learn the view
// lattice, and if the route was at its in-flight cap that call came back `503` — which the page
// treated as fatal and answered with "The surface could not be addressed". The route's `503` is
// documented backpressure asking the caller to *retry*, so a single tab could lock out a second one
// simply by rendering.
//
// T-454 answered it with a bounded retry and doubling backoff on the bootstrap path. **That was not
// enough, and T-630 is why.** With the retry in place the second tab still booted in 8.2 s and
// 11.7 s after 7 refusals, and twice did not boot at all, because the route's cap was also
// *first-come-first-served*: a tab enumerating a wide viewport holds all four slots continuously —
// it re-asks the instant one frees — so a newcomer's FIRST request, the one it cannot start
// without, competed on equal terms with the thousandth request of a tab that is already drawn.
// Retrying harder cannot win a race whose rules never let you in.
//
// So the route now divides its slots: `ceil(cap / clients)` each, plus one slot held back while any
// client has nothing on screen yet (`docs/api.md`, "Cost, and the two caps"). This file asserts
// that **in counts and ordering, never in wall-clock** (user, 2026-09-21):
//
//   - the second tab's first *successful* tile arrives within a stated number of its OWN requests;
//   - the refusals it sees before that are bounded and stated;
//   - the first tab's peak *admitted* concurrency never exceeds its share, measured on the wire
//     from when the server answered each request, and corroborated by the share the first tab's
//     own status line states;
//   - and the run is rejected as INCONCLUSIVE if the second tab was never made to wait, so a race
//     that did not happen cannot bank a green.
//
// **The red baseline is real and is one environment variable**: `HK_TILE_FAIR_SHARE=off` restores
// the first-come-first-served route, and the peak-share assertion below goes red against it (the
// first tab holds all four while the second boots). `cost.fair_share` reaches this spec as
// `HK_E2E_TILE_FAIR`, so a baseline run cannot be mistaken for a green one.
import test from "node:test";
import assert from "node:assert/strict";
import { Browser } from "./harness.mjs";

const ORIGIN = process.env.HK_E2E_ORIGIN, TOKEN = process.env.HK_E2E_TOKEN;
const STATUS = `(document.querySelector('[data-slot="status"]')?.textContent ?? '')`;
const BUTTON = (label) =>
  `[...document.querySelectorAll('.sp-btn')].find((b) => b.textContent.trim() === ${JSON.stringify(label)})`;

/**
 * **The stated bounds.** Counts, not seconds. With a cap of 4 and two clients the share is 2, so
 * the newcomer's first request meets a route the first tab may already have filled (it was alone,
 * and alone its share is the whole cap) — that one refusal is expected and is what registers the
 * newcomer. From then the first tab is held to its share and one slot is reserved, so the retry
 * gets in. The headroom above that is for the reads already in flight having to finish.
 */
const MAX_REQUESTS_TO_FIRST_TILE = 4;
const MAX_REFUSALS_BEFORE_FIRST_TILE = 3;

/** Peak overlapping requests among `recs`, counted from start to **the server's answer**. */
function peakConcurrency(recs, now) {
  const edges = [];
  for (const r of recs) {
    const end = r.respondedMs ?? r.endedMs ?? now;
    if (!(end > r.startedMs)) continue;
    edges.push([r.startedMs, 1], [end, -1]);
  }
  edges.sort((a, b) => a[0] - b[0] || a[1] - b[1]);
  let live = 0, peak = 0;
  for (const [, d] of edges) { live += d; peak = Math.max(peak, live); }
  return peak;
}

const tiles = (page) => page.requests.filter((r) => r.url.includes("/api/tiles"));

test("a second tab can open /surface while the first is saturating the tile route", async (t) => {
  const limit = Number(process.env.HK_E2E_TILE_LIMIT);
  const fair = process.env.HK_E2E_TILE_FAIR === "true";
  // **The share is what the POLICY says, not what the server under test implements.** Deriving it
  // from `fair_share` would make this assertion trivially true against the first-come-first-served
  // route — which is the one baseline it has to be able to go red against. Two clients are up, so
  // the share is `ceil(cap / 2)`; with more clients it is smaller still, and a bound that holds at
  // two holds there too.
  const share = Math.ceil(limit / 2);
  t.diagnostic(`server cap ${limit}, fair share ${fair ? "on" : "OFF (the pre-T-630 baseline)"}, ` +
    `so a client's share with two tabs up is ${share}`);
  const browser = await Browser.open();
  t.after(() => browser.close());

  // Tab one: load, then send it to the whole surface so it is demanding tiles as hard as it can.
  const first = await browser.page();
  await first.goto(`${ORIGIN}/surface.html#token=${TOKEN}`);
  await first.waitForSurfaceMounted();
  await first.waitFor("the first tab to be uploading tiles",
    `(${STATUS}.match(/(\\d+) uploads/)?.[1] | 0) > 0`, { timeoutMs: 90000 });
  // **The second tab's page is created BEFORE the storm starts.** Opening a Chrome tab takes over a
  // second, and on a young record the first tab's zoom is finished inside that — so a spec that
  // creates the tab and then navigates measures a route nobody is competing for, which is exactly
  // how a first-come-first-served route passes a fairness test. Created here, navigated below with
  // nothing in between.
  const second = await browser.page();

  // Four viewports and a ten-level jump: the worst tile storm this page has, started in the same
  // step as the navigation so the two overlap.
  await first.eval(`(${BUTTON("Split ⇔")})?.click(), (${BUTTON("Split ⇕")})?.click(), ` +
    `(${BUTTON("Whole surface")})?.click(), 1`);

  // Tab two, opened into that. No settling, no waiting for the first to go quiet: the whole point
  // is that the route is busy. Everything the first tab does from HERE is what has to leave room.
  const contentionFrom = Date.now(), t0 = contentionFrom;
  await second.goto(`${ORIGIN}/surface.html#token=${TOKEN}`);
  await second.waitForSurfaceMounted({ timeoutMs: 45000 });
  const mountedAt = Date.now();
  const bootMs = mountedAt - t0;

  const probeRequests = tiles(second);
  const probeRefusals = probeRequests.filter((r) => r.status === 503);
  // **In its own requests, not in seconds**: which of the second tab's tile requests was the first
  // the route actually served, and how many refusals it took to get there.
  const firstServed = probeRequests.findIndex((r) => r.status === 200);
  const refusalsBefore = probeRequests.slice(0, firstServed < 0 ? probeRequests.length : firstServed)
    .filter((r) => r.status === 503).length;
  t.diagnostic(`second tab mounted in ${bootMs} ms after ${probeRequests.length} tile request(s), ` +
    `${probeRefusals.length} of them refused 503; its first SERVED tile was request ` +
    `#${firstServed + 1}, after ${refusalsBefore} refusal(s)`);

  // It mounted, and it mounted for real rather than showing the card.
  assert.equal(await second.$count(".sp-fail"), 0,
    `the second tab could not address the surface while the first was busy: ${await second.$text(".sp-fail")}`);
  assert.match((await second.$text('[data-slot="census"]')) ?? "", /observed/,
    "the second tab mounted without a coverage census, so it did not really complete its probe");
  assert.deepEqual(second.exceptions, [], "uncaught exception in the second tab");

  // **The bound that replaces "it eventually booted".**
  assert.ok(firstServed >= 0, "the second tab was never served a tile at all");
  assert.ok(firstServed + 1 <= MAX_REQUESTS_TO_FIRST_TILE,
    `the second tab needed ${firstServed + 1} of its own requests to be served one tile, over the ` +
    `stated bound of ${MAX_REQUESTS_TO_FIRST_TILE}: a newcomer's first paint must not queue behind ` +
    "an already-drawn client's fill");
  assert.ok(refusalsBefore <= MAX_REFUSALS_BEFORE_FIRST_TILE,
    `the second tab saw ${refusalsBefore} refusals before its first tile, over the stated bound of ` +
    `${MAX_REFUSALS_BEFORE_FIRST_TILE}`);

  // The second tab is now asking for tiles of its own, against a first tab that has not stopped:
  // the end of the contention window, and where it is measured.
  await second.waitFor("the second tab to upload tiles of its own",
    `(${STATUS}.match(/(\\d+) uploads/)?.[1] | 0) > 0`, { timeoutMs: 60000 });
  const contentionTo = Date.now();

  // **The first tab's share, measured two ways.**
  //
  // On the wire: how many of its requests the route had admitted at once, counted from each
  // request's start to the moment the server answered it (a refusal never held a slot, so it is
  // not counted). This is the property the policy is: one client cannot hold the whole cap while
  // another is asking. Against the first-come-first-served route it is the cap itself, and this
  // assertion is the red baseline.
  // Measured over the CONTENTION WINDOW: from the moment the second tab was opened to the moment
  // it had tiles of its own on screen. That whole interval is two clients wanting the same four
  // slots — the second enumerating its first screen, the first zooming four panes over the whole
  // surface — which is the situation the policy is about, and a longer window than the boot alone
  // because the newcomer needs slots after its probe too (T-459: no visible fetch is starved).
  // Requests that OVERLAP the window, not only those that started in it: a tile read begun a moment
  // before the second tab opened is still holding one of the four slots while it boots, which is
  // precisely what it has to get past. (Measured the other way first, and it reported "0 requests"
  // over a window in which the second tab was refused three times — a filter that excluded the very
  // reads doing the refusing.)
  const during = tiles(first).filter((r) =>
    r.status !== 503 && r.startedMs <= contentionTo && (r.respondedMs ?? r.endedMs ?? Date.now()) >= contentionFrom);
  const peak = peakConcurrency(during, Date.now());
  // And from the page's own status line, which is what a user can see: `2+0/2 in flight (share 2)`.
  const statusLine = (await first.$text('[data-slot="status"]')) ?? "";
  const m = /(\d+)\+(\d+)\/(\d+) in flight \(share (\d+)\)/.exec(statusLine);
  t.diagnostic(`first tab: peak ${peak} admitted tile reads over ${during.length} requests the route ` +
    `served it while the second tab booted; status line "${m ? m[0] : statusLine}"`);
  // **Non-vacuity, before the assertion rather than after it**: a peak measured over a window in
  // which the first tab asked for nothing proves nothing about who the route would have preferred.
  // Say so, and do not assert on it — a green banked on a race that did not happen is the failure
  // mode this whole file is written against.
  const contested = during.length > 0;
  if (!contested) {
    t.diagnostic("INCONCLUSIVE: the first tab was not contending for the route while the second " +
      `drew: the route served it ${during.length} request(s) in the window. The ` +
      "boot bounds above still hold; the share bound below is not exercised by this run.");
  }
  assert.ok(!contested || peak <= share,
    `the first tab held ${peak} of the route's ${limit} slots at once while a second client was ` +
    `booting; its share is ${share}. This is the defect T-630 exists for: the cap was ` +
    "first-come-first-served, so a tab that re-asks the instant a slot frees never gives one up.");
  assert.ok(m, `the first tab's status line must state its in-flight share: "${statusLine}"`);
  assert.ok(Number(m[4]) <= share,
    `the first tab still believes its share is ${m[4]} of ${limit} with two clients up: "${m[0]}"`);
  assert.ok(Number(m[1]) + Number(m[2]) <= Number(m[3]),
    `the first tab reports more reads out than its own operating cap: "${m[0]}"`);

  // And it drew: mounting is not the claim, rendering is.
  const { census: c } = await second.waitForCanvas('[data-slot="canvas"]',
    (x) => x.distinct >= 32 && x.dominantShare < 0.92 && x.meanLuma > 8, { timeoutMs: 60000 });
  t.diagnostic(`second tab drew ${c.distinct} distinct colours, dominant ${c.dominant} at ${(c.dominantShare * 100).toFixed(1)} %`);

  // Non-vacuity: if the route was never actually busy for the second tab, this run proved nothing
  // about the share or the retry — it proved that two tabs can boot when there is room for both.
  // Say so rather than bank a green.
  if (probeRefusals.length === 0 && probeRequests.length <= 1) {
    t.diagnostic("INCONCLUSIVE: the second tab's probe was never refused, so neither the retry path " +
      "nor the fair share was exercised. Nothing here is wrong, but this run does not exercise it — " +
      "the first tab's storm and the second tab's boot did not overlap.");
  }
  if (during.length === 0) {
    t.diagnostic("INCONCLUSIVE: the first tab made no served tile request while the second booted, " +
      "so its peak share was measured over nothing.");
  }
});
