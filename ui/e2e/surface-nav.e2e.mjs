// **T-454's guard**: panning and zooming the surface must not exceed the tile route's own in-flight
// cap, and backpressure must stay a *probe* rather than becoming a *regime* (T-455).
//
// The defect this file exists for: `/api/tiles` answers `503` over `cost.in_flight_limit`
// concurrent reads, naming the cap. The client already had a cap, an `AbortController` per request
// and measured cancellation — and the `503` still reached the user. So the facts under test are
// facts about **the wire and the screen**, not about the client's own bookkeeping: concurrency is
// counted from CDP's `Network` events, so a client whose internal counter is wrong cannot certify
// itself, and the cap it is compared against is read from the server's own `cost.in_flight_limit`.
//
// ——— WHY THE BOUND IS NOT ZERO, AND WHY IT IS NOT A NUMBER THAT HAPPENS TO PASS ———
//
// The first version of this file asserted **zero** `503`s. After T-454 landed, that failed on 2 of
// ~1 850 requests, and "relax it to 2" would have been exactly the move this repo refuses. So the
// residual was measured instead. T-454's fix is AIMD: a `503` sets the ceiling and halves the
// operating cap, which then recovers one slot per 8 successes. **An AIMD controller discovers its
// share by being refused** — the server's cap is a single global budget shared by this page's two
// viewports, its own abandoned reads, the bootstrap probe and any other tab, so no participant can
// be *told* its share. The competing reading was that the residual is leakage: a client `abort()`
// failing to release the server's slot, which is what T-454's abandoned-slot accounting addresses.
//
// Eleven runs against the fix distinguish them — it is discovery:
//
//   * peak concurrency on the wire is **exactly the server's cap, never above** (4/4), where before
//     the fix it was 6/4. The over-subscription is gone.
//   * every refusal occurs with the operating cap **at its ceiling** — never at 2 or 3, which is
//     where leakage would also show. The cap trace is the same every run: 4 → 2 on the first
//     refusal, +1, +1, back to 4.
//   * after the last viewport change, the cap recovers **to the ceiling and stays there** through
//     ~3 700 further tile requests over 25 s with **zero** refusals. A leak keeps leaking;
//     discovery, once the share is found and demand is steady, stops.
//
// So forbidding the residual would forbid the mechanism that keeps the user from ever seeing one.
//
// ——— AND WHY THE REPLACEMENT BOUND IS NOT A COUNT ———
//
// The first attempt at the replacement was a count with a story attached: "AIMD cannot return to
// its ceiling more often than the viewport changes that push it off, so ≤ 1 refusal per gesture".
// The selftest killed it. A client with the halving **deleted** — refused, notices, does nothing —
// never leaves the ceiling, is refused 8-10 times across the same nine gestures, and passes that
// bound *and* the steady-state bound below. The story was reasoned from how the correct algorithm
// behaves, and a bound reasoned that way does not constrain the incorrect one.
//
// So the permission is conditioned on the mechanism instead of counted: refusals are allowed
// **because** they are how AIMD finds its share, and assertion (3) requires the halving that makes
// that true to be **observed** in the client's own operating cap. Take the halving away and the
// permission evaporates. The count survives only as a coarse guard against the pre-fix regime
// (27-48 refusals), which is safe to leave loose precisely because (3) is sharp.
//
// And the gestures have to be real gestures, or the whole thing is vacuous in the other direction:
// each one asserts the **view actually moved**, by reading the per-viewport chrome readout the page
// draws (centre, span, level). A drag that hit nothing would leave those identical.
import test from "node:test";
import assert from "node:assert/strict";
import path from "node:path";
import { Browser, census } from "./harness.mjs";
import { UI_DIR } from "./backend.mjs";

const ORIGIN = process.env.HK_E2E_ORIGIN, TOKEN = process.env.HK_E2E_TOKEN;
const ART = process.env.HK_E2E_ARTIFACTS ?? path.join(UI_DIR, "e2e", "artifacts");

/**
 * How long the page is left rendering with NOTHING moving the view, before the "converged" bound is
 * judged. Sized from the measured AIMD recovery: after the last gesture the operating cap climbs
 * back to its ceiling at 12–14 s from load, so this window has to be long enough to CONTAIN that
 * return — a shorter one would judge a client that never got back up to the cap, and prove nothing.
 */
const STEADY_STATE_MS = 8000;

/** The pane readouts the page draws: `[{ id, where, level, counts }]`. */
const READOUT = `[...document.querySelectorAll('.hk-surface-viewport')].map((v) => ({
  id: v.querySelector('.hk-surface-id')?.textContent ?? '',
  where: v.querySelector('.hk-surface-where')?.textContent ?? '',
  level: v.querySelector('.hk-surface-level')?.textContent ?? '',
}))`;

const STATUS = `(document.querySelector('[data-slot="status"]')?.textContent ?? '')`;

/** The page's own action buttons, found by the label the user reads. */
const BUTTON = (label) =>
  `[...document.querySelectorAll('.sp-btn')].find((b) => b.textContent.trim() === ${JSON.stringify(label)})`;

test("panning and zooming stays inside the tile route's in-flight cap, with no refusal reaching the user", async (t) => {
  // Read by the runner from the server's own `cost.in_flight_limit` before any browser existed —
  // see the note in run.mjs. Not a constant here: restating the cap would make the test agree with
  // a copy of the number rather than with the server that enforces it.
  const limit = Number(process.env.HK_E2E_TILE_LIMIT);
  assert.ok(Number.isInteger(limit) && limit > 0,
    `the server did not declare cost.in_flight_limit, so there is no cap to test against (got "${process.env.HK_E2E_TILE_LIMIT}")`);
  t.diagnostic(`server cap: cost.in_flight_limit = ${limit}`);

  const browser = await Browser.open();
  t.after(() => browser.close());
  const page = await browser.page();
  const tiles = page.watchConcurrency("tiles", (u) => u.includes("/api/tiles"));

  const loadedAt = Date.now();
  await page.goto(`${ORIGIN}/surface.html#token=${TOKEN}`);
  await page.waitForSurfaceMounted();
  await page.waitFor("the first tile textures to be uploaded",
    `(${STATUS}.match(/(\\d+) uploads/)?.[1] | 0) > 0`, { timeoutMs: 90000 });

  const rect = await page.$rect('[data-slot="canvas"]');
  const mid = { x: rect.x + rect.w / 2, y: rect.y + rect.h * 0.35 };
  const before = await page.eval(`JSON.stringify(${READOUT})`);

  // A pan, then zoom on each axis, then a pan of the whole-surface map at the bottom. Between
  // them the page is given real frames, because the fetch storm this is about is scheduled from
  // the render loop.
  // The client's OPERATING cap, sampled from the status line it renders. This is the one number
  // that separates a controller from a client that merely notices refusals: AIMD halves on a `503`,
  // so after any refusal the operating cap must be seen BELOW its ceiling. A client that never
  // backs off sits at the ceiling and is refused once per viewport change — which passes a count
  // bound and passes a steady-state bound, and is caught by nothing else here. (Measured: the
  // `t454-never-back-off` fault produces 8 refusals with the cap pinned at 4, against 2 with it
  // falling to 2.)
  const caps = [];
  const sampleCap = async () => {
    const m = (await page.eval(STATUS)).match(/(\d+)\/(\d+) in flight/);
    if (m) caps.push(Number(m[2]));
  };

  const moved = [], clamped = [], visited = [];
  const step = async (what, fn, { mustMove = true } = {}) => {
    const was = await page.eval(`JSON.stringify(${READOUT})`);
    await fn();
    await page.frames(6);
    await new Promise((r) => setTimeout(r, 450));
    await sampleCap();
    await new Promise((r) => setTimeout(r, 450));
    await sampleCap();
    const now = await page.eval(`JSON.stringify(${READOUT})`);
    if (mustMove) assert.notEqual(now, was, `${what} did not move the view — the gesture missed, so it tested nothing`);
    (now === was ? clamped : moved).push(what);
    visited.push(now);
  };

  // The sequence takes the view out to the whole surface and wheels back in. "Whole surface" is one
  // click and a ten-level jump in frequency — the worst tile storm this page can produce, and the
  // shape T-454 lives in: a viewport change that invalidates the whole working set and demands a
  // new one at once.
  //
  // **The TIME wheel is exercised but not required to move the view**, and that is a statement
  // about this fixture rather than a softened assertion. The surface's time extent is the record,
  // which here is seconds long, so the pane sits at the lattice's finest time level (`level_t` 0)
  // with the whole record already on screen: there is no finer level to zoom into and nothing
  // beyond the record to zoom out to, so a correct client clamps in both directions. Demanding
  // movement would fail on correct code; ignoring the axis would leave its requests untested. So
  // the wheels are delivered, their requests counted with all the others, and which axes actually
  // moved is reported. Frequency, which has ten levels of room here, carries the strict assertion.
  await step("drag-pan", () => page.drag(mid, { x: mid.x - 260, y: mid.y + 120 }, 12));
  await step("jump to the whole surface", () => page.click(BUTTON("Whole surface")));
  await step("wheel zoom in (time)", async () => { for (let i = 0; i < 5; i++) await page.wheel(mid, -240); }, { mustMove: false });
  await step("shift+wheel zoom in (frequency)", async () => { for (let i = 0; i < 5; i++) await page.wheel(mid, -240, { shift: true }); });
  await step("shift+wheel zoom out (frequency)", async () => { for (let i = 0; i < 3; i++) await page.wheel(mid, 240, { shift: true }); });
  await step("wheel zoom out (time)", async () => { for (let i = 0; i < 5; i++) await page.wheel(mid, 240); }, { mustMove: false });
  // The map along the bottom is a viewport too, and it opens showing the whole surface — so
  // *dragging* it is clamped for the same reason the time wheel is. Double-clicking it is not:
  // that is the page's discrete "send the active pane there", which moves the pane across the
  // spectrum in one step and invalidates its whole working set.
  const mapAt = (fx) => ({ x: rect.x + rect.w * fx, y: rect.y + rect.h - 18 });
  await step("drag the whole-surface map", () => page.drag(mapAt(0.5), { x: rect.x + rect.w * 0.5 + 180, y: rect.y + rect.h - 18 }, 10),
    { mustMove: false });
  await step("double-click the map to send the pane there", () => page.dblclick(mapAt(0.22)));
  await step("fit back to coverage", () => page.click(BUTTON("Fit to coverage")));
  t.diagnostic(`moved the view: ${moved.join(", ")}`);
  t.diagnostic(`clamped (no movement, expected on this fixture): ${clamped.join(", ") || "none"}`);

  // Let anything still queued drain, so a refusal from the last gesture has every chance to arrive
  // before the steady-state window opens.
  await page.frames(10);
  await new Promise((r) => setTimeout(r, 1500));
  const navigationEnded = Date.now();
  const navigationRefusals = page.requests.filter((r) => r.status === 503).length;

  // ——— STEADY STATE: the page keeps rendering, nothing moves the view ———
  // This window is the discriminator the bound rests on. The AIMD cap recovers to its ceiling
  // during it (measured at 12.2–13.8 s from load, comfortably inside), so the client spends most of
  // it running at the full server cap — exactly the condition under which a leaked server slot
  // would refuse it. Measured across runs: zero refusals here, through thousands of further tile
  // requests. A regression that reintroduces leakage shows up here first and unambiguously,
  // because nothing in this window is contending for the budget except the client itself.
  for (let i = 0; i < STEADY_STATE_MS / 500; i++) {
    await new Promise((r) => setTimeout(r, 500));
    await sampleCap();
  }

  // ——— the evidence, gathered and PRINTED before anything is asserted ———
  // A failing guard whose first assertion hides the rest of the picture is a guard people bisect by
  // hand. Everything below is reported, then judged.
  const tileReqs = page.requests.filter((r) => r.url.includes("/api/tiles"));
  const refused = page.requests.filter((r) => r.status === 503);
  const steadyRefusals = refused.filter((r) => r.startedMs >= navigationEnded);
  const steadyRequests = tileReqs.filter((r) => r.startedMs >= navigationEnded).length;
  const status = await page.eval(STATUS);
  const backpressure = Number(status.match(/(\d+) backpressure/)?.[1] ?? -1);
  const cancelled = Number(status.match(/(\d+) cancelled/)?.[1] ?? 0);
  const canceledOnWire = page.requests.filter((r) => r.error === "canceled").length;
  const gestures = moved.length + clamped.length;
  t.diagnostic(`peak ${tiles.peak}/${limit} in flight · ${tileReqs.length} tile requests · ` +
    `${cancelled} cancelled (client), ${canceledOnWire} aborted on the wire`);
  t.diagnostic(`operating cap over the run: ${caps.join(" ")} (server ceiling ${limit})`);
  t.diagnostic(`503s: ${navigationRefusals} during ${gestures} viewport changes, ` +
    `${steadyRefusals.length} during ${(STEADY_STATE_MS / 1000).toFixed(0)} s of steady state ` +
    `(${steadyRequests} requests) · ${backpressure} counted by the client`);
  if (refused.length) {
    t.diagnostic(`refusals at: ${refused.map((r) => `${r.startedMs - loadedAt}ms`).join(" ")} after load`);
  }

  // ——— what the product promises, in three parts ———

  // (1) NEVER EXCEED THE SERVER'S CAP. Exact, no tolerance: this is the half of T-454 that was a
  // straightforward bug (6 in flight against a cap of 4), and the fix's abandoned-slot accounting
  // is what holds it. `tiles.peak` is a **lower bound** on what the client had outstanding, since
  // Chrome opens at most six HTTP/1.1 connections per origin — an asymmetry in the sound direction
  // (a measured excess is real, and the cap is below the connection limit so a compliant client
  // still passes), but the number must never be read as "the client had exactly this many".
  assert.ok(tiles.peak <= limit,
    `${tiles.peak} tile requests were in flight at once against a declared cap of ${limit} ` +
    "(and that is a lower bound — the browser's own 6-connection limit hides anything beyond it)");
  assert.ok(tiles.peak > 1, `only ${tiles.peak} tile request was ever in flight — the cap was never approached, so this proves nothing`);

  // (2) ONCE CONVERGED, NEVER REFUSED AGAIN. This is the strict half, and it is stricter than the
  // "zero refusals" it replaces is about anything that matters: it forbids the regime the user
  // suffers (refusals arriving indefinitely) while permitting the probe that prevents it. Measured
  // zero over 25 s and ~3 700 requests on the fixed client; before the fix, refusals continued
  // throughout.
  assert.equal(steadyRefusals.length, 0,
    `the tile route refused ${steadyRefusals.length} of ${steadyRequests} tile requests during ` +
    `${(STEADY_STATE_MS / 1000).toFixed(0)} s in which NOTHING moved the view. After the AIMD ` +
    "controller has found its share, backpressure must stop: a refusal here is not discovery, it " +
    "is a slot that was never released — the leak T-454's abandoned-slot accounting exists to close.");

  // (3) A REFUSAL MUST ACTUALLY BACK THE CLIENT OFF. This is the assertion that separates a
  // controller from a client that merely counts refusals, and it is the sharp one: the permitted
  // refusals are permitted *because* they are how AIMD finds its share, so the permission is void
  // unless the halving that makes them a search — rather than a standing condition — is observed.
  //
  // It is also the assertion the first draft of this bound lacked, and the selftest proved it: with
  // the halving deleted the client was refused 8 times instead of 2, and BOTH a count bound and the
  // steady-state bound below still passed. A bound reasoned from how the correct algorithm behaves
  // does not constrain the incorrect one; this one is the algorithm's own contract.
  const minCap = caps.length ? Math.min(...caps) : null;
  if (refused.length > 0) {
    assert.ok(minCap !== null && minCap < limit,
      `the route refused ${refused.length} request(s), but the client's operating cap never fell ` +
      `below the server's ${limit} (samples: ${[...new Set(caps)].sort().join(",")}). A 503 must ` +
      "halve it. A client that is refused and does not back off has not discovered anything — it " +
      "is simply being refused, once per viewport change, for as long as the user keeps navigating.");
  }

  // (4) AND THE REFUSALS STAY A SEARCH, NOT A REGIME. A coarse bound against the pre-T-454 shape
  // (27-48 refusals across the same nine gestures), stated per viewport change because that is what
  // creates the contention a refusal reports. It is deliberately loose — (3) is what makes it safe
  // to be loose — and it must not be raised to fit a measurement: if it trips, something is asking
  // for slots it has not got.
  assert.ok(navigationRefusals <= gestures,
    `${navigationRefusals} refusals across ${gestures} viewport changes — backpressure has become ` +
    "the normal condition rather than a search for the share. Do not raise this bound to fit a " +
    "measurement; find what is asking for slots it has not got.");

  // (5) AND NONE OF IT REACHES THE USER AS A FAILURE. `busyRefusals` is a diagnostic counter in the
  // status line, not an error, so it is cross-checked against the wire rather than forced to zero —
  // the inversion of T-454's lesson, that every mechanism counted the CLIENT while making a claim
  // about the SERVER. Here the client's count must agree with what the server actually sent.
  assert.equal(backpressure, refused.length,
    `the client counted ${backpressure} refusals but the wire carried ${refused.length} — its ` +
    "backpressure bookkeeping disagrees with the server that did the refusing");
  assert.equal(await page.$count(".sp-fail"), 0, `a failure card appeared during navigation: ${await page.$text(".sp-fail")}`);
  assert.deepEqual(page.exceptions, [], "uncaught exception during navigation");

  // Cancellation is the thing that makes a cap a latency control rather than a queue: a fast pan
  // must abandon tiles for viewports the user has left. Measured from the client's own counter
  // here, because "abandoned" is a decision only it can report — but it is corroborated by the
  // wire, where aborted requests appear as canceled loads.
  assert.ok(cancelled > 0, "no tile was ever cancelled across the viewport changes — viewport cancellation is not running");

  // And it is still drawing after all that: the same histogram claim as the load test, plus the
  // requirement that the picture actually changed. A renderer that froze on the first frame would
  // pass every counter above.
  const imgAfter = await page.shot(path.join(ART, "surface-nav.png"));
  const box = { x: Math.round(rect.x), y: Math.round(rect.y), w: Math.round(rect.w), h: Math.round(rect.h) };
  const ca = census(imgAfter, box);
  assert.ok(ca.distinct >= 32 && ca.dominantShare < 0.92,
    `after navigating, the canvas is a flat fill: ${ca.distinct} colours, dominant ${ca.dominant} at ${(ca.dominantShare * 100).toFixed(1)} %`);

  // The view must have *been* somewhere else, which is not the same as *ending* somewhere else:
  // the sequence deliberately finishes on "Fit to coverage", whose whole job is to put the pane
  // back where it opened. Comparing the final state to the initial one would therefore fail on a
  // correct client — so the claim is made over the states the view actually passed through.
  assert.ok(visited.some((v) => v !== before),
    "the view never left its opening box across nine gestures, so nothing above exercised a viewport change");
  const ended = await page.eval(`JSON.stringify(${READOUT})`);
  t.diagnostic(ended === before
    ? "the view ended where it started, as \"Fit to coverage\" intends"
    : "the view ended somewhere other than its opening box");
});
