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
import { Browser, addressPeak, census, tileAsks, until } from "./harness.mjs";
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

/**
 * **A status line this file cannot parse is a wording drift, never a controller state** — so it
 * fails at once, naming the line, before anything waits on a parse.
 *
 * On 2026-09-23 the share clause gained its age (`share 2, stated 0.4 s ago`, a6ae3d17) while this
 * file matched `in flight \(share (\d+)\)` with the closing paren. Every readout became null; the
 * T-846 test spent 60 s waiting for a premise it could not parse and then reported "the operating
 * cap never reached 2 with the page quiet (last: null)" — a claim about the controller, on a page
 * whose status line read `0+0/2 in flight (share 2, stated 8.2 s ago) · queue 0`, which IS the
 * premise. The merge runner took that for a product defect on main. `parse` is the caller's own
 * reader, so this checks the exact pattern the claims below are read with.
 */
const assertReadable = (parse, st) => {
  const c = parse(st);
  assert.ok(c && Number.isFinite(c.busy) && Number.isFinite(c.queue),
    "the page's status line does not read as `N+M/L in flight (share C` … `queue Q` … `B backpressure`: " +
    "this file's parser and preview-main.ts's wording have drifted, so every controller readout " +
    `would be null and every claim about the cap vacuous. The line: ${JSON.stringify(st)}`);
  return c;
};

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

  // **The gesture point is re-derived from the canvas's box every time it is used.** The layout
  // moves under this test: T-505 put the tier inside every viewport row's level cell, so the row
  // wraps and un-wraps as these gestures change the level, and the canvas slides with it. A point
  // held in page coordinates from before a gesture can land off the pane — and `input.ts`
  // `preventDefault`s every wheel over the canvas BEFORE it decides which viewport the point is
  // over, so a wheel that missed still arrives and still reports `defaultPrevented`. It would read
  // as a gesture the surface refused rather than one the surface never got.
  const midAt = async (fx = 0.5, fy = 0.35) => {
    const r = await page.$rect('[data-slot="canvas"]');
    assert.ok(r && r.w > 100 && r.h > 100, `the canvas has no box to gesture on: ${JSON.stringify(r)}`);
    return { x: r.x + r.w * fx, y: r.y + r.h * fy, rect: r };
  };
  const rect = await page.$rect('[data-slot="canvas"]');
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
  /** The ceiling the client may recover to at each cap sample — its SHARE since T-630, printed
   * beside the cap as `(share N)`. Index-aligned with `caps`; NaN where the line named none. */
  const shares = [];
  const sampleCap = async () => {
    const st = await page.eval(STATUS);
    const m = st.match(/(\d+)\/(\d+) in flight/);
    if (m) { caps.push(Number(m[2])); shares.push(Number(st.match(/in flight \(share (\d+)/)?.[1] ?? NaN)); }
  };

  const moved = [], clamped = [], visited = [];
  /**
   * One gesture, read once **the surface has applied it** rather than 900 ms later.
   *
   * The wait used to be `frames(6)` plus two 450 ms sleeps, and the readout was read at the end of
   * them — so whether this step saw the gesture depended on how many frames the box had got round
   * to, which is a measurement of the machine and not of the page. Beside one bounded worker it
   * lost: *"the drags moved nothing at all, so this proves nothing"*, green alone.
   *
   * `waitUntilStill` settles on the readout **not changing** across real frames, which is the right
   * wait before both shapes of assertion here and cannot manufacture either: a gesture that does
   * nothing settles immediately on the old value and still fails `mustMove`, and one that moves the
   * view is still classified by what it moved to. The cap samples stay two real intervals apart,
   * because the AIMD trace they feed is a sequence and not a snapshot.
   */
  const step = async (what, fn, { mustMove = true } = {}) => {
    const was = await page.eval(`JSON.stringify(${READOUT})`);
    await fn();
    const settled = await page.waitUntilStill(`${what} to be applied and the readout to settle`,
      `JSON.stringify(${READOUT})`, { stable: 3, framesEach: 4, timeoutMs: 15000 });
    await sampleCap();
    await page.frames(8);
    await sampleCap();
    const now = settled.value;
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
  await step("drag-pan", async () => {
    const m = await midAt();
    await page.drag(m, { x: m.x - 260, y: m.y + 120 }, 12);
  });
  await step("jump to the whole surface", () => page.click(BUTTON("Whole surface")));
  await step("wheel zoom in (uniform)", async () => { for (let i = 0; i < 5; i++) await page.wheel(await midAt(), -240); },
    { mustMove: false });
  await step("shift+wheel zoom in (frequency)", async () => { for (let i = 0; i < 5; i++) await page.wheel(await midAt(), -240, { shift: true }); });
  await step("shift+wheel zoom out (frequency)", async () => { for (let i = 0; i < 3; i++) await page.wheel(await midAt(), 240, { shift: true }); });
  await step("wheel zoom out (uniform)", async () => { for (let i = 0; i < 5; i++) await page.wheel(await midAt(), 240); },
    { mustMove: false });
  // The map along the bottom is a viewport too, and it opens showing the whole surface — so
  // *dragging* it is clamped for the same reason the time wheel is. Double-clicking it is not:
  // that is the page's discrete "send the active pane there", which moves the pane across the
  // spectrum in one step and invalidates its whole working set.
  const mapAt = async (fx) => {
    const r = (await midAt()).rect;
    return { x: r.x + r.w * fx, y: r.y + r.h - 18, rect: r };
  };
  await step("drag the whole-surface map", async () => {
    const m = await mapAt(0.5);
    await page.drag(m, { x: m.rect.x + m.rect.w * 0.5 + 180, y: m.y }, 10);
  }, { mustMove: false });
  await step("double-click the map to send the pane there", async () => page.dblclick(await mapAt(0.22)));
  await step("fit back to coverage", () => page.click(BUTTON("Fit to coverage")));
  t.diagnostic(`moved the view: ${moved.join(", ")}`);
  t.diagnostic(`clamped (no movement, expected on this fixture): ${clamped.join(", ") || "none"}`);

  // **The boundary is drawn once the gestures' own requests have been ANSWERED, not 1500 ms after
  // the last one.** Every request still on the wire when the gestures end is one whose `503` would
  // otherwise land inside the steady window and be counted against a window that did not cause it —
  // and how long the route takes to answer four in-flight reads is the same twenty-fold-varying
  // service rate this file already refuses to bet on elsewhere. The route's cap bounds how many
  // there can be, so this is a short wait on a small, named set: requests that started before the
  // mark and have neither a status nor an error.
  await page.frames(10);
  const settlingFrom = Date.now();
  const unanswered = () => page.requests.filter(
    (r) => r.startedMs <= settlingFrom && r.status === null && r.error === null);
  const drained = await until("the gestures' own requests to be answered before the steady window opens",
    async () => unanswered().length === 0, { timeoutMs: 30000, everyMs: 150 })
    .then((ms) => ({ ok: true, ms }), () => ({ ok: false, ms: Date.now() - settlingFrom }));
  t.diagnostic(`the gestures' requests settled after ${drained.ms} ms` +
    (drained.ok ? "" : ` — ${unanswered().length} never came back; opening the steady window anyway`));

  // **…and then until the SERVER says it is done with them, which the wire cannot say** (T-932).
  //
  // "Answered" above is the wire's word, and for an ABORTED read the wire's word is false: the
  // client cancels, CDP records `canceled` at once, and `/api/tiles` goes on producing the tile and
  // holding its slot until it finishes — the fact T-454's abandoned-slot accounting is built on.
  // The client charges that slot for the route's *measured mean* service time; under load a slow
  // overview read outlives the mean by seconds. The release candidate of 2026-09-25 (92d0f6f1, three
  // lanes, load ~25) opened the window with the page's cap already at 1, share 2, and was refused
  // once ~4 s in. At a cap of 1 a read is issued only when the page's own budget has nothing else
  // out, so whatever filled the route was not a read the page was waiting for: the gestures' own
  // abandoned reads, still being made (the only other asker is this file's serial probe).
  // That refusal is a cost of the gestures, counted against a window that did not cause it. Alone,
  // three runs of the same tree were green.
  //
  // So the window opens only once the route **states** it holds no producer slot at all apart from
  // the asker's own: `cost.in_flight` (every slot out, server-wide) minus `cost.in_flight_held` (the
  // probe's own — 0 when it is answered from the hot-tile cache, which holds none). The probe names
  // itself, so "its own" is exactly its own and never the anonymous bucket a leaked `curl`-style read
  // would sit in. Zero is the one reading that cannot hide an abandoned read, whatever the page is
  // doing at that instant, so the page's backlog may keep draining and is still judged by (2) below —
  // this only stops (2) judging what the gestures left running inside the route. Refusals the page
  // takes WHILE this waits are still counted, as the gestures' cost, by (4). Nothing is loosened: the
  // bound, the window's length and the requests it counts are unchanged, and a slot that is NEVER
  // released (the leak itself) now fails here, stated by the server, instead of passing whenever the
  // page happens not to ask for anything.
  //
  // Measured against a scratch `hk` whose reads could be told to hold their slot (T-932's hand-back):
  // with two such reads kept under the page's own client id until 3 s after the last gesture, the
  // file WITHOUT this wait went red at (2) 3 of 3 — "refused 2-3 of 3-6 … cap trace 1 1 1 …", the
  // release candidate's message — and WITH it was green 3 of 3, having waited ~3 s for the route; a
  // slot leaked outright (never released) was green without it and red here, 31 s after the page
  // went idle.
  const probeUrl = `${ORIGIN}/api/tiles?` + new URLSearchParams({
    token: TOKEN, level_f: "0", level_t: "0", f_index: "0", t_index: "0", cells: "8",
  });
  const drainUrl = `${probeUrl}&client=surface-nav-drain-probe`;
  /** What the route holds for anyone but the asker, from its own answer; null if it did not say. */
  const routeHoldsElsewhere = async () => {
    const r = await fetch(drainUrl).catch(() => null);
    if (!r || r.status !== 200) return { status: r?.status ?? 0, others: null };
    const c = (await r.json().catch(() => ({})))?.cost ?? {};
    const others = Number.isFinite(c.in_flight) && Number.isFinite(c.in_flight_held)
      ? c.in_flight - c.in_flight_held : null;
    return { status: r.status, others, inFlight: c.in_flight, held: c.in_flight_held };
  };
  /** The page wants nothing and has nothing on the wire: the only state in which a slot the route
   * still holds cannot be one the page is waiting for. */
  const pageIdle = async () => {
    const st = await page.eval(STATUS);
    const f = st.match(/(\d+)\+(\d+)\/(\d+) in flight/);
    const q = Number(st.match(/queue (\d+)/)?.[1] ?? -1);
    const wire = page.requests.some((r) => r.url.includes("/api/tiles") && !r.url.includes("/api/tiles/events") &&
      r.status === null && r.error === null);
    return !!f && Number(f[1]) === 0 && q === 0 && !wire;
  };
  /**
   * How long the route may go on holding a slot **while the page wants nothing** before that is the
   * leak rather than a slow read. The longest service time this client is built to expect is the
   * map's 5.2 s (`MAX_SERVER_MS` in tilecache.ts); six of them is what a read abandoned at the last
   * gesture gets, and a slot still out after that with nobody asking is one that will not come back.
   * Not a deadline on the page's own backlog — while the page is working this does not run.
   */
  const LEAK_IDLE_MS = 6 * 5200;
  const routeDrain = { probes: 0, maxOthers: 0, idleHeldMs: 0, statuses: new Map() };
  const routeFrom = Date.now();
  let idleHeldSince = null;
  for (;;) {
    const s = await routeHoldsElsewhere();
    routeDrain.probes++;
    routeDrain.statuses.set(s.status, (routeDrain.statuses.get(s.status) ?? 0) + 1);
    if (s.others !== null) routeDrain.maxOthers = Math.max(routeDrain.maxOthers, s.others);
    if (s.others === 0) break;
    // A `503` to one serial read is the route saying it is full, which is the same statement with
    // the count left out; anything else unreadable says nothing either way.
    const holding = s.others !== null ? s.others > 0 : s.status === 503;
    if (holding && await pageIdle()) {
      idleHeldSince ??= Date.now();
      routeDrain.idleHeldMs = Date.now() - idleHeldSince;
      assert.ok(routeDrain.idleHeldMs <= LEAK_IDLE_MS,
        `the route still holds ${s.others ?? "every"} producer slot(s) (${s.others === null
          ? "it refused a single serial read" : `in_flight ${s.inFlight}, the probe's own ${s.held}`}) ` +
        `${(routeDrain.idleHeldMs / 1000).toFixed(1)} s after the page stopped wanting ` +
        "anything — nothing on the wire, nothing queued, nothing in flight. A slot the route holds " +
        "for a read nobody is waiting for, for longer than any read this client expects to take, is " +
        "the leak T-454's abandoned-slot accounting exists to close, stated by the server itself.");
    } else {
      idleHeldSince = null;
    }
    await new Promise((r) => setTimeout(r, 100));
  }
  t.diagnostic(`the route released every slot but the probe's own ${Date.now() - routeFrom} ms after ` +
    `the wire settled (${routeDrain.probes} probe(s), at most ${routeDrain.maxOthers} slot(s) held ` +
    `elsewhere, ${routeDrain.idleHeldMs} ms of it with the page idle; statuses ` +
    `${[...routeDrain.statuses].map(([k, n]) => `${k}×${n}`).join(" ")})`);
  const navigationEnded = Date.now();
  // **Counted per ADDRESS** (T-573): a `GET /api/tiles/batch` answers 200 and carries each
  // address's own 503 inside, so a status-line count would see none of the refusals the client
  // backs off from. [[tileAsks]] expands every request into the addresses it named, each with the
  // status the route gave THAT address; everything below that reasons about refusals, re-asks or
  // turnover is per address for the same reason.
  await page.settleBodies();
  const navigationRefusals = tileAsks(page.requests).filter((a) => a.status === 503).length;

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
  // (`probeUrl` is defined above, beside the drain that shares its address.)
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
  /** The client's own queue depth, off the status line it already prints ("queue N"). */
  const queueDepth = async () => Number((await page.eval(STATUS)).match(/queue (\d+)/)?.[1] ?? -1);
  const queueAtStart = await queueDepth();
  /**
   * The client's own outstanding work, off the status line it already prints:
   * `N+M/L in flight · queue Q` — N issued, M charged for abandoned reads, L the operating cap.
   *
   * **This is the premise that decides what a refusal MEANS** (T-690). `/api/tiles` takes its slot
   * before it does any work, so a full budget refuses the probe below whether the slots are held
   * by reads nobody will ever collect (T-454's leak — a defect) or by reads THIS PAGE IS STILL
   * WAITING FOR (backpressure — correct). Those are opposite findings, and the only thing that
   * tells them apart is whether the client has anything outstanding.
   */
  const outstanding = async () => {
    const st = await page.eval(STATUS);
    const f = st.match(/(\d+)\+(\d+)\/(\d+) in flight/);
    return {
      inflight: Number(f?.[1] ?? -1), abandoned: Number(f?.[2] ?? -1),
      queue: Number(st.match(/queue (\d+)/)?.[1] ?? -1),
    };
  };
  const probes = [];
  const probeFrom = Date.now();
  // Where the steady window's own cap samples start, so an additive increase the GESTURES paid for
  // cannot license a refusal inside this window.
  const capsAtSteadyStart = caps.length;
  for (let i = 0; i < STEADY_STATE_MS / 500; i++) {
    await new Promise((r) => setTimeout(r, 500));
    await sampleCap();
    if (i % 2 === 1) {
      const held = await outstanding();
      const t0 = Date.now();
      const status = await fetch(probeUrl).then((r) => r.status, () => 0);
      // IDLE means the client is holding nothing and wants nothing: no request in flight, no
      // abandoned read still charged, and an empty queue. A refusal HERE is unattributable to
      // this page and is therefore the leak, stated from outside the client.
      probes.push({ status, ms: Date.now() - t0, held, idle: held.inflight === 0 && held.abandoned === 0 && held.queue === 0 });
    }
  }
  const probeTo = Date.now();
  const followingAfter = await page.eval(FOLLOWING);
  const queueAtEnd = await queueDepth();
  const frozen = (f) => f.length > 0 && !f.split(",").includes("true");
  // **The MAP is a viewport too, and since T-505 it is a viewport on its own lattice.** `FOLLOWING`
  // above asks only about `[data-viewport="pane"]`, so the still-view premise has never covered the
  // minimap — and the minimap follows the live edge whatever the panes do. Before T-505 that cost
  // nothing here: the map drew from the same lattice at a coarse level, where one tile is megahertz
  // wide and minutes tall, so it re-asked for nothing inside an 8 s window. It now draws from the
  // OVERVIEW tier, and its requests are a different scheme on a different lattice.
  //
  // So the claim is partitioned by scheme rather than counted in one heap (T-564's move). What the
  // frozen panes draw from is `scheme=view`, and THAT is what must be silent; the map's own
  // `scheme=overview` traffic is legitimate — it is following — and is reported with its premise
  // instead of being counted as the panes' speculation. Lumping them made this assertion say
  // "17 requests, therefore speculation" about a viewport that was doing its job.
  const mapFollowing = await page.eval(
    `[...document.querySelectorAll('.hk-surface-viewport[data-viewport="minimap"]')]` +
    `.map((v) => v.getAttribute('data-following')).join(",")`);
  const schemeOf = (a) => a.scheme;

  // ——— the evidence, gathered and PRINTED before anything is asserted ———
  // A failing guard whose first assertion hides the rest of the picture is a guard people bisect by
  // hand. Everything below is reported, then judged.
  await page.settleBodies();
  const tileReqs = tileAsks(page.requests);
  const refused = tileReqs.filter((a) => a.status === 503);
  const steadyRefusals = refused.filter((r) => r.startedMs >= navigationEnded);
  const steady = tileReqs.filter((r) => r.startedMs >= navigationEnded);
  const steadyRequests = steady.length;
  const steadyByScheme = new Map();
  for (const r of steady) steadyByScheme.set(schemeOf(r), (steadyByScheme.get(schemeOf(r)) ?? 0) + 1);
  // **A request that starts after the gestures is not the same thing as a request the gestures did
  // not want.** The client asks through a queue behind the route's in-flight cap, so the last
  // gesture's own addresses keep going out for as long as that queue takes to drain — measured on
  // this run: `queue 30` at ~1537 ms a tile, which is forty seconds of honest backlog inside an
  // 8 s window. Counting those as speculation is counting the rate limit.
  //
  // So the window is partitioned by what the address IS, not by when it was sent (T-564's move):
  //
  //  - a **first-time** address is the tail of the gestures' own enumeration — wanted, queued
  //    before this window began, and arriving late because the route is slow;
  //  - a **re-ask** is an address this page already put on the wire and is asking for again with
  //    nothing on screen changed. That is the speculation T-471's prefetch ring was made of, and
  //    it is unbounded by construction — its ring re-asked the same addresses every frame, 27-33
  //    of them inside this window, which is exactly what this partition catches and a raw count
  //    could only catch by being lucky about the queue depth.
  //
  // **And "already asked" means already ANSWERED.** A request the client aborted mid-gesture — and
  // it aborts thousands, one per frame the box moved — never came back with anything, so asking
  // for it again is the only way the pane can ever draw it. Counting an unanswered abort as a
  // repeat would make the claim "never retry what you cancelled", which is the opposite of what
  // this file wants: T-454's whole subject is that an abandoned read must be re-driven rather than
  // leaked. So the set is the addresses that came back `200`.
  const askedBefore = new Set(tileReqs
    .filter((r) => r.startedMs < navigationEnded && r.status === 200)
    .map((a) => a.key));
  const steadyDetailReqs = steady.filter((r) => schemeOf(r) === "view");
  const steadyDetail = steadyDetailReqs.length;
  const steadyReasks = steadyDetailReqs.filter((r) => askedBefore.has(r.key));
  const steadyFirstTime = steadyDetail - steadyReasks.length;
  const status = await page.eval(STATUS);
  const backpressure = Number(status.match(/(\d+) backpressure/)?.[1] ?? -1);
  const cancelled = Number(status.match(/(\d+) cancelled/)?.[1] ?? 0);
  const canceledOnWire = page.requests.filter((r) => r.error === "canceled").length;
  const gestures = moved.length + clamped.length;
  const addrPeak = addressPeak(tileReqs);
  t.diagnostic(`peak ${addrPeak}/${limit} tile ADDRESSES outstanding on the wire at once (over ` +
    `${tiles.peak} request(s) — a batch names many)`);
  t.diagnostic(`peak ${tiles.peak}/${limit} in flight · ${tileReqs.length} tile requests · ` +
    `${cancelled} cancelled (client), ${canceledOnWire} aborted on the wire`);
  t.diagnostic(`operating cap over the run: ${caps.join(" ")} (server ceiling ${limit})`);
  t.diagnostic(`the client's share at each sample:  ${shares.join(" ")}`);
  // A NaN share is the wording drift above, not a share: it made this trace NaN on every sample of
  // the 2026-09-23 run while the test stayed green, so it is a failure rather than a diagnostic.
  assert.ok(caps.length > 0 && shares.every(Number.isFinite),
    `the status line's share did not parse at ${shares.filter((x) => !Number.isFinite(x)).length} of ` +
    `${shares.length} cap sample(s) (last line: ${JSON.stringify(await page.eval(STATUS))}) — the parser ` +
    "and preview-main.ts's wording have drifted");
  t.diagnostic(`503s: ${navigationRefusals} during ${gestures} viewport changes, ` +
    `${steadyRefusals.length} during ${(STEADY_STATE_MS / 1000).toFixed(0)} s of steady state ` +
    `(${steadyRequests} requests) · ${backpressure} counted by the client`);
  const probeRefusals = probes.filter((p) => p.status === 503);
  t.diagnostic(`cache over the run: ${status}`);
  t.diagnostic(`steady-state DETAIL requests: ${steadyFirstTime} first-time (the gestures' own ` +
    `backlog draining) + ${steadyReasks.length} re-asked · client queue ${queueAtStart} -> ${queueAtEnd}`);
  t.diagnostic(`steady-state requests by scheme: ` +
    `${[...steadyByScheme].map(([k, n]) => `${k}=${n}`).join(" ") || "none"} · map following [${mapFollowing}]`);
  t.diagnostic(`panes following the live edge across the steady window: [${followingBefore}] -> ` +
    `[${followingAfter}] (${frozen(followingBefore) && frozen(followingAfter)
      ? "all frozen: the still-view claim applies" : "one is live: claim not made"})`);
  const idleProbes = probes.filter((p) => p.idle);
  const idleRefusals = idleProbes.filter((p) => p.status === 503);
  // Tile reads the route COMPLETED for the page during the probe window, counted from CDP's own
  // network log rather than from anything the client says about itself: a `200` whose body
  // finished inside the window. This is the budget turning over, measured from outside.
  const served = tileReqs.filter((r) => r.status === 200 &&
    r.endedMs !== null && r.endedMs >= probeFrom && r.endedMs <= probeTo).length;
  t.diagnostic(`steady-state slot probes: ${probes.length} asked, ` +
    `${probes.filter((p) => p.status === 200).length} answered, ${probeRefusals.length} refused · ` +
    `statuses ${probes.map((p) => p.status).join(" ")} · ${probes.map((p) => `${p.ms}ms`).join(" ")}`);
  t.diagnostic(`  of those, ${idleProbes.length} were taken while the client held NOTHING ` +
    `(0 in flight, 0 abandoned, queue 0) — ${idleRefusals.length} of them refused · ` +
    `client state per probe: ${probes.map((p) => `${p.held.inflight}+${p.held.abandoned}/q${p.held.queue}`).join(" ")}`);
  t.diagnostic(`  route turnover across the probe window: ${served} tile read(s) completed 200 for the page`);
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
  // **…and counted per ADDRESS, because since T-573 that is what the cap is a cap on** (T-846). A
  // `GET /api/tiles/batch` carries up to 64 addresses, the client charges one slot per address and
  // the route takes one producer slot per address — so a client that ignored its cap altogether
  // put every address it wanted into one or two batches, and the request count above read it as
  // `peak 2/4`. Measured with `t454-ignore-the-cap` injected: green on the request count, red here.
  assert.ok(addrPeak <= limit,
    `${addrPeak} tile ADDRESSES were outstanding on the wire at once against a declared cap of ` +
    `${limit}, over only ${tiles.peak} request(s). The route's cap is per address and so is the ` +
    "client's budget: batching changes how many requests carry them, not how many may be asked for.");

  // (2) ONCE CONVERGED, NEVER REFUSED AGAIN. This is the strict half, and it is stricter than the
  // "zero refusals" it replaces is about anything that matters: it forbids the regime the user
  // suffers (refusals arriving indefinitely) while permitting the probe that prevents it. Measured
  // zero over 25 s and ~3 700 requests on the fixed client; before the fix, refusals continued
  // throughout.
  // **BOUNDED BY THE SEARCH THAT PAID FOR IT, not by zero (T-690).** This was
  // `steadyRefusals.length === 0`, and its own premise — "after the AIMD controller has FOUND ITS
  // SHARE" — is the thing that stops being true on a slow route. AIMD only raises on a completion,
  // so with the gestures' backlog still draining (measured here: queue 41 -> 26 at ~1424 ms a
  // tile) the client is still SEARCHING during this window: it probes upward, gets refused once,
  // and halves. The cap trace says so in order — `2×20 3×7 4×3 2×4` — and one refusal is what that
  // costs. Calling it "a slot that was never released" makes a load meter out of the assertion.
  //
  // So it is bounded by the mechanism that licenses it: **an additive increase may cost at most
  // one refusal**, which is AIMD's own contract, counted over THIS window's cap samples. What it
  // still forbids is the regime the user suffers and the one `t454-never-back-off` produces — a
  // client pinned at the ceiling, refused over and over, never raising because it never fell
  // (measured there: 8 refusals against 0 rises). Counts and ordering, never a duration.
  const steadyCaps = caps.slice(capsAtSteadyStart);
  const capRises = steadyCaps.reduce((n, c, i) => n + (i > 0 && c > steadyCaps[i - 1] ? 1 : 0), 0);
  t.diagnostic(`operating cap across the steady window: ${steadyCaps.join(" ")} — ${capRises} additive ` +
    `increase(s), ${steadyRefusals.length} refusal(s) of ${steadyRequests} request(s)`);
  assert.ok(steadyRefusals.length <= capRises,
    `the tile route refused ${steadyRefusals.length} of ${steadyRequests} tile requests during ` +
    `${(STEADY_STATE_MS / 1000).toFixed(0)} s in which NOTHING moved the view, while the client ` +
    `raised its operating cap only ${capRises} time(s) (cap trace over the window: ` +
    `${steadyCaps.join(" ")}). A refusal is permitted only as the cost of an additive increase — ` +
    "that is what makes backpressure a SEARCH. More refusals than increases is backpressure as a " +
    "REGIME: a client pinned at the ceiling, or a slot that was never released — the leak T-454's " +
    "abandoned-slot accounting exists to close.");

  // (2b) …AND THE WINDOW WAS ACTUALLY ASKED. (2) is a claim about a set the page may leave empty —
  // measured empty on this fixture, every run — so on its own it certifies nothing. These two say
  // the window was exercised and by what: the route answered every serial probe made while the page
  // sat idle, so its slot budget was genuinely free. A page that walked away from reads the server
  // is still producing refuses these, which is the leak stated as the user's own experience of it:
  // a second tab, or this page's next gesture, being told the route is full.
  assert.ok(probes.length >= 4,
    `only ${probes.length} slot probes were made during the steady-state window — too few for (2) ` +
    "to be a test rather than a formality");
  // **THE LEAK, STATED WHERE IT IS UNAMBIGUOUS (T-690).** This used to be `probeRefusals == []`
  // — every refusal a leak — and that is a claim about the route's SERVICE RATE, not about its
  // accounting. `/api/tiles` takes its slot before it does any work, so whether a serial probe
  // lands between the page's own reads depends entirely on how long one read takes:
  //
  //     this file with one other spec  · ~167 ms/tile  · queue 0  -> 8 of 8 probes answered
  //     this file in the 13-spec suite · ~3612 ms/tile · queue 28 -> 8 of 8 probes refused
  //
  // and in the second run the client held two slots and wanted twenty-eight more tiles. Those
  // refusals are the route being BUSY WITH WORK THIS PAGE IS WAITING FOR, which is backpressure
  // working, not a slot that was never released. Calling them a leak makes the guard a load
  // meter. So the claim is made where the two cannot be confused: a probe taken while the client
  // holds NOTHING AND WANTS NOTHING can only be refused by a slot nobody is waiting for.
  //
  // On a quiet route every probe is an idle probe and the original bound is recovered exactly.
  assert.deepEqual(idleRefusals.map((p) => p.status), [],
    `the tile route refused ${idleRefusals.length} of ${idleProbes.length} single, serial tile ` +
    "requests made while THE CLIENT HELD NOTHING AT ALL — 0 in flight, 0 abandoned, queue 0 " +
    `(all ${probes.length} probe statuses: ${probes.map((p) => p.status).join(" ")}; client state ` +
    `at each: ${probes.map((p) => `${p.held.inflight}+${p.held.abandoned}/q${p.held.queue}`).join(" ")}). ` +
    "Nothing was asking, so the budget those slots came out of was held by reads this client " +
    "abandoned and the server is still producing — T-454's leak, from outside the client.");
  // …AND THE BUDGET WAS SEEN TO TURN OVER, so the assertion above is never satisfied by a route
  // that answered nobody. A window in which no probe was answered AND no tile read completed for
  // the page is a budget that is simply stuck, whatever the client is holding — the leak's other
  // face, and the one the idle partition cannot see because a stuck route never lets the client
  // reach idle. Counts, not durations.
  const answeredProbes = probes.filter((p) => p.status === 200).length;
  assert.ok(answeredProbes + served > 0,
    `across the whole steady-state window the tile route answered ${answeredProbes} of ` +
    `${probes.length} serial probes AND completed ${served} tile reads for the page: its slot ` +
    "budget never turned over at all. Whoever holds those slots is not giving them back.");
  assert.ok(probes.every((p) => p.status === 200 || p.status === 503),
    `a steady-state slot probe did not get an HTTP answer at all (statuses: ${probes.map((p) => p.status).join(" ")}) — ` +
    "a probe that errors proves nothing either way, so the assertions above would be vacuous");

  // **T-538 re-landed speculation under this bound, and did not weaken it.** The lane that replaced
  // T-471's standing ring (`TileCache.prefetchAhead`) is driven by DISPLACEMENT between frames and
  // asks each address at most once per session, so it cannot produce a re-ask at all and a view
  // that is not moving has no direction to predict. It also issues only while the client holds
  // NOTHING — which on this fixture is never, the gestures' backlog still draining right through
  // the window — so the measured cost of it here is exactly zero: the status line's own
  // `N guessed (M drawn)` read `0 guessed (0 drawn)` over the whole run, and the steady window's
  // re-ask count was 0 with 4-7 first-time addresses, which is the backlog and not a decision.
  //
  // (2c) A FROZEN VIEW THAT NOTHING TOUCHES ASKS FOR NOTHING. The premise is measured at both ends
  // of the window, not assumed: every pane is off the live edge, so nothing new can be wanted, and
  // only then is "it must ask for nothing" the right claim. What this forbids is speculation — and
  // speculation is not free: every speculative read that a later gesture aborts leaves the server
  // producing a tile nobody will read, holding the slot (2) and (2b) are about.
  if (frozen(followingBefore) && frozen(followingAfter)) {
    assert.deepEqual(steadyReasks.map((r) => r.key).slice(0, 5), [],
      `the page RE-ASKED ${steadyReasks.length} DETAIL-tier addresses it had already been ANSWERED ` +
      "for, " +
      `during ${(STEADY_STATE_MS / 1000).toFixed(0)} s in which nothing moved the view and no pane ` +
      `was following the live edge (queue ${queueAtStart} -> ${queueAtEnd}; scheme=view ` +
      `${steadyFirstTime} first-time + ${steadyReasks.length} re-asked; all schemes: ` +
      `${[...steadyByScheme].map(([k, n]) => `${k}=${n}`).join(" ") || "none"}; the map's own ` +
      `following state: [${mapFollowing}]). With nothing new on screen to draw, asking a second ` +
      "time for what you already asked for is speculation — and speculation a later gesture aborts " +
      "is a server slot spent on a read nobody will ever look at (T-471). First-time addresses are " +
      "NOT counted here: they are the last gesture's own enumeration still draining through the " +
      "route's in-flight cap, which is the rate limit and not a decision. Nor is the map's " +
      "overview-tier traffic: the map follows the live edge whatever the panes do, and since T-505 " +
      "it draws from its own lattice (T-564: partition by kind, do not count one heap).");
    assert.ok(queueAtEnd <= queueAtStart,
      `the client's tile queue GREW from ${queueAtStart} to ${queueAtEnd} across ` +
      `${(STEADY_STATE_MS / 1000).toFixed(0)} s in which nothing moved the view. A frozen surface ` +
      "may drain a backlog; it may not accumulate one, and a queue that grows with no input is the " +
      "same defect the re-ask assertion above is about, spelled as depth instead of as repetition.");
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
// T-454's CONTROLLER, AGAINST A REFUSAL THIS TEST KNOWS HAPPENED (T-846)
// ———————————————————————————————————————————————————————————————————————————————————————————————
//
// Assertion (3) above — "a refusal must halve the cap" — is conditional on the route refusing, and
// since T-573 it does not, on this fixture: a `/api/tiles/batch` answers its members on at most the
// asker's share of workers and retires a worker on a 503 rather than recording one, and T-630's
// share clamps the client's ceiling on every answer. Measured over the gestures above: 0 refusals,
// the operating cap pinned at 2 by a transient share of 2 — so `t454-never-back-off` (the halving
// deleted) came back GREEN, because there was nothing to back off from and the cap was already
// below the server's number for another reason.
//
// So the refusal is put on the wire deliberately, in exactly the batch-era shape: a 200 whose one
// member carries its own `503` and the route's own words, which is how `hk-api` answers a batch
// member it cannot admit. The injection sits in `fetch`, before the page's scripts, like T-523's
// proxy in `live-edge.e2e.mjs`. It rewrites ONE member of ONE batch; everything else is the real
// route. The claim is then the controller's own contract, read off the status line the page draws:
// the operating cap in the first readout that counts the refusal is BELOW the one before it. A
// client that merely counts refusals keeps its cap (measured with `t454-never-back-off`: 3 -> 3,
// share 4); the controller halves it (measured: 4 -> 2, share 4).
test("a per-address 503 inside a batch answer halves the operating cap: back-off is a controller, not a counter (T-454, T-846)", async (t) => {
  const limit = Number(process.env.HK_E2E_TILE_LIMIT);
  assert.ok(Number.isInteger(limit) && limit > 0, `the server did not declare cost.in_flight_limit (got "${process.env.HK_E2E_TILE_LIMIT}")`);
  const inject = `(() => {
    const real = window.fetch.bind(window);
    window.__t846 = { arm: 0, refused: [] };
    window.fetch = async (input, init) => {
      const url = typeof input === "string" ? input : input.url;
      const r = await real(input, init);
      if (!(window.__t846.arm > 0 && url.includes("/api/tiles/batch") && r.status === 200)) return r;
      const body = await r.json();
      const e = (body.tiles ?? []).find((x) => x.status === 200);
      if (e) {
        window.__t846.arm--;
        window.__t846.refused.push(e.address?.spelling ?? "?");
        e.status = 503;
        delete e.tile;
        e.error = "too many tile reads in flight (limit ${limit}, share ${limit}): injected by ui/e2e/surface-nav.e2e.mjs (T-846)";
      }
      return new Response(JSON.stringify(body), { status: 200, headers: { "content-type": "application/json" } });
    };
  })();`;
  const browser = await Browser.open();
  t.after(() => browser.close());
  const page = await browser.page(undefined, { initScript: inject });
  await page.goto(`${ORIGIN}/surface.html#token=${TOKEN}`);
  await page.waitForSurfaceMounted();
  await page.waitFor("the first tile textures to be uploaded",
    `(${STATUS}.match(/(\\d+) uploads/)?.[1] | 0) > 0`, { timeoutMs: 90000 });

  /** The controller's state as the page prints it: `N+M/L in flight (share C)` and `B backpressure`. */
  const read = (st) => {
    const f = st.match(/(\d+)\+(\d+)\/(\d+) in flight \(share (\d+)/);
    return f ? { inflight: Number(f[1]), cap: Number(f[3]), share: Number(f[4]),
      busy: Number(st.match(/(\d+) backpressure/)?.[1] ?? NaN), queue: Number(st.match(/queue (\d+)/)?.[1] ?? NaN) } : null;
  };
  // The status line must parse BEFORE the premise is waited for: an unreadable line would time the
  // wait out and report a controller state the page may well be in (2026-09-23, `assertReadable`).
  assertReadable(read, await page.eval(STATUS));
  // The premise: a cap that CAN halve. At 1 the halving is `max(1, …)` and a controller is
  // indistinguishable from a counter, so the test waits for the cap to be at least 2 with the page
  // quiet, rather than assuming where the opening fill left it.
  const ready = await page.waitForValue("the operating cap to be at least 2 with nothing outstanding", STATUS,
    (st) => { const c = read(st); return !!c && c.cap >= 2 && c.inflight === 0 && c.queue === 0; }, { timeoutMs: 60000 });
  t.diagnostic(`before the injected refusal: ${JSON.stringify(read(ready.value))}`);
  assert.ok(ready.ok, `the operating cap never reached 2 with the page quiet (last: ${JSON.stringify(assertReadable(read, ready.value))}), ` +
    "so a halving could not be told from no halving and this test would assert nothing");

  // Arm, then ask for tiles the page does not hold: a frequency zoom IN over the opening view, which
  // is observed spectrum (the page opens on the coverage map's observed extent) at a finer level
  // than anything cached — so T-580's survey cannot answer it and a batch must go out.
  const trace = [];
  let last = read(ready.value);
  await page.eval("window.__t846.arm = 1");
  const r = await page.$rect('[data-slot="canvas"]');
  const at = { x: r.x + r.w * 0.5, y: r.y + r.h * 0.35 };
  let hit = null;
  for (let i = 0; i < 6 && !hit; i++) {
    await page.wheel(at, -240, { shift: true });
    const t0 = Date.now();
    while (!hit && Date.now() - t0 < 8000) {
      const c = read(await page.eval(STATUS));
      if (c) {
        trace.push(`${c.cap}/${c.share}·${c.busy}`);
        if (c.busy > last.busy) hit = { before: last, after: c };
        else last = c;
      }
      await new Promise((res) => setTimeout(res, 100));
    }
  }
  const refused = await page.eval("window.__t846.refused.slice()");
  t.diagnostic(`injected a per-address 503 for ${JSON.stringify(refused)}; cap/share·backpressure readouts: ${trace.join(" ")}`);
  assert.ok(refused.length > 0, "no batch went out for the zoom, so no refusal was injected and this test asserts nothing");
  assert.ok(hit, `the client never counted the injected refusal (readouts: ${trace.join(" ")})`);
  t.diagnostic(`the readout that first counts it: ${JSON.stringify(hit.after)} (the one before: ${JSON.stringify(hit.before)})`);
  // Strictly LOWER, and nothing more exact: the readout is a 500 ms sample, so the cap it shows as
  // "before" may have moved by an additive step since. What no correct client can do is come out of
  // a refusal at or above where it went in — `max(1, min(⌊cap/2⌋, ceiling))` is below any cap ≥ 2.
  // A fall in the SHARE at the same instant would lower even a counter's cap (the insert clamp), so
  // it is reported when it happens rather than assumed not to.
  if (hit.after.share < hit.before.cap) {
    t.diagnostic(`the share fell to ${hit.after.share} in the same readout, so the clamp alone could explain ` +
      "a lower cap this run: the fault this test exists for may pass it once in such a run, a correct client never fails it");
  }
  assert.ok(hit.after.cap < hit.before.cap,
    `the route refused a member of a batch and the client's operating cap went from ${hit.before.cap} to ` +
    `${hit.after.cap} with its share at ${hit.after.share}. A 503 must HALVE it (AIMD's multiplicative ` +
    "decrease): a client that counts the refusal and keeps its cap has not discovered anything, and it is " +
    "refused again on its very next burst, for as long as the user keeps navigating — T-454's regime.");
  assert.deepEqual(page.exceptions, [], "uncaught exception while the refusal was handled");
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
  // **Retries the route's backpressure** (T-690). `/api/tiles` takes its slot before it does any
  // work and answers `503` over `cost.in_flight_limit`; the earlier tests in this file leave the
  // shared route busy, so a bare `fetch` here manufactures the refusal and then reads it as a
  // broken server. Observed exactly that: this premise threw `GET /api/tiles → 503` 182 ms into
  // the test, before its own 120 s wait for the record to grow had a chance to run once. A `503`
  // is "busy now", never "no" — the same rule the product's own bootstrap follows.
  const get = async (p, { tries = 40, waitMs = 200 } = {}) => {
    for (let i = 0; ; i++) {
      const r = await fetch(`${ORIGIN}${p}${p.includes("?") ? "&" : "?"}token=${TOKEN}`);
      if (r.ok) return r.json();
      if (r.status !== 503 || i >= tries) {
        throw new Error(`GET ${p} → ${r.status}` +
          (r.status === 503 ? ` after ${i} retries of the route's backpressure` : ""));
      }
      await new Promise((res) => setTimeout(res, waitMs));
    }
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

/** The pane row of the chrome — not the map, which is a viewport too and moves for other reasons.
 * `time` is the chrome's offset-from-the-edge LABEL, which rounds to whole seconds past 10 s and
 * states only the top edge; `t0Ns`/`t1Ns` are the pane's time window itself, unrounded, from the
 * row's `data-t0-ns` / `data-t1-ns`. */
const PANE_ROW = `(() => { const v = document.querySelector('.hk-surface-viewport[data-viewport="pane"]');
  if (!v) return null;
  const where = v.querySelector('.hk-surface-where')?.textContent ?? '';
  const level = v.querySelector('.hk-surface-level')?.textContent ?? '';
  const parts = where.split(' · ');
  return { freq: parts[0] ?? '', time: parts[1] ?? '', level,
           t0Ns: Number(v.getAttribute('data-t0-ns')), t1Ns: Number(v.getAttribute('data-t1-ns')) };
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

/**
 * **The premise every gesture test in this file needs, and the one that made two of them fail
 * ALONE.**
 *
 * `SurfacePreview` resolves the surface's bounds ONCE, at load, so how much room the time axis has
 * is decided by how long the backend has been recording when the page opens — and each of these
 * tests opens its own page. Measured 2026-09-22: run alone, this file met a ~40 s record against a
 * 16 s zoom floor (2.5 floors); run after another spec, 120-149 s (7.5-9.3 floors). At 2.5 floors
 * the axis is close enough to clamped in both directions that a zoom cannot be seen to move it, and
 * T-472 and T-486 went red **alone** and green in company — the exact inverse of a load flake, and
 * the reason the merge runner's "re-run the failing spec alone" triage manufactured a red for this
 * file rather than clearing one.
 *
 * T-456 has waited for this since it was written. The wait belongs to all three, so it lives here:
 * asked of the two routes that state the extent and the floor, bounded generously, and asserted
 * rather than assumed — a fixture that never grows a usable axis fails as a missing premise instead
 * of as a welded one.
 */
async function waitForTimeRoom(t) {
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
  return room;
}

test("T-456: drag pans, a plain wheel zooms BOTH axes, and the modifiers reach the page", async (t) => {
  // ——— the premise, measured and waited for ———
  await waitForTimeRoom(t);

  const browser = await Browser.open();
  t.after(() => browser.close());
  const page = await browser.page();
  await page.goto(`${ORIGIN}/surface.html#token=${TOKEN}`);
  await page.waitForSurfaceMounted();
  await page.waitFor("the first tile textures to be uploaded",
    `(${STATUS}.match(/(\\d+) uploads/)?.[1] | 0) > 0`, { timeoutMs: 90000 });
  assert.equal(await page.eval(INSTALL_PROBE), true);

  /**
   * The gesture point, **re-derived from the canvas's box every time it is used**.
   *
   * A point taken once at the top of a test is in page coordinates of the layout that existed then,
   * and this page's layout moves: T-505 put the tier inside every viewport row's level cell, so
   * that row wraps and un-wraps as a gesture changes the level and the canvas slides with it. A
   * wheel delivered at a stale point can land off the pane entirely — and `input.ts`
   * `preventDefault`s every wheel over the canvas *before* it decides which viewport the point is
   * over, so such a wheel still arrives at `window` and still reports `defaultPrevented`. The
   * delivery checks below therefore cannot see it, and the gesture reads as one the surface refused
   * rather than one the surface never got.
   */
  const midAt = async (fx = 0.5, fy = 0.35) => {
    const r = await page.$rect('[data-slot="canvas"]');
    assert.ok(r && r.w > 100 && r.h > 100, `the canvas has no box to gesture on: ${JSON.stringify(r)}`);
    return { x: r.x + r.w * fx, y: r.y + r.h * fy };
  };
  const rect = await page.$rect('[data-slot="canvas"]');
  const mid = { x: rect.x + rect.w / 2, y: rect.y + rect.h * 0.35 };
  const read = async () => page.eval(PANE_ROW);
  /** Wait for the surface to APPLY whatever was just dispatched, then read the settled row. */
  const settled = async (what) => {
    const r = await page.waitUntilStill(what, `JSON.stringify(${PANE_ROW})`,
      { stable: 3, framesEach: 4, timeoutMs: 15000 });
    return JSON.parse(r.value);
  };
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
    await settled("Whole surface to be applied");
    for (let i = 0; i < inSteps; i++) await page.wheel(await midAt(), -240);
    if (inSteps) await settled("the reset zoom to be applied");
    await page.eval("window.__wheels = []");
    return read();
  };

  /** One gesture, from a stated starting window: what the view did, and what the browser delivered. */
  const gesture = async (what, run, { inSteps = 0 } = {}) => {
    const was = await reset(inSteps);
    await run();
    // The readout is read once the surface has applied the gesture, never after a frame count: how
    // many frames a wheel or a drag takes to reach the view is the box's business, and reading too
    // early reports a gesture that arrived as one that did nothing.
    const now = await settled(`${what} to be applied and the readout to settle`);
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
  const drag = await gesture("drag (diagonal)", async () => {
    const m = await midAt();
    await page.drag(m, { x: m.x - 240, y: m.y + 140 }, 12);
  }, { inSteps: 2 });
  assert.ok(drag.freqMoved, "a drag must pan the FREQUENCY axis");
  assert.ok(drag.timeMoved, "a drag must pan the TIME axis: both axes, in one gesture");
  assert.deepEqual(levels(drag.now), levels(drag.was), "a pan must not change either pyramid level");

  // ——— 2. PLAIN WHEEL: UNIFORM ZOOM, BOTH AXES ———
  const plain = await gesture("plain wheel", async () => {
    for (let i = 0; i < 2; i++) await page.wheel(await midAt(), -240);
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
    for (let i = 0; i < 2; i++) await page.wheel(await midAt(), -240, { shift: true });
  });
  assert.ok(shift.wheels.length > 0 && shift.wheels.every((w) => w.shift),
    `the browser did not deliver shiftKey on the wheel: ${JSON.stringify(shift.wheels)}`);
  assert.ok(shift.freqMoved, "shift + wheel must zoom frequency");
  assert.equal(shift.timeMoved, false,
    `shift + wheel moved the TIME axis (${shift.was.time} → ${shift.now.time}): the axes are welded`);

  // ——— 4. ALT / OPTION + WHEEL: TIME ONLY ———
  // The binding the whole ticket turns on, and the one a unit test cannot speak for.
  const alt = await gesture("alt + wheel", async () => {
    for (let i = 0; i < 2; i++) await page.wheel(await midAt(), -240, { alt: true });
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
    for (let i = 0; i < 2; i++) await page.wheel(await midAt(), -240, { ctrl: true });
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
  // **Wait for the page to SAY it is drawn, not for a constant.** This was a 1200 ms sleep, which
  // was long enough when a level-0 tile was 1.6 MHz x 256 s. Since T-501 the finest tier is the
  // display plan's own bin and row, so the fitted viewport is cut into several times as many tiles
  // and 1200 ms lands mid-fill: measured here at 92.5 % dominant against a 92 % bound — the census
  // was reading how far the fetch had got, not whether the renderer draws. The pane reports its own
  // residency in the chrome, so that is what the wait is on. A pane that never becomes resident is
  // a real defect and still fails, now with the counts saying so.
  const paneCounts = `(document.querySelector('.hk-surface-viewport[data-viewport="pane"] .hk-surface-counts')?.textContent ?? '')`;
  //
  // **Stand-ins count as drawn, and deliberately.** A coarse stand-in is a real ancestor tile with
  // real cells in it — the surface's own way of showing something honest while the finer answer is
  // in flight — so a pane drawn with six of them is drawing, which is the only thing the census
  // below is a question about. What must be zero is `pending`: that is the pane's own statement
  // that some of what is on screen is its bare ground.
  const filled = await page.waitFor("the fitted pane to report itself drawn (0 pending)",
    `/· 0 pending/.test(${paneCounts}) && !/^0 tiles/.test(${paneCounts})`,
    { timeoutMs: 30000 }).then(() => true, () => false);
  const counts = await page.eval(paneCounts);
  t.diagnostic(`after "Fit to coverage" the pane reports: ${counts}${filled ? "" : " (NEVER became resident)"}`);
  assert.ok(filled, `the fitted pane never finished drawing: ${counts}. A census over a pane that is ` +
    "still fetching measures the tile route, not the renderer.");
  // **And the rectangle is re-read here.** `rect` was taken before the gestures, and the chrome's
  // height is not constant across them — T-505 puts the tier inside every viewport row's level
  // cell, so a row wraps and un-wraps as the level changes and the canvas moves with it. Sampling
  // the stale box reads the page around the canvas, which is a flat fill by construction.
  const shotRect = await page.$rect('[data-slot="canvas"]');
  const shots = await page.shot(path.join(ART, "surface-gestures.png"));
  const c = census(shots, { x: Math.round(shotRect.x), y: Math.round(shotRect.y), w: Math.round(shotRect.w), h: Math.round(shotRect.h) });
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
  // **Before the browser, because the page reads the bounds once at load** — see
  // [[waitForTimeRoom]]. Run alone, this test met a record barely wider than its own zoom floor and
  // reported the result as a welded axis.
  await waitForTimeRoom(t);
  const browser = await Browser.open();
  t.after(() => browser.close());
  const page = await browser.page();
  await page.goto(`${ORIGIN}/surface.html#token=${TOKEN}`);
  await page.waitForSurfaceMounted();
  await page.waitFor("the first tile textures to be uploaded",
    `(${STATUS}.match(/(\\d+) uploads/)?.[1] | 0) > 0`, { timeoutMs: 90000 });
  assert.equal(await page.eval(INSTALL_PROBE), true);

  /**
   * The gesture point, **re-derived from the canvas's box for every wheel**.
   *
   * Off-centre on both axes, so the two anchors differ and a gesture that fed one to both would
   * show. Re-derived rather than taken once, because the layout moves while this test runs: T-505
   * put the tier inside every viewport row's level cell, and this test drives the level from 11 to
   * 8 and the frequency span from megahertz to megahertz, so the row wraps and un-wraps and the
   * canvas slides with it.
   *
   * That matters here more than anywhere else in this file, because of what `input.ts` does in what
   * order: it `preventDefault`s **every** wheel over the canvas before it decides which viewport
   * the point is over, and `SurfacePreview.wheel` returns silently when the point is over no pane.
   * So a wheel delivered at a stale point arrives at `window`, is recorded by the probe, reports
   * `defaultPrevented: true` — and moves nothing. Every "did not move the time axis" check below
   * would read that as the surface refusing the gesture, which is the opposite finding, and is what
   * *"4 alt wheels outward did not move the time axis (−21 s): it is welded shut"* looked like in
   * the gate's pooled run and never looked like alone.
   */
  const atNow = async () => {
    const r = await page.$rect('[data-slot="canvas"]');
    assert.ok(r && r.w > 100 && r.h > 100, `the canvas has no box to gesture on: ${JSON.stringify(r)}`);
    return { x: r.x + r.w * 0.42, y: r.y + r.h * 0.35 };
  };
  const read = async () => page.eval(PANE_ROW);
  /**
   * Wait for the surface to APPLY what was just dispatched, and hand back the settled row.
   *
   * Never a frame count: how many frames a wheel takes to reach the view is a fact about the box.
   * Settling on "the readout stopped changing" is the right wait before both shapes of assertion in
   * this test and can manufacture neither — a wheel the surface clamps settles at once on the old
   * value and still fails a `notEqual`, and one that moves the view is still judged on where it
   * landed. This preview holds a fixed edge (see the header), so a settled readout here is view
   * state and not lag.
   */
  const settle = async (what) => {
    const r = await page.waitUntilStill(what, `JSON.stringify(${PANE_ROW})`,
      { stable: 3, framesEach: 4, timeoutMs: 15000 });
    return JSON.parse(r.value);
  };
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

  /** One wheel, and everything the readout says once the surface has applied it. */
  const wheel = async (mods = {}, deltaY = -240) => {
    const was = await read();
    await page.eval("window.__wheels = []");
    await page.wheel(await atNow(), deltaY, mods);
    const now = await settle("the surface to apply the wheel");
    const wheels = JSON.parse(await page.eval("JSON.stringify(window.__wheels)"));
    return { was, now, wheels, moved: !same(was, now) };
  };

  /** `PROBE_STEPS` wheels of one kind, reported as one move — see [[PROBE_STEPS]]. */
  const probe = async (mods, deltaY) => {
    const was = await read();
    await page.eval("window.__wheels = []");
    for (let i = 0; i < PROBE_STEPS; i++) await page.wheel(await atNow(), deltaY, mods);
    const now = await settle(`the surface to apply ${PROBE_STEPS} ${JSON.stringify(mods)} wheels`);
    const wheels = JSON.parse(await page.eval("JSON.stringify(window.__wheels)"));
    assert.equal(wheels.length, PROBE_STEPS,
      `the browser delivered ${wheels.length} of ${PROBE_STEPS} wheels for ${JSON.stringify(mods)} — ` +
      "a gesture that did not arrive cannot be evidence that it moved nothing");
    assert.ok(wheels.every((w) => w.prevented), "a wheel over the canvas was not preventDefaulted");
    return { was, now, wheels };
  };

  // ——— out to the whole surface, then wheel IN until the uniform gesture stops ———
  await page.click(BUTTON("Whole surface"));
  t.diagnostic(`whole surface: ${show(await settle("Whole surface to be applied"))}`);

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
  t.diagnostic(`alt + wheel inward at the lock: time window [${held.t0Ns}, ${held.t1Ns}] → ` +
    `[${altIn.now.t0Ns}, ${altIn.now.t1Ns}] ns`);

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
  //
  // **Judged on the time WINDOW, not on the label** (the 09-22 gate red). The label is the pane's top
  // edge as an offset from the live edge, rounded to whole seconds past 10 s — and an outward zoom
  // anchored 35 % down a pane at the time floor moves that top by about a second, so the same real
  // move read `−24 s → −23 s` on one run and `−21 s → −21 s` on another, depending only on where the
  // fraction fell. What alt+wheel outward must do is WIDEN the time span; that is read from the
  // pane's own window, exactly, and a welded axis leaves it bit-for-bit unchanged.
  const base = await read();
  const altOut = await probe({ alt: true }, 240);
  const spanS = (r) => (r.t1Ns - r.t0Ns) / 1e9;
  assert.ok(Number.isFinite(spanS(base)) && spanS(base) > 0 && Number.isFinite(spanS(altOut.now)),
    `the pane row does not state its time window (data-t0-ns/data-t1-ns): ${JSON.stringify(base)}`);
  t.diagnostic(`alt + wheel outward: time span ${spanS(base).toFixed(3)} s → ${spanS(altOut.now).toFixed(3)} s ` +
    `(x${(spanS(altOut.now) / spanS(base)).toFixed(2)}), label ${base.time} → ${altOut.now.time}, frequency ${altOut.now.freq}`);
  assert.ok(spanS(altOut.now) > spanS(base),
    `${PROBE_STEPS} alt wheels outward did not widen the time axis (span ${spanS(base)} s → ` +
    `${spanS(altOut.now)} s, label ${base.time}): it is welded shut`);
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
  // The same premise, for the same reason ([[waitForTimeRoom]]): this test zooms the time axis in
  // and then scrubs 160 px along it, and neither is a gesture on a record with no room in it.
  await waitForTimeRoom(t);
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

  /** The pane's own offset behind the live edge, in seconds, off the readout it already prints. */
  const LAG_S = `(() => {
    const v = document.querySelector('.hk-surface-viewport[data-viewport="pane"]');
    const where = v?.querySelector('.hk-surface-where')?.textContent ?? '';
    const label = (where.split(' \u00b7 ')[1] ?? '').trim();
    if (label === 'LIVE') return 0;
    // Parsed the way paneSpanBoundS parses the ruler, and deliberately NOT anchored on the sign:
    // the label's leading minus is a typographic one, and a regex that assumed which codepoint it
    // was reported every reading as unreadable while the number was right there.
    const mm = /([\\d.]+) m ([\\d.]+) s/.exec(label);
    if (mm) return Number(mm[1]) * 60 + Number(mm[2]);
    const m = /([\\d.]+) (ms|s|h)/.exec(label);
    if (!m) return null;
    return Number(m[1]) * { ms: 1e-3, s: 1, h: 3600 }[m[2]];
  })()`;

  /**
   * **A drag back to the edge whose RELEASE is taken from the pane's own position, not from a step
   * count** (the deflake, 2026-09-22).
   *
   * `settleTime` re-follows only when the release lands inside the snap-back zone — `snapPx` device
   * pixels of the pane's own height, a fraction of a second of capture time at this zoom. But the
   * two sides of that comparison come from two different clocks: the window was clamped against the
   * edge the pane model knew on its **last rendered frame**, and the release is measured against
   * the edge the **stream** has reached by the time the pointer lifts. The residual lag at release
   * is therefore exactly "how long since this page last rendered" — microseconds on a quiet box,
   * hundreds of milliseconds beside three other lanes and a bounded worker, where this failed as
   * *"a drag released at the live edge left the pane frozen"* and passed alone every time.
   *
   * So the pointer is walked a frame at a time, and then nudged a pixel at a time until the pane
   * says it has come to rest AT the edge — and released immediately after that frame, so what is
   * released against is what was last drawn. The claim is untouched: a client that does not snap
   * back never re-follows, whatever the lag at release was, and still fails below with it printed.
   */
  const dragBackToEdge = async (dy, steps, { restS = 0.05, nudges = 40 } = {}) => {
    let y = mid.y;
    await page.mouse("mousePressed", mid.x, y, { buttons: 1, clickCount: 1 });
    for (let i = 1; i <= steps; i++) {
      y = mid.y + (dy * i) / steps;
      await page.mouse("mouseMoved", mid.x, y, { buttons: 1 });
      await page.frames(1);
    }
    let lag = await page.eval(LAG_S), used = 0;
    for (; used < nudges && (lag === null || lag > restS); used++) {
      y += 1;
      await page.mouse("mouseMoved", mid.x, y, { buttons: 1 });
      await page.frames(1);
      lag = await page.eval(LAG_S);
    }
    await page.mouse("mouseReleased", mid.x, y, { buttons: 0, clickCount: 1 });
    await page.frames(6);
    return { lag, used, following: await follow() };
  };

  // 1. THE REPORTED DEFECT. One pixel, straight up the time axis — a twitch, not a scrub.
  assert.deepEqual(await step("a 1 px time-pan", -1, 1), ["true"],
    "a 1 px time-pan dropped the pane out of live: this is the dead zone the user asked for, twice");

  // 2. THE CONTROL, and the other half of the rule: dragged well beyond the zone it commits to pause.
  assert.deepEqual(await step("a 160 px scrub", -160, 12), ["false"],
    "a real scrub did not pause the pane — so step 1 proves nothing, because the attribute never moves");

  // 3. AND BACK. A drag hard toward the edge clamps against it, and the release is a return to live
  //    rather than a pause a few rows short of it — with rows appending under the cursor throughout.
  const back = await dragBackToEdge(420, 14);
  t.diagnostic(`a drag back to the live edge (420 px, then ${back.used} one-pixel nudge(s)): the pane ` +
    `reported itself ${back.lag === null ? "at an unreadable offset" : `${(back.lag * 1000).toFixed(0)} ms`} ` +
    `behind the edge at the release · data-following = ${JSON.stringify(back.following)}`);
  assert.deepEqual(back.following, ["true"],
    "a drag released at the live edge left the pane frozen — the second half of the report " +
    `(the pane put itself ${back.lag === null ? "?" : (back.lag * 1000).toFixed(0)} ms behind the edge ` +
    `when the pointer lifted, after ${back.used} nudge(s))`);
});
