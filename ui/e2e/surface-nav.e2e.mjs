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
//     **That third measurement no longer describes this fixture and is kept only as the history of
//     why the bound is what it is:** re-measured in 2026-09, the page asks for NOTHING once the view
//     stops moving, so the cap does not recover either (it rises only on a completion) and the
//     steady-state set is empty. See the steady-state block below, which now asks the route itself.
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
  // **The two UNIFORM wheels are `mustMove: false`, and T-472 is why.**
  //
  // This note used to read: "a plain wheel moves the view on this fixture whatever the time axis
  // does — the frequency axis has ten levels of room here and carries the movement." T-472 deleted
  // that property deliberately. A plain wheel now stops when EITHER axis reaches a bound, because a
  // gesture in which frequency carries on alone is exactly the aspect-ratio drift the user reported.
  // This file runs first, against a young record: the surface's time extent is whatever has been
  // ingested so far and a pane may not magnify below `minCells` level-0 cells (16 × 1 s here), so
  // the time axis is often pinned in both directions before the first wheel — and a uniform wheel is
  // then correctly a no-op, for the same reason the map drag below is.
  //
  // This is a **narrower premise, not a softer assertion**: the wheels are still delivered and their
  // requests still counted with the rest of the storm, the step still records whether it moved (it
  // is printed either way), and every claim about which axes a wheel moves is made in the T-456 and
  // T-472 tests below, each of which establishes its own premise first rather than assuming one.
  await step("drag-pan", () => page.drag(mid, { x: mid.x - 260, y: mid.y + 120 }, 12));
  await step("jump to the whole surface", () => page.click(BUTTON("Whole surface")));
  await step("wheel zoom in (uniform)", async () => { for (let i = 0; i < 5; i++) await page.wheel(mid, -240); },
    { mustMove: false });
  await step("shift+wheel zoom in (frequency)", async () => { for (let i = 0; i < 5; i++) await page.wheel(mid, -240, { shift: true }); });
  await step("shift+wheel zoom out (frequency)", async () => { for (let i = 0; i < 3; i++) await page.wheel(mid, 240, { shift: true }); });
  await step("wheel zoom out (uniform)", async () => { for (let i = 0; i < 5; i++) await page.wheel(mid, 240); },
    { mustMove: false });
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
  //
  // **What this window actually contains, measured rather than assumed.** The note here used to say
  // the AIMD cap recovers to its ceiling during it (12.2–13.8 s from load) and that the client keeps
  // asking for thousands of tiles, so a leaked server slot would refuse it. Neither half is true on
  // this fixture. Re-measured over six full-suite runs after T-471's ring prefetch was removed:
  //
  //     tile requests by the page during the 8 s window: 0, every run.
  //     the operating cap: pinned at whatever the last refusal left it (1 or 2), every run.
  //
  // Both for the same reason. The view is not moving, nothing is falling off the live edge fast
  // enough to miss, so the client wants nothing — and the cap only rises in `succeeded()`, which
  // needs a completion, so a client that asks for nothing recovers nothing. **`steadyRefusals` was
  // therefore judging an empty set: 0 of 0.** That is how T-471's prefetch ring got in — its defect
  // was visible here precisely BECAUSE it kept asking during this window, and a client that keeps
  // quiet passes the same assertion without being tested by it.
  //
  // So the window asks for itself, twice over, and the two halves do different jobs.
  //
  // **The probe (2b): the route's budget, asked from outside the client.** Once per second, from
  // THIS process rather than the page, one tile is requested — serially, never more than one
  // outstanding, over the same address `tileCost` uses at startup, so it is known-servable and
  // cheap. `/api/tiles` takes its slot before it does any work, so a budget permanently short of
  // slots refuses this, and the answer comes from outside the client whose own bookkeeping cannot
  // be the witness to it (this file's opening rule). **Measured honestly: this is a floor, not the
  // detector.** Run against T-471's ring restored, all eight probes were answered `200` while the
  // page was being refused 1-2 times in the same window — the leak's slots are held only while the
  // server finishes producing an abandoned tile, so a serial probe usually lands between them. What
  // it does guarantee is that (2) is never again judging an empty set. It costs nothing when the
  // client is well-behaved, and the page's own requests are unaffected: the probe is not a browser
  // request, so it never enters `page.requests` and never perturbs the concurrency watch or the
  // counter cross-check below.
  const probeUrl = `${ORIGIN}/api/tiles?` + new URLSearchParams({
    token: TOKEN, level_f: "0", level_t: "0", f_index: "0", t_index: "0", cells: "8",
  });
  // **The still-view claim (2c): what the page itself may ask for.** Its premise is measured at
  // both ends of the window rather than assumed. **No pane is
  // following the live edge** by the time the gestures are over — every one of them was dragged or
  // scrubbed off it, which `data-following` states as a fact rather than a measurement (T-478, and
  // the same attribute `live-edge.e2e.mjs` asserts on). A frozen pane over already-captured data,
  // with nothing moving it, **wants nothing**: the record may well still be growing (it is — the
  // replay ingests throughout, `latest_s` advances ~8 s across this window), but no pane is showing
  // the place it is growing at. So the second assertion below is that the page asks for nothing,
  // and it is the one T-471's ring fails outright (27-33 requests here, against 0 for a client that
  // asks only for what it draws). If a pane IS following, new rows are legitimately wanted and the
  // claim is not made — reported as not made, never assumed away.
  const FOLLOWING = `[...document.querySelectorAll('.hk-surface-viewport[data-viewport="pane"]')]` +
    `.map((v) => v.getAttribute('data-following')).join(",")`;
  const followingBefore = await page.eval(FOLLOWING);
  const probes = [];
  for (let i = 0; i < STEADY_STATE_MS / 500; i++) {
    await new Promise((r) => setTimeout(r, 500));
    await sampleCap();
    if (i % 2 === 1) {
      const t0 = Date.now();
      const status = await fetch(probeUrl).then((r) => r.status, () => 0);
      probes.push({ status, ms: Date.now() - t0 });
    }
  }
  const followingAfter = await page.eval(FOLLOWING);
  const frozen = (f) => f.length > 0 && !f.split(",").includes("true");

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
  const probeRefusals = probes.filter((p) => p.status === 503);
  t.diagnostic(`panes following the live edge across the steady window: [${followingBefore}] -> ` +
    `[${followingAfter}] (${frozen(followingBefore) && frozen(followingAfter)
      ? "all frozen: the still-view claim applies" : "one is live: claim not made"})`);
  t.diagnostic(`steady-state slot probes: ${probes.length} asked, ` +
    `${probes.filter((p) => p.status === 200).length} answered, ${probeRefusals.length} refused · ` +
    `statuses ${probes.map((p) => p.status).join(" ")} · ${probes.map((p) => `${p.ms}ms`).join(" ")}`);
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

  // (2b) …AND THE WINDOW WAS ACTUALLY ASKED. (2) is a claim about a set the page may leave empty —
  // measured empty on this fixture, every run — so on its own it certifies nothing. These two say
  // the window was exercised and by what: the route answered every serial probe made while the page
  // sat idle, so its slot budget was genuinely free. A page that walked away from reads the server
  // is still producing refuses these, which is the leak stated as the user's own experience of it:
  // a second tab, or this page's next gesture, being told the route is full.
  assert.ok(probes.length >= 4,
    `only ${probes.length} slot probes were made during the steady-state window — too few for (2) ` +
    "to be a test rather than a formality");
  assert.deepEqual(probeRefusals, [],
    `the tile route refused ${probeRefusals.length} of ${probes.length} single, serial tile ` +
    `requests made while the page was idle (statuses: ${probes.map((p) => p.status).join(" ")}). ` +
    "Nothing else was asking, so the budget those slots came out of was held by reads this client " +
    "abandoned and the server is still producing — T-454's leak, from outside the client.");
  assert.ok(probes.every((p) => p.status === 200),
    `a steady-state slot probe did not get an answer at all (statuses: ${probes.map((p) => p.status).join(" ")}) — ` +
    "a probe that errors proves nothing either way, so the assertion above would be vacuous");

  // (2c) A FROZEN VIEW THAT NOTHING TOUCHES ASKS FOR NOTHING. The premise is measured at both ends
  // of the window, not assumed: every pane is off the live edge, so nothing new can be wanted, and
  // only then is "it must ask for nothing" the right claim. What this forbids is speculation — and
  // speculation is not free: every speculative read that a later gesture aborts leaves the server
  // producing a tile nobody will read, holding the slot (2) and (2b) are about.
  if (frozen(followingBefore) && frozen(followingAfter)) {
    assert.equal(steadyRequests, 0,
      `the page made ${steadyRequests} tile requests during ${(STEADY_STATE_MS / 1000).toFixed(0)} s ` +
      "in which nothing moved the view and no pane was following the live edge. With nothing new on " +
      "screen to draw, a tile request is speculation — and speculation a later gesture aborts is a " +
      "server slot spent on a read nobody will ever look at (T-471).");
  } else {
    t.diagnostic(`a pane was still following the live edge ([${followingBefore}] -> [${followingAfter}]), ` +
      "so new rows were legitimately wanted: the still-view claim is NOT made this run");
  }

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

// ———————————————————————————————————————————————————————————————————————————————————————————————
// T-456: GOOGLE-MAPS NAVIGATION, AND THE MODIFIER THE BROWSER ACTUALLY DELIVERED
// ———————————————————————————————————————————————————————————————————————————————————————————————
//
// The user asked for four gestures — drag pans both axes, plain wheel zooms both about the cursor,
// shift+wheel takes frequency, and a modifier takes time — and named the risk in the same breath:
// **ctrl+wheel is captured by macOS and by browsers, so confirm the chosen modifier actually works
// in a real browser.** That is why this test is here rather than in `ui/test`: a unit test hands
// `{ altKey: true }` to a function and learns nothing about whether a browser would ever produce it.
//
// So every wheel below is dispatched through CDP with real **modifier bits** (Alt 1, Ctrl 2, Meta 4,
// Shift 8), which the browser turns back into `altKey`/`ctrlKey`/… on the `WheelEvent` the page
// receives — and a probe listener in the page **reads the modifier that arrived** and reports it.
// The distinction is the one this milestone keeps getting wrong: *an assertion that the view zoomed
// is not an assertion that the browser delivered your modifier.* Both are made, separately.
//
// ——— WHAT THIS CANNOT PROVE, STATED BEFORE THE GREEN ———
//
// A CDP event enters at the **renderer**. It therefore cannot answer the question that decided the
// binding: macOS's Accessibility → Zoom setting *"Use scroll gesture with modifier keys to zoom"*
// defaults to ^Control and, when enabled, consumes ctrl+scroll in the **window server** — no `wheel`
// is dispatched to any browser, so there is nothing to `preventDefault` and nothing for this test to
// observe. No in-browser harness can distinguish that from the user not having scrolled.
//
// That is why the time axis is on **ALT/OPTION** and ctrl is deliberately left unbound, and why the
// ctrl case below gathers evidence rather than asserting a binding. The second reason is visible
// here though: Chrome and Safari deliver a trackpad **pinch** as a wheel with `ctrlKey` set, so a
// ctrl binding would make a pinch zoom one axis. Unbound, it falls into the uniform gesture.
//
// ——— THE PREMISE THIS TEST MEASURES BEFORE IT USES IT ———
//
// A pane may not magnify below `minCells` (16) level-0 cells (`panes.ts`), which on this lattice is
// 16 × 1 s = 16 s, and the surface's time extent is the record `hk serve --replay --loop` has
// ingested so far — it grows in real time. A young record leaves the time axis clamped in both
// directions, and then "the plain wheel moved the time axis" would fail on correct code. So the room
// is **measured from the server's own numbers and waited for**, not assumed from where this file
// happens to sit in the run order.

/** `minCells` in ui/src/surface/panes.ts: the zoom floor, in level-0 cells. Restated, and asserted
 * against the server's own cell size below rather than against a second copy of the cell size. */
const MIN_CELLS = 16;
/**
 * How much room above that floor the time axis needs before a two-step wheel can be seen to move it.
 *
 * Two and a half floors: two wheel steps are a factor of 0.487, so 2.5 × 16 s = 40 s → 19.5 s, which
 * lands clear of the clamp rather than on it. Deliberately not higher — the record grows at whatever
 * rate the replay is ingested, so a bound that needs a minute of history is a bound that fails on a
 * runner whose ingest lags wall-clock, and this premise must be reachable there too.
 */
const TIME_ROOM = 2.5;

/** The surface's time extent and zoom floor, from the two routes that state them. */
async function timeRoom() {
  const get = async (p) => {
    const r = await fetch(`${ORIGIN}${p}${p.includes("?") ? "&" : "?"}token=${TOKEN}`);
    if (!r.ok) throw new Error(`GET ${p} → ${r.status}`);
    return r.json();
  };
  const nav = await get("/api/navigation");
  const tile = await get("/api/tiles?level_f=0&level_t=0&f_index=0&t_index=0&cells=8");
  const latestS = nav?.time?.latest_s;
  const oldestS = tile?.coverage?.horizon?.oldest_record_s;
  const cellS = nav?.time?.min_t_cell_s;
  if (![latestS, oldestS, cellS].every((v) => typeof v === "number" && Number.isFinite(v))) {
    throw new Error(`the server did not state the surface's time extent or its finest cell: ` +
      `latest=${latestS} oldest=${oldestS} cell=${cellS}`);
  }
  return { extentS: latestS - oldestS, floorS: MIN_CELLS * cellS };
}

/** The pane row of the chrome — not the map, which is a viewport too and moves for other reasons. */
const PANE_ROW = `(() => { const v = document.querySelector('.hk-surface-viewport[data-viewport="pane"]');
  if (!v) return null;
  const where = v.querySelector('.hk-surface-where')?.textContent ?? '';
  const level = v.querySelector('.hk-surface-level')?.textContent ?? '';
  const parts = where.split(' · ');
  return { freq: parts[0] ?? '', time: parts[1] ?? '', level };
})()`;

/**
 * Record every wheel the browser dispatches, **as the page receives it**.
 *
 * Registered on `window`, passive, so it runs after the canvas's own non-passive listener and its
 * `defaultPrevented` reports whether the page actually suppressed the browser's default — which for
 * ctrl+wheel and cmd+wheel is page zoom. `devicePixelRatio` and `visualViewport.scale` are recorded
 * so that suppression can be checked against the browser rather than against the page's own claim.
 */
const INSTALL_PROBE = `(() => {
  window.__wheels = [];
  window.__zoomBase = { dpr: window.devicePixelRatio,
                        scale: window.visualViewport ? window.visualViewport.scale : null };
  window.addEventListener("wheel", (e) => window.__wheels.push({
    shift: e.shiftKey, alt: e.altKey, ctrl: e.ctrlKey, meta: e.metaKey,
    deltaX: e.deltaX, deltaY: e.deltaY, deltaMode: e.deltaMode,
    prevented: e.defaultPrevented,
    slot: e.target && e.target.getAttribute ? e.target.getAttribute("data-slot") : null,
  }), { passive: true });
  return true;
})()`;

const ZOOM_NOW = `JSON.stringify({ dpr: window.devicePixelRatio,
  scale: window.visualViewport ? window.visualViewport.scale : null, base: window.__zoomBase })`;

test("T-456: drag pans, a plain wheel zooms BOTH axes, and the modifiers reach the page", async (t) => {
  // ——— the premise, measured and waited for ———
  let room = await timeRoom();
  const waitedFrom = Date.now();
  while (room.extentS < TIME_ROOM * room.floorS && Date.now() - waitedFrom < 120000) {
    await new Promise((r) => setTimeout(r, 2000));
    room = await timeRoom();
  }
  t.diagnostic(`time axis: ${room.extentS.toFixed(0)} s of record against a ${room.floorS.toFixed(0)} s ` +
    `zoom floor (${(room.extentS / room.floorS).toFixed(1)} floors)` +
    (Date.now() - waitedFrom > 2000 ? `, after waiting ${((Date.now() - waitedFrom) / 1000).toFixed(0)} s for the record to grow` : ""));
  assert.ok(room.extentS >= TIME_ROOM * room.floorS,
    `the recording has only ${room.extentS.toFixed(0)} s of record against a ${room.floorS.toFixed(0)} s ` +
    "zoom floor, so the time axis is clamped in both directions and a uniform zoom CANNOT be seen to " +
    "move it. This is the premise, not the claim: without it a green run would prove nothing about " +
    "the time half of the gesture.");

  const browser = await Browser.open();
  t.after(() => browser.close());
  const page = await browser.page();
  await page.goto(`${ORIGIN}/surface.html#token=${TOKEN}`);
  await page.waitForSurfaceMounted();
  await page.waitFor("the first tile textures to be uploaded",
    `(${STATUS}.match(/(\\d+) uploads/)?.[1] | 0) > 0`, { timeoutMs: 90000 });
  assert.equal(await page.eval(INSTALL_PROBE), true);

  const rect = await page.$rect('[data-slot="canvas"]');
  const mid = { x: rect.x + rect.w / 2, y: rect.y + rect.h * 0.35 };
  const read = async () => page.eval(PANE_ROW);
  const levels = (row) => row.level.match(/level (\d+)\/(\d+)/)?.slice(1).map(Number) ?? [NaN, NaN];

  /**
   * Put both axes back on the whole surface — the state with the most room to zoom INTO, on both
   * axes, which is what every wheel gesture below needs.
   *
   * `inSteps` then zooms uniformly back in. A pan needs the opposite of what a zoom needs: on the
   * whole surface the window already *is* the surface, so `PaneModel.normalise` clamps the centre
   * on both axes and a drag is correctly a no-op. Two uniform steps leave ~half the surface off
   * screen on each axis, which is room to pan into.
   */
  const reset = async (inSteps = 0) => {
    await page.click(BUTTON("Whole surface"));
    await page.frames(4);
    for (let i = 0; i < inSteps; i++) await page.wheel(mid, -240);
    if (inSteps) await page.frames(4);
    await page.eval("window.__wheels = []");
    return read();
  };

  /** One gesture, from a stated starting window: what the view did, and what the browser delivered. */
  const gesture = async (what, run, { inSteps = 0 } = {}) => {
    const was = await reset(inSteps);
    await run();
    await page.frames(6);
    const now = await read();
    const wheels = JSON.parse(await page.eval("JSON.stringify(window.__wheels)"));
    t.diagnostic(`${what}: freq ${was.freq} → ${now.freq} · time ${was.time} → ${now.time} · ${now.level}`);
    t.diagnostic(`${what}: ${wheels.length} wheel event(s) reached the page` +
      (wheels.length ? `, first { shift:${wheels[0].shift} alt:${wheels[0].alt} ctrl:${wheels[0].ctrl} ` +
        `meta:${wheels[0].meta} deltaX:${wheels[0].deltaX} deltaY:${wheels[0].deltaY} ` +
        `defaultPrevented:${wheels[0].prevented} target:${wheels[0].slot} }` : ""));
    return { was, now, wheels, freqMoved: now.freq !== was.freq, timeMoved: now.time !== was.time };
  };

  // ——— 1. DRAG PANS BOTH AXES ———
  // Diagonal, so a handler that fed one component to both axes — or measured travel as the sum of
  // the two, which is T-407's defect — would show up as the wrong axis moving.
  const drag = await gesture("drag (diagonal)", () =>
    page.drag(mid, { x: mid.x - 240, y: mid.y + 140 }, 12), { inSteps: 2 });
  assert.ok(drag.freqMoved, "a drag must pan the FREQUENCY axis");
  assert.ok(drag.timeMoved, "a drag must pan the TIME axis: both axes, in one gesture");
  assert.deepEqual(levels(drag.now), levels(drag.was), "a pan must not change either pyramid level");

  // ——— 2. PLAIN WHEEL: UNIFORM ZOOM, BOTH AXES ———
  const plain = await gesture("plain wheel", async () => {
    for (let i = 0; i < 2; i++) await page.wheel(mid, -240);
  });
  assert.equal(plain.wheels.length, 2, "the browser did not deliver the plain wheels to the page at all");
  assert.ok(plain.wheels.every((w) => !w.shift && !w.alt && !w.ctrl && !w.meta),
    `a plain wheel arrived carrying a modifier: ${JSON.stringify(plain.wheels)}`);
  assert.ok(plain.wheels.every((w) => w.prevented),
    "the page did not preventDefault a wheel over the canvas — the browser is free to scroll or zoom the page under it");
  assert.ok(plain.freqMoved, "a plain wheel must zoom the FREQUENCY axis: it is the uniform gesture");
  assert.ok(plain.timeMoved, "a plain wheel must zoom the TIME axis too — that is what 'uniform' means");

  // …AND THE AXES ARE STILL INDEPENDENTLY LEVELLED. One gesture, one factor, two axes — but two
  // levels, resolved separately from two different cell sizes. A uniform gesture that collapsed the
  // pyramid to a single level would make these equal, which is exactly the re-welding T-434 undid.
  const [lf, lt] = levels(plain.now);
  t.diagnostic(`after the uniform zoom the pane is at level ${lf}/${lt} — one gesture, two levels`);
  assert.ok(Number.isFinite(lf) && Number.isFinite(lt), `the chrome did not state a level: "${plain.now.level}"`);
  assert.notEqual(lf, lt,
    `one uniform gesture left both axes at level ${lf}: the frequency and time levels have been welded together`);

  // ——— 3. SHIFT + WHEEL: FREQUENCY ONLY ———
  const shift = await gesture("shift + wheel", async () => {
    for (let i = 0; i < 2; i++) await page.wheel(mid, -240, { shift: true });
  });
  assert.ok(shift.wheels.length > 0 && shift.wheels.every((w) => w.shift),
    `the browser did not deliver shiftKey on the wheel: ${JSON.stringify(shift.wheels)}`);
  assert.ok(shift.freqMoved, "shift + wheel must zoom frequency");
  assert.equal(shift.timeMoved, false,
    `shift + wheel moved the TIME axis (${shift.was.time} → ${shift.now.time}): the axes are welded`);

  // ——— 4. ALT / OPTION + WHEEL: TIME ONLY ———
  // The binding the whole ticket turns on, and the one a unit test cannot speak for.
  const alt = await gesture("alt + wheel", async () => {
    for (let i = 0; i < 2; i++) await page.wheel(mid, -240, { alt: true });
  });
  assert.ok(alt.wheels.length > 0, "the browser delivered NO wheel event at all when alt was held — " +
    "alt+wheel is being consumed before the page sees it, and the time axis has no modifier");
  assert.ok(alt.wheels.every((w) => w.alt && !w.ctrl && !w.shift),
    `the modifier that ARRIVED was not alt: ${JSON.stringify(alt.wheels)}`);
  assert.ok(alt.wheels.every((w) => w.prevented), "an alt wheel over the canvas was not preventDefaulted");
  assert.ok(alt.timeMoved, `alt + wheel must zoom TIME (it stayed at ${alt.was.time})`);
  assert.equal(alt.freqMoved, false,
    `alt + wheel moved the FREQUENCY axis (${alt.was.freq} → ${alt.now.freq}): the axes are welded`);

  // ——— 5. CTRL + WHEEL: EVIDENCE, NOT A BINDING ———
  // Ctrl is deliberately unbound, so the claim here is only what that implies: whatever the browser
  // does deliver must fall into the uniform gesture, and the PAGE must not be zoomed underneath the
  // surface. The renderer-level answer below is reported for exactly what it is worth — it says
  // nothing about the window server, which is the reason the binding is alt.
  const ctrl = await gesture("ctrl + wheel (unbound: expected to behave as a plain wheel)", async () => {
    for (let i = 0; i < 2; i++) await page.wheel(mid, -240, { ctrl: true });
  });
  const zoom = JSON.parse(await page.eval(ZOOM_NOW));
  t.diagnostic(`ctrl + wheel: devicePixelRatio ${zoom.base.dpr} → ${zoom.dpr}, ` +
    `visualViewport.scale ${zoom.base.scale} → ${zoom.scale}`);
  assert.equal(zoom.dpr, zoom.base.dpr,
    "ctrl + wheel over the canvas zoomed the PAGE: the listener's preventDefault is not binding");
  assert.equal(zoom.scale, zoom.base.scale, "ctrl + wheel pinch-zoomed the page under the surface");
  if (ctrl.wheels.length === 0) {
    t.diagnostic("ctrl + wheel: the browser delivered NO wheel to the page — which is precisely the " +
      "failure mode that put the time axis on alt, observed here at the renderer rather than at the OS.");
  } else {
    assert.ok(ctrl.wheels.every((w) => w.ctrl), `ctrlKey did not arrive: ${JSON.stringify(ctrl.wheels)}`);
    assert.ok(ctrl.wheels.every((w) => w.prevented),
      "a ctrl wheel over the canvas was not preventDefaulted, so the browser's page zoom is live");
    assert.ok(ctrl.freqMoved && ctrl.timeMoved,
      "ctrl is unbound, so a ctrl wheel must be the uniform gesture — this is the path a trackpad " +
      `pinch takes (freq moved: ${ctrl.freqMoved}, time moved: ${ctrl.timeMoved})`);
  }

  // ——— and none of it broke the page ———
  assert.equal(await page.$count(".sp-fail"), 0, `a failure card appeared: ${await page.$text(".sp-fail")}`);
  assert.deepEqual(page.exceptions, [], "uncaught exception during the gesture run");

  // The gestures leave the pane on the whole surface, of which 0.8 % was ever sampled — so a census
  // there measures the fixture's coverage, not the renderer, and would read a correct picture as a
  // flat fill. "Fit to coverage" is the page's own way back to the region the backend reported as
  // observed, which is the only window where "is it still drawing?" is a question about drawing.
  await page.click(BUTTON("Fit to coverage"));
  await page.frames(8);
  await new Promise((r) => setTimeout(r, 1200));
  const shots = await page.shot(path.join(ART, "surface-gestures.png"));
  const c = census(shots, { x: Math.round(rect.x), y: Math.round(rect.y), w: Math.round(rect.w), h: Math.round(rect.h) });
  assert.ok(c.distinct >= 32 && c.dominantShare < 0.92,
    `after the gestures the canvas is a flat fill: ${c.distinct} colours, dominant ${c.dominant} at ${(c.dominantShare * 100).toFixed(1)} %`);
});


// ———————————————————————————————————————————————————————————————————————————————————————————————
// T-472: AT EITHER BOUND, A PLAIN WHEEL MOVES NEITHER AXIS
// ———————————————————————————————————————————————————————————————————————————————————————————————
//
// The user's report, against T-456's uniform zoom: past the Y-axis limit the gesture keeps scaling X
// while Y is clamped, so the aspect ratio drifts, the view jumps, and the frequency axis has to be
// shift-scrolled back **every time**. The fix stops both axes together when either runs out.
//
// ——— WHAT THIS TEST IS EVIDENCE OF, SAID BEFORE THE GREEN ———
//
// *A zoom being clamped is not the same as the aspect ratio being preserved.* The old code clamped
// too — per axis, which is exactly how the ratio drifted. The **ratio** property is therefore not
// asserted here: it is asserted over the whole reachable range of the gesture in
// `ui/test/surface-aspect.test.ts` (19 440 wheels over a grid of surfaces, starting windows, cursor
// anchors and every factor a wheel can produce — 7 705 violations through the per-axis gesture, 0
// through this one). What a browser adds, and only a browser can, is that **the real event reaches
// the real host and the real host does this** — the T-450 lesson, where a module that could not load
// in a browser had been proved correct on 114 973 pixels.
//
// So the claim here is the bound behaviour, as three facts that only mean something together:
//
//   (a) at the stop, a plain wheel moves NEITHER axis;
//   (b) at that same stop, **alt+wheel inward also moves nothing** — so it is the TIME axis that ran
//       out; and
//   (c) at that same stop, **shift+wheel inward still moves frequency** — so the frequency axis had
//       room, and (a) is therefore the LOCK stopping the gesture rather than frequency's own clamp.
//
// Without (c), (a) is satisfied by a view zoomed out to everything on both axes, which is precisely
// where the old code ended up: it dragged frequency on to its own floor after time had stopped.
//
// ——— THE WITNESSES, AND WHY EACH ONE CAN SPEAK ———
//
// The stated pyramid level is **useless as a time witness on this fixture**, and that is measured
// rather than assumed: the whole record is a few tens of seconds against a ~500 px pane, so the time
// axis resolves to level 0 at every span it can hold, and `level 10/0 → level 3/0` across a fourteen
// -step zoom is a frequency story with a constant beside it. A test that had read the level for
// "time did not move" would have been green for a gesture that moved time freely — T-448's shape,
// where the gate guaranteed every counter except the one asserted.
//
// So time is read from the pane's offset-from-the-edge, and T-478's trap is handled by **checking
// its premise instead of asserting the habit**: that offset drifts with wall-clock lag when a pane
// is FOLLOWING a live edge, and an `equal` on it then goes red under load while a `notEqual` can go
// green on drift alone. This page has no live edge to follow — `SurfacePreview.edgeNs` resolves once
// from `GET /api/navigation` and every viewport opens frozen — so the offset here is pure view
// state. That is not taken on trust: the readout is sampled twice across real frames with nothing
// touching it, and the test fails if it moved.
//
// And "nothing moved" is worthless if nothing was delivered, so every gesture's wheel events are
// read back from the page's own probe and checked for the modifier bits the browser actually set.

/**
 * A cap on the zoom-in walk, not a claim about where it stops.
 *
 * Where it stops is fixture-dependent — it is `log(time floor / time extent)` in wheel steps — and
 * this file must not restate it, because a cap tuned to the fixture is a second copy of a number the
 * server owns. It only has to be larger than that, and the assertion that matters is (c): with the
 * lock removed the walk runs on to FREQUENCY's floor instead, and shift+wheel is then dead too.
 */
const MAX_STEPS = 60;
/**
 * How many modifier wheels a single-axis probe uses.
 *
 * Not one: a single step can move a span by less than the readout's own resolution, and "it did not
 * change" would then be evidence of nothing. Four steps are 4.2x out and 0.24x in.
 */
const PROBE_STEPS = 4;

test("T-472: at the bound a plain wheel moves NEITHER axis, while shift and alt each still move one", async (t) => {
  const browser = await Browser.open();
  t.after(() => browser.close());
  const page = await browser.page();
  await page.goto(`${ORIGIN}/surface.html#token=${TOKEN}`);
  await page.waitForSurfaceMounted();
  await page.waitFor("the first tile textures to be uploaded",
    `(${STATUS}.match(/(\\d+) uploads/)?.[1] | 0) > 0`, { timeoutMs: 90000 });
  assert.equal(await page.eval(INSTALL_PROBE), true);

  const rect = await page.$rect('[data-slot="canvas"]');
  // Off-centre on both axes, so the two anchors differ and a gesture that fed one to both would show.
  const at = { x: rect.x + rect.w * 0.42, y: rect.y + rect.h * 0.35 };
  const read = async () => page.eval(PANE_ROW);
  const same = (a, b) => a.freq === b.freq && a.time === b.time && a.level === b.level;
  const show = (r) => `${r.freq} · ${r.time} · ${r.level}`;

  // ——— the premise that licenses reading the time offset at all (T-478) ———
  const idle0 = await read();
  await page.frames(12);
  await new Promise((r) => setTimeout(r, 1200));
  await page.frames(12);
  const idle1 = await read();
  assert.ok(same(idle0, idle1),
    `the pane's readout moved with NO gesture touching it (${show(idle0)} → ${show(idle1)}). On a page ` +
    "that follows a live edge the offset drifts with wall-clock lag, and then neither an equal nor a " +
    "notEqual on it means anything — this preview is supposed to hold a fixed edge.");

  /** One wheel, and everything the readout says. */
  const wheel = async (mods = {}, deltaY = -240) => {
    const was = await read();
    await page.eval("window.__wheels = []");
    await page.wheel(at, deltaY, mods);
    await page.frames(4);
    const now = await read();
    const wheels = JSON.parse(await page.eval("JSON.stringify(window.__wheels)"));
    return { was, now, wheels, moved: !same(was, now) };
  };

  /** `PROBE_STEPS` wheels of one kind, reported as one move — see [[PROBE_STEPS]]. */
  const probe = async (mods, deltaY) => {
    const was = await read();
    await page.eval("window.__wheels = []");
    for (let i = 0; i < PROBE_STEPS; i++) await page.wheel(at, deltaY, mods);
    await page.frames(6);
    const now = await read();
    const wheels = JSON.parse(await page.eval("JSON.stringify(window.__wheels)"));
    assert.equal(wheels.length, PROBE_STEPS,
      `the browser delivered ${wheels.length} of ${PROBE_STEPS} wheels for ${JSON.stringify(mods)} — ` +
      "a gesture that did not arrive cannot be evidence that it moved nothing");
    assert.ok(wheels.every((w) => w.prevented), "a wheel over the canvas was not preventDefaulted");
    return { was, now, wheels };
  };

  // ——— out to the whole surface, then wheel IN until the uniform gesture stops ———
  await page.click(BUTTON("Whole surface"));
  await page.frames(4);
  t.diagnostic(`whole surface: ${show(await read())}`);

  let steps = 0;
  for (let i = 0; i < MAX_STEPS; i++) {
    const w = await wheel({});
    assert.equal(w.wheels.length, 1, "the browser did not deliver the plain wheel to the page");
    assert.ok(!w.wheels[0].shift && !w.wheels[0].alt && !w.wheels[0].ctrl && !w.wheels[0].meta,
      `a plain wheel arrived carrying a modifier: ${JSON.stringify(w.wheels[0])}`);
    assert.ok(w.wheels[0].prevented, "the page did not preventDefault a wheel over the canvas");
    if (!w.moved) break;
    steps++;
  }
  const held = await read();
  t.diagnostic(`plain wheel in: ${steps} step(s), then it stopped at ${show(held)}`);

  // Non-vacuity in the first direction: the gesture does something before it stops. A wheel wired to
  // nothing would satisfy every "did not move" below.
  assert.ok(steps > 0,
    "the plain wheel moved the view on none of its steps, so 'it stops at the bound' is a statement " +
    "about a gesture that never worked");
  assert.ok(steps < MAX_STEPS,
    `the plain wheel was still moving the view after ${MAX_STEPS} steps — it never stopped, so ` +
    "nothing below is being tested at a bound");

  // ——— (a) AT THE BOUND, A PLAIN WHEEL MOVES NEITHER AXIS ———
  for (let i = 0; i < 3; i++) {
    const w = await wheel({});
    assert.equal(w.wheels.length, 1, "a plain wheel at the bound was not delivered, so this proves nothing");
    assert.equal(w.now.freq, held.freq,
      `a plain wheel at the bound moved the FREQUENCY axis (${held.freq} → ${w.now.freq}). This is the ` +
      "reported bug: one axis is clamped, the other keeps scaling, and the aspect ratio drifts.");
    assert.equal(w.now.time, held.time,
      `a plain wheel at the bound moved the TIME axis (${held.time} → ${w.now.time})`);
    assert.equal(w.now.level, held.level, `a plain wheel at the bound changed the stated level`);
  }

  // ——— (b) IT IS TIME THAT RAN OUT: four alt wheels inward move nothing either ———
  const altIn = await probe({ alt: true }, -240);
  assert.ok(altIn.wheels.every((w) => w.alt && !w.shift && !w.ctrl),
    `the modifier that ARRIVED was not alt: ${JSON.stringify(altIn.wheels)}`);
  assert.equal(altIn.now.time, held.time,
    `${PROBE_STEPS} alt wheels inward still moved the time axis (${held.time} → ${altIn.now.time}), so ` +
    "the plain wheel above did not stop because TIME ran out. This run reached some other bound, and " +
    "(c) below would be measuring the wrong thing.");
  assert.equal(altIn.now.freq, held.freq, "alt + wheel moved the FREQUENCY axis: the axes are welded");

  // ——— (c) FREQUENCY HAD ROOM: shift inward still moves it, and only it ———
  // The assertion that makes (a) mean something. Without it, "a plain wheel moved neither axis" is
  // equally true of the old code once BOTH axes are pinned — which is where the old code went, by
  // dragging frequency down to its own floor after time had already stopped. (Measured: with the
  // lock removed this same walk runs to that floor instead, and shift is then dead here too.)
  const shiftIn = await probe({ shift: true }, -240);
  assert.ok(shiftIn.wheels.every((w) => w.shift),
    `the browser did not deliver shiftKey on the wheel: ${JSON.stringify(shiftIn.wheels)}`);
  assert.notEqual(shiftIn.now.freq, held.freq,
    `shift + wheel did not move the frequency axis either (${held.freq}). Frequency is at its own ` +
    "bound too, so the plain wheel above was stopped by the pane rather than by the lock — and the " +
    "escape hatch the user reaches for is dead.");
  assert.equal(shiftIn.now.time, held.time,
    `shift + wheel moved the TIME axis (${held.time} → ${shiftIn.now.time}): the axes are welded`);
  t.diagnostic(`shift + wheel at the lock: ${held.freq} → ${shiftIn.now.freq} (frequency had room all along)`);

  // ——— (d) …AND ALT STILL SKEWS THE OTHER WAY, deliberately ———
  // Only shift and alt may change the aspect ratio, and this pair is also what says the axes are
  // still INDEPENDENT: alt moves time and leaves the frequency window exactly where it was. The fix
  // that would quietly undo T-434/T-438/T-440 — keeping the pixels square by welding the two axes
  // together — cannot produce either (c) or (d).
  const base = await read();
  const altOut = await probe({ alt: true }, 240);
  t.diagnostic(`alt + wheel outward: time ${base.time} → ${altOut.now.time}, frequency ${altOut.now.freq}`);
  assert.notEqual(altOut.now.time, base.time,
    `${PROBE_STEPS} alt wheels outward did not move the time axis (${base.time}): it is welded shut`);
  assert.equal(altOut.now.freq, base.freq,
    `alt + wheel outward moved the FREQUENCY axis (${base.freq} → ${altOut.now.freq})`);

  assert.equal(await page.$count(".sp-fail"), 0, `a failure card appeared: ${await page.$text(".sp-fail")}`);
  assert.deepEqual(page.exceptions, [], "uncaught exception during the bound run");
});

// ---------------------------------------------------------------------------
// T-486: the snap-to-live dead zone, in a real browser with a real advancing edge
// ---------------------------------------------------------------------------
//
// The user reported this twice, and both halves are gestures rather than arithmetic, so the unit
// tier cannot be the last word on them: a 1 px time-pan dropped the pane out of live, and a drag
// back toward the top — released as a new row appended under the cursor — re-paused.
//
// **It asserts `data-following`, never the readout string.** T-478: a following pane's chrome ends
// in its offset from the live edge, and that offset drifts with wall-clock lag, so a readout is a
// measurement of the lag rather than a statement of the state. This ticket is precisely about small
// offsets from the edge, which is the worst possible thing to read a small-offset-tolerant state
// from. `data-following` is what the chrome sets from the pane model's own answer.
//
// **Non-vacuity is built in as step 2**: the same page, same pointer, a bigger drag, and the
// attribute must go to `false`. A guard that could not observe the pane leaving live would pass
// step 1 no matter what the client did.
test("T-486: a 1 px time-pan keeps the pane LIVE; a real drag pauses it; a drag back to the edge returns it", async (t) => {
  const browser = await Browser.open();
  t.after(() => browser.close());
  const page = await browser.page();
  // **The APP page, not `/surface.html`.** The standalone preview reports no live edge, so
  // `SurfacePreview` correctly freezes every viewport at open (T-450's historical preview) — there
  // would be no follow state to hold, and this test would pass by describing a page it is not about.
  assert.equal(await page.goto(`${ORIGIN}/#token=${TOKEN}`), "load");
  await page.waitFor("the app's surface to draw",
    `!!document.querySelector('.sf-canvas') && document.querySelector('.sf-canvas').width > 200`,
    { timeoutMs: 60000 });
  await page.waitFor("the chrome to report a viewport",
    `document.querySelectorAll('.hk-surface-viewport[data-viewport="pane"]').length > 0`, { timeoutMs: 30000 });

  const FOLLOWING = `[...document.querySelectorAll('.hk-surface-viewport[data-viewport="pane"]')]` +
    `.map((v) => v.getAttribute('data-following'))`;
  const follow = async () => JSON.parse(await page.eval(`JSON.stringify(${FOLLOWING})`));

  // The premise, asserted rather than assumed: there IS a pane following a live edge to fall out of.
  const start = await follow();
  assert.deepEqual(start, ["true"],
    `no pane is following the live edge, so there is nothing for a dead zone to hold: ${JSON.stringify(start)}`);

  const rect = await page.$rect(".sf-canvas");
  const mid = { x: rect.x + rect.w / 2, y: rect.y + rect.h * 0.35 };

  // **Make room on the time axis first, and say why.** A following pane opens on the whole observed
  // extent, so `PaneModel.normalise` clamps its centre and a backward pan is a no-op — the pane
  // would stay live for a reason that has nothing to do with a dead zone, and step 2's control
  // would be vacuous. Alt+wheel zooms time alone (T-456) and a zoom is not a pause, so this leaves
  // the pane following with the whole record behind it to scrub into.
  for (let i = 0; i < 3; i++) await page.wheel(mid, -240, { alt: true });
  await page.frames(6);
  assert.deepEqual(await follow(), ["true"], "zooming the time axis must not pause the pane: a zoom is not a pause");

  const step = async (what, dy, steps) => {
    await page.drag(mid, { x: mid.x, y: mid.y + dy }, steps);
    await page.frames(6);
    const now = await follow();
    t.diagnostic(`${what} (${dy} px): data-following = ${JSON.stringify(now)}`);
    return now;
  };

  // 1. THE REPORTED DEFECT. One pixel, straight up the time axis — a twitch, not a scrub.
  assert.deepEqual(await step("a 1 px time-pan", -1, 1), ["true"],
    "a 1 px time-pan dropped the pane out of live: this is the dead zone the user asked for, twice");

  // 2. THE CONTROL, and the other half of the rule: dragged well beyond the zone it commits to pause.
  assert.deepEqual(await step("a 160 px scrub", -160, 12), ["false"],
    "a real scrub did not pause the pane — so step 1 proves nothing, because the attribute never moves");

  // 3. AND BACK. A drag hard toward the edge clamps against it, and the release is a return to live
  //    rather than a pause a few rows short of it — with rows appending under the cursor throughout.
  assert.deepEqual(await step("a drag back to the live edge", 420, 14), ["true"],
    "a drag released at the live edge left the pane frozen — the second half of the report");
});
