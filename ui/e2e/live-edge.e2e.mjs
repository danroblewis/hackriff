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
import { Browser, census, tileAsks, waitWhileWorking } from "./harness.mjs";
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

/**
 * **The pane's own residency report**, from `.hk-surface-counts` — `${tiles} tiles · ${fallbacks}
 * coarse stand-ins · ${pending} pending`.
 *
 * This is `PaneReport`: what the renderer actually drew the frame with, not a second calculation
 * that could disagree with the pixels. `canvas-journey.e2e.mjs` gates every grey measurement on the
 * same instrument, for the same reason, and the two copies are four lines of DOM reading rather than
 * a shared claim — the thing that must not be written twice is the *rule*, and there is none here.
 *
 * T-523 wants it for what it says about a **refused** place rather than a slow one: `TileCache`
 * deliberately answers `pending` for a place it has marked terminal (never grey, `tilecache.ts`),
 * so a place a 502 wrongly made terminal is a `pending` that never clears, however long anyone
 * waits and whatever the pixels happen to show over the coarse ancestor stretched across it.
 *
 * It **reports** rather than throws when the pane will not converge, so the assertion that follows
 * can describe the finding.
 *
 * **What it is bounded BY** (the deflake, 2026-09-22). It used to be bounded by a fixed 25 s / 40 s,
 * and that is a bet on the tile route's service rate — measured in this repo at 167 ms a tile
 * beside one other spec and 3612 ms a tile in the full suite. Under the gate's pooled lanes it lost
 * that bet twice with the product working: *"the pane never became resident with a healthy tunnel
 * (7 tiles · 21 coarse stand-ins · 0 pending)"*, green alone every time, which is a pane that was
 * still filling being reported as one that had stopped. It is now bounded by whether the page is
 * still WORKING — its own report changing, or its requests on the wire — which is exactly the
 * distinction the assertions below turn on, and which makes the T-523 defect (a steady `0 tiles ·
 * N coarse stand-ins · 0 pending` with nothing asked for again) report FASTER than the deadline did.
 */
async function waitForResident(page, { timeoutMs = 120000, everyMs = 400, stallMs = 12000 } = {}) {
  const COUNTS = `(() => { const v = document.querySelector('.hk-surface-viewport[data-viewport="pane"]');
    return v ? (v.querySelector('.hk-surface-counts')?.textContent ?? '') : ''; })()`;
  const r = await waitWhileWorking(page, () => page.eval(COUNTS), (counts) => {
    const m = /(\d+) tiles · (\d+) coarse stand-in\S* · (\d+) pending/.exec(counts);
    return !!m && Number(m[1]) > 0 && Number(m[2]) === 0 && Number(m[3]) === 0;
  }, { everyMs, stallMs, timeoutMs });
  return { resident: r.ok, counts: r.value, ms: r.ms, stalledMs: r.stalledMs };
}

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
  // Diagnostics, NOT the claim: a tile being re-fetched is not a row appearing on screen. They run
  // BEFORE the assertion, because a diagnostic that only appears when the claim PASSES is useless
  // exactly when it is needed — T-482 had to reorder this to find out which addresses a failing run
  // was asking for. The claim itself is unchanged.
  // Per ADDRESS (T-573): one batch request names many, and its URL carries no `level_f` of its own —
  // read per request, every batched ask landed in one "null/null" bucket.
  await page.settleBodies();
  const asked = tileAsks(page.requests);
  const repeats = new Map();
  for (const r of asked) repeats.set(r.key, (repeats.get(r.key) ?? 0) + 1);
  const top = [...repeats.values()].sort((a, b) => b - a)[0] ?? 0;
  t.diagnostic(`${asked.length} tile requests, the most-asked address ${top} times, peak ${tiles.peak} in flight`);
  // **Which addresses, by level, with how long each took on the wire.** A live pane that stopped
  // drawing because the client stopped asking, and one starved by the client spending its budget
  // elsewhere, are identical in pixels and different here. The latency column separates the two ways
  // the second can happen: a client rate-limited by the *cost* of the last revalidation starves the
  // cheap level while the cheap level's own latency stays flat; a server whose history lock is held
  // by an expensive read makes the cheap level slow too. T-482 needed exactly this pair of numbers —
  // level 0 went x68 @69 ms to x14 @183 ms when a coarse viewport came back to life beside it.
  const byLevel = new Map();
  for (const r of asked) {
    const k = `${r.levelF}/${r.levelT}`;
    byLevel.set(k, (byLevel.get(k) ?? 0) + 1);
  }
  const msByLevel = new Map();
  for (const r of asked) {
    const k = `${r.levelF}/${r.levelT}`;
    if (r.endedMs && r.startedMs) (msByLevel.get(k) ?? msByLevel.set(k, []).get(k)).push(r.endedMs - r.startedMs);
  }
  const med = (a) => (a.length ? [...a].sort((x, y) => x - y)[a.length >> 1] : NaN);
  t.diagnostic(`levels asked (level_f/level_t, count, median ms on the wire): ${[...byLevel]
    .sort((a, b) => b[1] - a[1])
    .map(([k, n]) => `${k} x${n} @${med(msByLevel.get(k) ?? []).toFixed?.(0) ?? "?"}ms`).join("   ")}`);

  // **The longest any address went between successive answers, PER LEVEL.** T-491: whether the
  // `t+8 s` sample is a real render is decided by whether it lands inside such a hole, and the two
  // causes look identical in pixels — a lane whose clock was set by a neighbour's history-lock hold,
  // and a lane that is simply expensive. Per level, because an all-levels maximum is dominated by
  // the minimap's tiles, which are asked once during its opening fill and are not the subject; the
  // live pane's own level is the row to read. (Measured on the pane's level: 2.6 s mean before this
  // ticket, 1.7 s after.)
  const again = new Map(), gapByLevel = new Map();
  for (const r of asked) {
    const k = `${r.levelF}/${r.levelT}`;
    const prev = again.get(r.key);
    if (prev !== undefined) gapByLevel.set(k, Math.max(gapByLevel.get(k) ?? 0, r.startedMs - prev));
    again.set(r.key, r.endedMs ?? r.startedMs);
  }
  t.diagnostic(`longest gap between successive answers for ONE address, by level: ${[...gapByLevel]
    .sort((a, b) => b[1] - a[1]).map(([k, g]) => `${k} ${g}ms`).join("   ") || "no address was asked twice"}`);

  // **The FIRST sample is asserted on its own** (T-491). The `4 of 5` tolerance below is deliberate
  // — one screenshot may catch a frame mid-scroll — but it is also exactly wide enough to hide a
  // dead OPENING, and that is the failure it hid: the flat sample was `t+8 s` in every one of the
  // runs that failed, never a later one, and the first seconds of a session are when a user is
  // looking. A tolerance that happens to cover the one sample whose failure has its own mechanism is
  // not a tolerance, it is a blind spot.
  assert.ok(samples[0].ok,
    `the newest rows of a following pane were a FLAT fill 8 s after the surface first drew ` +
    `(${samples[0].c.distinct} distinct, dominant ${(samples[0].c.dominantShare * 100).toFixed(0)} %). ` +
    "The later samples say whether the live edge recovers; this one says whether it ever started. " +
    "T-491: look at the gap and the per-level latencies above — a live lane whose cadence was set " +
    "from one answer the minimap's initial fill had slowed is what this guard exists to catch.");

  const drawn = samples.filter((s) => s.ok).length;
  assert.ok(drawn >= 4,
    `the newest rows of a following pane were a real render in only ${drawn} of 5 samples over 20 s ` +
    "of capture. This is T-460: the rows were recorded and served, and the client stopped asking.\n  " +
    samples.map((s) => `t+${s.at}s ${s.c.distinct} distinct / dominant ${(s.c.dominantShare * 100).toFixed(0)} %`).join("\n  "));

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

  // Terminal = a refusal the ROUTE itself can emit (T-523's `fromTheRoute`, stated here rather than
  // imported because this tier drives the shipped bundle rather than linking against its modules):
  // every 4xx, plus 500 and 501. 503 is backpressure and retryable; every other 5xx would have come
  // from a proxy, and none can reach this suite, which talks to `hk serve` over loopback.
  const terminal = (s) => s >= 400 && (s < 500 || s === 500 || s === 501);
  // Per ADDRESS: a batch carries each address's refusal inside its 200 (T-573).
  await page.settleBodies();
  const refused = tileAsks(page.requests).filter((r) => r.status !== null && terminal(r.status));
  const per = new Map();
  for (const r of refused) per.set(r.key, (per.get(r.key) ?? 0) + 1);
  t.diagnostic(`${refused.length} permanent refusals over ${per.size} distinct place(s)`);
  for (const [url, n] of per) {
    assert.equal(n, 1, `${url} was refused ${n} times — asking again cannot change the answer, ` +
      "so a status the ROUTE can emit must be terminal (T-479, narrowed by T-523)");
  }
  // And the retryable one still is: a 503 is T-454's backpressure, a statement about *now*.
  const busy = tileAsks(page.requests).filter((r) => r.status === 503).length;
  t.diagnostic(`${busy} backpressure refusals (503), which stay retryable`);
  assert.deepEqual(page.exceptions, [], "uncaught exception while the surface ran");
});

test("a proxy's 502s do not make a place terminal: the live edge recovers with NO resize (T-523)", async (t) => {
  // T-523. The user's tunnel answers 502 for a slow `/api/tiles`; after a rapid zoom the live edge
  // stopped advancing, and only a window RESIZE brought it back. A 502 is not the route's answer —
  // it is a proxy saying the route did not answer — so it must not make a place terminal. The
  // injection sits in `fetch`, installed before the page's own scripts, and a flag turns it on.
  //
  // **What this test had to get right, having got it wrong twice.** The obvious assertion —
  // *every address the proxy 502'd is asked for again* — is unsound in both of the shapes it can
  // take, and both passed against the very defect they were written for:
  //
  //  - **Failing requests DURING a free-running zoom.** A zoom moves the viewport, so an address
  //    refused at an intermediate zoom is one the client legitimately never wants again. It
  //    reported three "stranded" places that were simply off-screen, against a working fix.
  //  - **Failing requests with the view HELD STILL.** Then every refused address is a tile already
  //    in hand, whose T-460 revalidation the refresh lane re-picks whether or not the place was
  //    marked terminal — so the assertion is green under the defect. Measured: 10 of 10 re-asked
  //    with the fix reverted.
  //
  // So the gesture is a zoom **out**, made while the tunnel is wedged, that the view then **stays
  // at**: out, because a coarser `level_f` is addresses the client does not hold (zooming *in*
  // only narrows onto tiles it already has); stays, because a place is only owed a redraw while
  // the viewport still wants it. And the claim is the pane's own residency readout rather than the
  // request log — see step 3.
  //
  // **This proxy carries no WebSocket to `/ws/tiles/rows`** (T-893). Since T-893 a following pane
  // puts the row its edge is in on screen from PUSHED rows, so with the feed up the pane's own
  // places at the zoomed level are drawn without ever being fetched — the premise below ("a place
  // the pane still wants was answered 502") is then unreachable, and worse, the recovery claim
  // would be vacuous, because a place made terminal would still be drawn from pushed rows. The
  // subject here is the TILE-READ path, so the feed is refused for the whole test, as a proxy
  // that does not do WebSockets would: the pane is then on the polling path T-523 was about.
  const inject = `(() => {
    const RealWS = window.WebSocket;
    window.WebSocket = class extends RealWS {
      constructor(url, protocols) {
        super(String(url).includes("/ws/tiles/rows") ? String(url).replace("/ws/tiles/rows", "/ws/no-such-route") : url, protocols);
      }
    };
    const real = window.fetch.bind(window);
    window.__t523 = { on: false, injected: [] };
    window.fetch = (input, init) => {
      const url = typeof input === "string" ? input : input.url;
      if (window.__t523.on && url.includes("/api/tiles")) {
        window.__t523.injected.push(url);
        return Promise.resolve(new Response("bad gateway", { status: 502, statusText: "Bad Gateway" }));
      }
      return real(input, init);
    };
  })();`;
  const browser = await Browser.open();
  t.after(() => browser.close());
  const page = await browser.page(undefined, { initScript: inject });
  assert.equal(await page.goto(`${ORIGIN}/#token=${TOKEN}`), "load");
  await page.waitFor("the app's surface to draw",
    `!!document.querySelector('.sf-canvas') && document.querySelector('.sf-canvas').width > 200`,
    { timeoutMs: 60000 });
  await page.waitFor("the chrome to report a viewport",
    `document.querySelectorAll('.hk-surface-viewport[data-viewport="pane"]').length > 0`, { timeoutMs: 30000 });
  const { rect } = await page.waitForCanvas(".sf-canvas", isRender, { timeoutMs: 90000 });
  const dpr = await page.eval("window.devicePixelRatio || 1");
  const pane = paneRectOf(rect, dpr);
  const at = { x: Math.round(pane.x + pane.w / 2), y: Math.round(pane.y + pane.h / 2) };
  const strip = {
    x: Math.round(pane.x), w: Math.round(pane.w),
    y: Math.round(pane.y + pane.h * FRESH_FROM), h: Math.round(pane.h * (FRESH_TO - FRESH_FROM)),
  };

  // 1. Let the pane become fully resident with the tunnel healthy, so the counts below start from a
  //    known place and the addresses the zoom asks for are genuinely NEW.
  const before = await waitForResident(page);
  t.diagnostic(`residency before the outage after ${before.ms} ms: ${before.counts}` +
    (before.resident ? "" : ` — the page then made no progress and asked for nothing for ${before.stalledMs} ms`));
  assert.ok(before.resident, `the pane never became resident with a healthy tunnel (${before.counts}); ` +
    "nothing after this would be a claim about 502s");

  // 2. The tunnel wedges, and the user zooms IN — and the view STAYS there.
  //
  //    **Stays**, because a place is only owed a redraw while the viewport still wants it — end the
  //    gesture somewhere else and a stranded place is indistinguishable from one legitimately
  //    abandoned. So nothing below touches the wheel, the window size or the pane again.
  //
  //    **In, by at least two frequency levels — and this used to say OUT** (T-846). Out was chosen
  //    because a coarser `level_f` is addresses the client does not hold. But the tuned window is a
  //    sliver of the surface, so a zoom out lands mostly on spectrum the radio never sampled, and
  //    since T-580 those places are answered by the coverage survey and never requested at all;
  //    what was left to fetch was the observed band's one or two tiles, one level out of which is
  //    the fallback pin already in hand. With `t523-proxy-502-is-terminal` injected the pane
  //    converged anyway (`2 tiles · 0 coarse stand-ins · 0 pending · 8 never sampled`). A zoom IN
  //    from the opening view stays inside the tuned window, so every address at the new level is
  //    OBSERVED spectrum T-580 cannot short-circuit, and FINER than anything the cache holds (it
  //    keeps its own level and coarser pins, never finer) — so each one is a new place the pane
  //    must fetch, and one a 502 makes terminal is drawn as a coarse stand-in for good.
  const paneLevel = async () => {
    const lv = await page.eval(`document.querySelector('.hk-surface-viewport[data-viewport="pane"] .hk-surface-level')?.textContent ?? ''`);
    const m = /level (\d+)\/(\d+)/.exec(lv);
    return m ? { f: Number(m[1]), t: Number(m[2]) } : { f: NaN, t: NaN };
  };
  const levelBefore = (await paneLevel()).f;
  assert.ok(Number.isFinite(levelBefore), "the pane's readout states no level, so the zoom below cannot be measured");
  await page.eval("window.__t523.on = true");
  let levelAfter = levelBefore, ticks = 0;
  while (levelAfter > levelBefore - 2 && ticks < 16) {
    await page.wheel(at, -240, { shift: true }); await page.frames(4); ticks++;
    levelAfter = (await paneLevel()).f;
  }
  t.diagnostic(`zoomed in ${ticks} wheel tick(s): pane level_f ${levelBefore} -> ${levelAfter}`);
  assert.ok(levelAfter <= levelBefore - 2,
    `${ticks} shift-wheel ticks inward moved the pane only from level_f ${levelBefore} to ${levelAfter}, ` +
    "so the zoom may have landed on addresses already in hand and this test would assert nothing");
  // **The outage lasts as long as the zoom's own enumeration does, not 3500 ms.** The premise below
  // is that the zoom's addresses were refused, and how long the client takes to put them on the
  // wire is the route's service rate — the same twenty-fold-varying quantity `waitForResident`
  // above was bounded wrongly by. So the wedge is held until the injector's own tally stops
  // growing: every address the gesture wanted has been refused, and none is still to come.
  const asked = await page.waitForValue("the wedged proxy to be asked for the zoom's new addresses",
    "window.__t523.injected.length", (n) => n > 0, { timeoutMs: 30000 });
  // **…and until the pane's OWN final addresses have been refused** (T-846). This is what the test
  // was missing, and why `t523-proxy-502-is-terminal` stayed green. The first 502s arm T-499's
  // silence gate, which asks for nothing at all until it opens (up to 30 s) — so every refusal the
  // old wait saw landed in the first ~20 ms, on the FIRST wheel tick's addresses and its pins, and
  // the tally then sat still for 4 s and the wedge was lifted before the client had asked for a
  // single address at the level the pane ended on. Those were then fetched through a healthy
  // tunnel, and the pane converged whatever a 502 did (measured with the fault in: `2 tiles · 0
  // coarse stand-ins · 0 pending · 8 never sampled`, the 8 answered by T-580's survey). Holding the
  // wedge until a refusal names the pane's final `level_f` is the premise the recovery claim
  // needs: a place the pane still wants WAS answered 502. The deadline covers two openings of the
  // gate at its 30 s ceiling; the correct client probes on each.
  //
  // **Its own level in BOTH axes.** A tile one step coarser in time (`level_t + 1`) at the same
  // `level_f` is a fallback pin the pane asks for mid-zoom and never again once its own tiles are
  // in hand — so a refusal there says nothing about whether a 502 is terminal. Measured: the first
  // refusal "at level_f 5" arrived 1 ms into the outage and was exactly such a pin.
  const own = await paneLevel();
  const levelsOf = (url) => {
    const q = new URL(url, "http://relative.invalid").searchParams;
    return q.has("addresses")
      ? q.get("addresses").split(",").filter(Boolean).map((sp) => sp.split(".").slice(0, 2).join("/"))
      : [`${q.get("level_f")}/${q.get("level_t")}`];
  };
  const ownLevel = `${own.f}/${own.t}`;
  const finalRefused = await page.waitForValue(
    `the wedged proxy to refuse an address at the pane's own level ${ownLevel}`,
    "window.__t523.injected.slice()", (urls) => urls.some((u) => levelsOf(u).includes(ownLevel)),
    { timeoutMs: 75000 });
  t.diagnostic(`a refusal named the pane's own level ${ownLevel} after ${finalRefused.ms} ms of outage ` +
    `(levels refused so far: ${[...new Set(finalRefused.value.flatMap(levelsOf))].join(" ")})`);
  assert.ok(finalRefused.ok,
    `in ${finalRefused.ms} ms of outage the client never asked the wedged proxy for an address at the ` +
    `pane's own level ${ownLevel} (levels refused: ${[...new Set(finalRefused.value.flatMap(levelsOf))].join(" ")}), ` +
    "so no place the pane still wants was answered 502 and the recovery below would prove nothing");
  const wedge = await waitWhileWorking(page, () => page.eval("window.__t523.injected.length"),
    () => false, { stallMs: 4000, timeoutMs: 30000 });
  t.diagnostic(`the wedged proxy was first asked after ${asked.ms} ms and stopped being asked at ` +
    `${wedge.value} refusal(s), ${wedge.ms} ms into the outage`);
  await page.eval("window.__t523.on = false");
  const PANE_WINDOW = `(() => { const v = document.querySelector('.hk-surface-viewport[data-viewport="pane"]');
    return v ? [Number(v.getAttribute('data-t0-ns')), Number(v.getAttribute('data-t1-ns'))] : null; })()`;
  const winAtLift = await page.eval(PANE_WINDOW);
  const liftedAt = Date.now();
  const injected = [...new Set(await page.eval("window.__t523.injected.slice()"))];
  t.diagnostic(`${injected.length} distinct tile addresses answered 502 by the injected proxy during the zoom`);
  assert.ok(injected.length > 0, "the zoom injected no 502s, so this test asserts nothing");
  assert.equal(
    await page.$count('.hk-surface-viewport[data-viewport="pane"][data-following="true"]'), 1,
    "the pane stopped following during a frequency-only zoom; the test's subject is gone");

  // 3. Recovery, with NO resize and no further gesture of any kind.
  //
  // **The claim is the page's own residency readout, not the pixels and not the request log.**
  //  - Pixels alone passed on the defect: a missing tile is drawn as an upscaled coarser ancestor
  //    (`FALLBACK_MARK`), never as grey, so a stalled pane still censuses as "drawn".
  //  - "Every 502'd address was asked again" is unsound in both directions. A zoom moves the
  //    viewport, so an address refused mid-gesture may be one the client legitimately never wants
  //    again; and with the view held still instead, every refused address is a RESIDENT tile whose
  //    T-460 revalidation the refresh lane re-picks whether or not the place was marked terminal —
  //    which is why a first draft of this test passed against the very defect it was written for.
  //  - `.hk-surface-counts` is `PaneReport`: what the renderer actually drew this frame with. A
  //    place T-479 marks terminal is answered `pending, failed` for the rest of the session
  //    (`acquire` deliberately never answers grey), and the renderer then draws whatever coarser
  //    ancestor it holds, upscaled, forever. So **a terminal place is a pane that never converges
  //    onto its own tiles** — measured here without the fix as a steady `0 tiles · 3 coarse
  //    stand-ins · 0 pending`. That is the defect's own signature, it is what the user saw as a
  //    stalled view, and re-addressing the tiles is exactly what their resize did. Nothing here
  //    resizes, and the counts are read straight off the page.
  const after = await waitForResident(page);
  t.diagnostic(`residency after the outage after ${after.ms} ms: ${after.counts}` +
    (after.resident ? "" : ` — the page then made no progress and asked for nothing for ${after.stalledMs} ms, ` +
      "which is the terminal-place signature rather than a slow route"));
  // **Residency alone cannot see the defect on a FOLLOWING pane** (T-846), and this is why the
  // claim below is not only the readout. A terminal place heals by scrolling away: the live edge
  // carries every refused tile out of the bottom of the pane within one pane-span, after which the
  // pane holds only addresses minted since the outage and converges whatever a 502 did. Measured
  // with the fault in, the pane converged 52 s after the outage; with it out, 78 s — the correct
  // client is not faster than the scroll, so no deadline separates them (the pane spans ~52 s).
  const winAtResident = await page.eval(PANE_WINDOW);
  const spanS = winAtLift ? (winAtLift[1] - winAtLift[0]) / 1e9 : NaN;
  const advancedS = winAtLift && winAtResident ? (winAtResident[1] - winAtLift[1]) / 1e9 : NaN;
  t.diagnostic(`the pane spans ${spanS.toFixed(1)} s; its live edge advanced ${advancedS.toFixed(1)} s ` +
    "between the outage ending and the pane converging");
  // **So the sharp claim is on the wire: a place the proxy refused AT THE PANE'S OWN LEVEL is asked
  // for again once the tunnel is healthy.** The injected 502s never reach the network, so any
  // request CDP sees for one of those addresses after the lift is a re-ask. The two ways this was
  // unsound before do not apply here: the addresses are at the level the view STAYED at (not an
  // intermediate zoom the client rightly abandoned), and none was ever resident (finer than any
  // level the cache held), so T-460's refresh lane — which only revalidates a tile in hand — cannot be
  // the one re-asking. With a 502 made terminal, `acquire` never schedules them again: zero.
  // At least one, not all: the oldest of them may scroll out of the pane before T-499's gate
  // opens, which is the client correctly no longer wanting them; the newest stays on screen for a
  // whole pane-span, far longer than the gate's 30 s ceiling.
  await page.settleBodies();
  const refusedHere = new Set(tileAsks(injected.map((url) => ({ url: new URL(url, ORIGIN).href, status: 502, startedMs: 0, endedMs: null })))
    .filter((a) => a.levelF === own.f && a.levelT === own.t).map((a) => a.key));
  const reasked = new Set(tileAsks(page.requests)
    .filter((a) => a.startedMs >= liftedAt && refusedHere.has(a.key)).map((a) => a.key));
  t.diagnostic(`${refusedHere.size} distinct address(es) at the pane's own level ${ownLevel} were answered 502; ` +
    `${reasked.size} of them asked for again after the outage`);

  const samples = [];
  for (let i = 0; i < 5; i++) {
    await new Promise((r) => setTimeout(r, 4000));
    const img = await page.shot(i === 4 ? path.join(ART, "live-edge-502.png") : null);
    const c = census(img, strip);
    samples.push({ at: 4 + i * 4, c, ok: isRender(c) });
  }
  for (const s of samples) t.diagnostic(`t+${s.at}s newest rows: ${s.c.distinct} distinct, dominant ` +
    `${(s.c.dominantShare * 100).toFixed(0)} % — ${s.ok ? "drawn" : "FLAT"}`);
  assert.ok(refusedHere.size > 0, "no address at the pane's own level was refused, so nothing below is a claim about 502s");
  assert.ok(reasked.size > 0,
    `none of the ${refusedHere.size} place(s) at the pane's own level ${ownLevel} that the proxy answered ` +
    "502 was ever asked for again once the tunnel was healthy, though the view stayed on them. A 502 is the " +
    "proxy saying the route did not answer; reading it as the route's own refusal makes the place terminal " +
    "for the session, and on a following pane that hides as a stall until the live edge scrolls it away " +
    "(T-523: the user's view that only a resize brought back).");
  assert.ok(after.resident,
    `after a zoom whose tile requests the proxy answered 502, the pane never converged onto its own ` +
    `tiles again (${after.counts}) though nothing was resized and no gesture was made. A place a 502 ` +
    "made terminal is never asked for again, so the pane draws an upscaled coarser ancestor for the " +
    "rest of the session: the user's stalled view, which only a resize cleared (T-523).");
  const drawn = samples.filter((s) => s.ok).length;
  assert.ok(drawn >= 4, `after a zoom burst with injected 502s the newest rows were drawn in only ${drawn} of 5 ` +
    "samples — a proxy's 502 made a place terminal and the live edge stalled until something re-laid it out");
  assert.deepEqual(page.exceptions, [], "uncaught exception while the live view ran");
});

test("a FOLLOWING pane shorter than a tile keeps its rows across a row boundary, with the cold tile route slowed (T-890)", async (t) => {
  // **The defect** (T-890, seen in a gate and alone): a following pane had its tiles, then the live
  // edge crossed into a new time row and for 8-10+ s it drew NOTHING — "0 tiles · 2 coarse stand-ins
  // · 2 drew nothing", meanLuma 0.0. The pane's span was shorter than a level-0 tile (10.24 s), so
  // once its old row scrolled out only the NEW row's tile could put rows on it, and that tile was
  // not asked for until the edge was already inside it — a cold miss on a busy route.
  //
  // **The slowed route is the busy box, made deterministic.** Every `GET /api/tiles/batch` (how an
  // ordinary miss is asked for, T-573) is held for COLD_MS before it goes out; the single-tile
  // revalidation of the live edge (sent alone, T-573) is left alone. That is the asymmetry a
  // loaded box has — a batch answers when its slowest member does and queues behind the parent
  // pins — and it makes the crossing's cold miss outlast the pane's span on any machine, so the
  // defect is red here without needing load. The assertion is the pane's own report
  // (`PaneReport`, what the renderer drew the frame with): after it has been resident, the pane
  // never again has NONE of its own tiles in hand at its own level.
  const COLD_MS = 2000;
  const inject = `(() => {
    const real = window.fetch.bind(window);
    window.__t890 = { held: 0 };
    window.fetch = async (input, init) => {
      const url = typeof input === "string" ? input : input.url;
      if (url.includes("/api/tiles/batch")) {
        window.__t890.held++;
        await new Promise((r) => setTimeout(r, ${COLD_MS}));
      }
      return real(input, init);
    };
  })();`;
  const browser = await Browser.open();
  t.after(() => browser.close());
  // A narrow window, so the pane is a couple of tile columns wide as observed ("2 tiles"), not six:
  // fewer places to fill cold, and the same boundary.
  const page = await browser.page(undefined, { initScript: inject, width: 800 });
  assert.equal(await page.goto(`${ORIGIN}/#token=${TOKEN}`), "load");
  await page.waitFor("the chrome to report a viewport",
    `document.querySelectorAll('.hk-surface-viewport[data-viewport="pane"]').length > 0`, { timeoutMs: 60000 });
  assert.equal(
    await page.$count('.hk-surface-viewport[data-viewport="pane"][data-following="true"]'), 1,
    "no pane is following the live edge, so there is no live edge to cross");

  const READ = `(() => { const v = document.querySelector('.hk-surface-viewport[data-viewport="pane"]');
    const c = document.querySelector('.sf-canvas').getBoundingClientRect();
    return JSON.stringify({ level: v.querySelector('.hk-surface-level')?.textContent ?? '',
      counts: v.querySelector('.hk-surface-counts')?.textContent ?? '',
      following: v.getAttribute('data-following'),
      t0: Number(v.getAttribute('data-t0-ns')), t1: Number(v.getAttribute('data-t1-ns')),
      x: c.x + c.width / 2, y: c.y + c.height / 3 }); })()`;
  const read = async () => JSON.parse(await page.eval(READ));

  // A pane SHORTER than a tile, as observed (2.8 s against 10.24 s): zoom time alone (alt, T-456)
  // until it is. A pane taller than a tile always has its previous row in the box, and the defect
  // could hide behind it.
  let r = await read();
  for (let i = 0; i < 30 && r.t1 - r.t0 > 3e9; i++) {
    await page.wheel({ x: r.x, y: r.y }, -240, { alt: true });
    await page.frames(3);
    r = await read();
  }
  const cellMs = Number(/× (\d+(?:\.\d+)?) ms cells/.exec(r.level)?.[1] ?? NaN);
  const tileNs = cellMs * 1e6 * 256;
  t.diagnostic(`pane span ${((r.t1 - r.t0) / 1e9).toFixed(2)} s at "${r.level}"; a tile is ${(tileNs / 1e9).toFixed(2)} s`);
  assert.ok(Number.isFinite(tileNs), `the pane's readout states no time cell ("${r.level}"), so no row boundary can be located`);
  assert.ok(r.t1 - r.t0 < tileNs, `the pane (${((r.t1 - r.t0) / 1e9).toFixed(2)} s) is not shorter than a tile, so this run proves nothing`);

  const res = await waitForResident(page);
  t.diagnostic(`resident after ${res.ms} ms: ${res.counts}`);
  assert.ok(res.resident, `the pane never became resident with the cold route slowed (${res.counts})`);
  const level0 = (await read()).level;

  // Watch across two JUDGED row boundaries, sampling the pane's own report. A boundary is judged
  // only when the pane had been resident for 3 x COLD_MS before reaching it: the next row can only
  // be asked for once the current one is in hand, so a boundary a moment after the first fill is
  // a race about the fill, not about the crossing. Each judged boundary is watched for a span and
  // two cold fetches past it — the window in which the defect drew nothing.
  const rowOf = (x) => Math.floor(x.t1 / tileNs);
  const span = r.t1 - r.t0;
  const empty = [];
  let crossings = 0, judged = 0, lastJudgedAt = 0, row = rowOf(await read()), blankSince = null, worstBlank = 0;
  const started = Date.now();
  while (judged < 2 || Date.now() - lastJudgedAt < span / 1e6 + 2 * COLD_MS) {
    if (Date.now() - started > 6 * tileNs / 1e6) break;
    const s = await read();
    assert.equal(s.following, "true", "the pane stopped following, so the rest of this run is not about the live edge");
    assert.equal(s.level, level0, `the pane changed level mid-run ("${level0}" -> "${s.level}") with no gesture`);
    if (rowOf(s) !== row) {
      crossings++;
      row = rowOf(s);
      const at = Date.now() - started;
      if (judged > 0 || at >= 3 * COLD_MS) { judged++; lastJudgedAt = Date.now(); }
      t.diagnostic(`crossed into row ${row} after ${at} ms${judged ? ` (judged boundary ${judged})` : " (too soon after the first fill: not judged)"}`);
    }
    const m = /(\d+) tiles · (\d+) coarse stand-in\S* · (\d+) pending/.exec(s.counts);
    const tiles = Number(m?.[1] ?? NaN), fallbacks = Number(m?.[2] ?? NaN);
    const nothing = Number(/(\d+) drew nothing/.exec(s.counts)?.[1] ?? 0);
    if (judged > 0 && tiles === 0 && !/never sampled/.test(s.counts)) empty.push({ at: Date.now() - started, counts: s.counts });
    if (tiles + fallbacks - nothing <= 0) { blankSince ??= Date.now(); worstBlank = Math.max(worstBlank, Date.now() - blankSince); }
    else blankSince = null;
    await new Promise((res2) => setTimeout(res2, 100));
  }
  const held = await page.eval("window.__t890.held");
  t.diagnostic(`${crossings} row boundaries crossed, ${judged} judged; ${held} batch request(s) held ${COLD_MS} ms; ` +
    `longest stretch with nothing drawn ${worstBlank} ms; ${empty.length} sample(s) with no tile of the pane's own in hand`);
  for (const e of empty.slice(0, 5)) t.diagnostic(`  +${e.at} ms: ${e.counts}`);
  assert.ok(held > 0, "no batch request was held, so the cold route was never slowed and this run proves nothing");
  assert.ok(judged >= 2, `only ${judged} judged row boundar${judged === 1 ? "y" : "ies"} in the watch, so the defect was not exercised`);
  assert.equal(empty.length, 0,
    `a following pane had NONE of its own tiles in hand after crossing a row boundary (first at +${empty[0]?.at} ms: ` +
    `"${empty[0]?.counts}"): its old row scrolled out and the new row was still a cold fetch — the live edge ` +
    "gated on a tile request (T-890)");
  assert.deepEqual(page.exceptions, [], "uncaught exception while the live view ran");
});

test("a FOLLOWING pane's newest rows arrive PUSHED: a row feed opens and every pushed row is on screen, with every tile read slowed (T-893)", async (t) => {
  // **The defect** (T-893, after T-890): rows reached a following pane only when the polling lane
  // re-asked its live tile, at most once per a share of what a tile costs. On a busy route that
  // period exceeds a short pane's span, so the TOP of the pane was drawn seconds behind the rows the
  // server already had — "drawn to 2.3 s short of the top" of a 2.8 s pane in a gate run, with no
  // row crossing at all. `/ws/tiles/rows` pushes each row as it is recorded, and nothing used it.
  //
  // **Every** tile read is held here — the batch AND the lone revalidation — so polling alone cannot
  // keep up on any machine; the row feed is the only path left that can. The assertions are that a
  // feed opened and that the pane is drawn up to the newest row the server has PUSHED (the pane's
  // own report against the rows seen on the socket), sampled over several seconds and a row boundary.
  const HOLD_MS = 2500;
  const inject = `(() => {
    const real = window.fetch.bind(window);
    window.__t893 = { held: 0, feeds: 0, messages: 0, pushedNs: 0 };
    window.fetch = async (input, init) => {
      const url = typeof input === "string" ? input : input.url;
      if (url.includes("/api/tiles")) {
        window.__t893.held++;
        await new Promise((r) => setTimeout(r, ${HOLD_MS}));
      }
      return real(input, init);
    };
    const RealWS = window.WebSocket;
    window.WebSocket = class extends RealWS {
      constructor(url, protocols) {
        super(url, protocols);
        if (String(url).includes("/ws/tiles/rows")) {
          window.__t893.feeds++;
          this.addEventListener("message", (ev) => {
            window.__t893.messages++;
            // The newest row the SERVER has pushed, on the capture clock: what the pane could show.
            try {
              const m = JSON.parse(ev.data);
              if (m.type === "rows") window.__t893.pushedNs = Math.max(window.__t893.pushedNs, (m.row0 + m.rows) * m.t_cell_s * 1e9);
            } catch {}
          });
        }
      }
    };
  })();`;
  const browser = await Browser.open();
  t.after(() => browser.close());
  const page = await browser.page(undefined, { initScript: inject, width: 800 });
  assert.equal(await page.goto(`${ORIGIN}/#token=${TOKEN}`), "load");
  await page.waitFor("the chrome to report a viewport",
    `document.querySelectorAll('.hk-surface-viewport[data-viewport="pane"]').length > 0`, { timeoutMs: 60000 });
  assert.equal(
    await page.$count('.hk-surface-viewport[data-viewport="pane"][data-following="true"]'), 1,
    "no pane is following the live edge, so there is no live edge to keep up with");

  const READ = `(() => { const v = document.querySelector('.hk-surface-viewport[data-viewport="pane"]');
    const c = document.querySelector('.sf-canvas').getBoundingClientRect();
    return JSON.stringify({ level: v.querySelector('.hk-surface-level')?.textContent ?? '',
      counts: v.querySelector('.hk-surface-counts')?.textContent ?? '',
      following: v.getAttribute('data-following'),
      t0: Number(v.getAttribute('data-t0-ns')), t1: Number(v.getAttribute('data-t1-ns')),
      x: c.x + c.width / 2, y: c.y + c.height / 3 }); })()`;
  const read = async () => JSON.parse(await page.eval(READ));
  // A pane shorter than 3 s, as observed: zoom time alone (alt, T-456).
  let r = await read();
  for (let i = 0; i < 30 && r.t1 - r.t0 > 3e9; i++) {
    await page.wheel({ x: r.x, y: r.y }, -240, { alt: true });
    await page.frames(3);
    r = await read();
  }
  const span = r.t1 - r.t0;
  t.diagnostic(`pane span ${(span / 1e9).toFixed(2)} s at "${r.level}"`);
  assert.ok(span < 3.5e9, `the pane (${(span / 1e9).toFixed(2)} s) did not zoom below 3.5 s, so it is not the observed shape`);

  // Wait until the pane draws anything at all, then watch its top for 15 s.
  const drew = await waitWhileWorking(page, read, (s) => {
    const m = /(\d+) tiles · (\d+) coarse stand-in\S*/.exec(s.counts);
    return !!m && Number(m[1]) + Number(m[2]) > 0;
  }, { everyMs: 300, stallMs: 30000, timeoutMs: 90000 });
  assert.ok(drew.ok, `the pane never drew anything with every tile read held ${HOLD_MS} ms (${drew.value?.counts})`);
  const shorts = [];
  const started = Date.now();
  const level0 = (await read()).level;
  // A settling second first: the first tile read may still be landing.
  await new Promise((res) => setTimeout(res, 1000));
  while (Date.now() - started < 16000) {
    const s = await read();
    assert.equal(s.following, "true", "the pane stopped following, so the rest of this run is not about the live edge");
    assert.equal(s.level, level0, `the pane changed level mid-run ("${level0}" -> "${s.level}") with no gesture`);
    const short = Number(/drawn to (\d+(?:\.\d+)?) s short of the top/.exec(s.counts)?.[1] ?? 0);
    // A pane that drew nothing at all is short by its whole span (the readout then states no top).
    const m = /(\d+) tiles · (\d+) coarse/.exec(s.counts);
    const blank = Number(/(\d+) drew nothing/.exec(s.counts)?.[1] ?? 0);
    const none = !m || Number(m[1]) + Number(m[2]) - blank <= 0;
    // The client's own lag: how far below the newest row the server has already pushed (capped at
    // the pane's top) the pane is drawn. The server's stalls are not in this number; the client's are.
    const pushedNs = Number(await page.eval("window.__t893.pushedNs"));
    const topNs = none ? s.t0 : s.t1 - short * 1e9;
    const lag = pushedNs > 0 ? Math.max(0, Math.min(pushedNs, s.t1) - topNs) / 1e9 : null;
    shorts.push({ at: Date.now() - started, short: none ? span / 1e9 : short, lag, counts: s.counts });
    await new Promise((res) => setTimeout(res, 200));
  }
  const stats = JSON.parse(await page.eval("JSON.stringify(window.__t893)"));
  const worst = shorts.reduce((m, x) => (x.short > m.short ? x : m));
  const late = shorts.filter((x) => x.short > 0.5);
  t.diagnostic(`${shorts.length} samples; worst top shortfall ${worst.short} s at +${worst.at} ms ("${worst.counts}"); ` +
    `${late.length} sample(s) over 0.5 s; ${stats.held} tile read(s) held ${HOLD_MS} ms; ${stats.feeds} row feed(s) opened, ${stats.messages} message(s)`);
  for (const x of late.slice(0, 5)) t.diagnostic(`  +${x.at} ms: ${x.counts}`);
  const sorted = shorts.map((x) => x.short).sort((a, b) => a - b);
  t.diagnostic(`shortfall p50 ${sorted[Math.floor(sorted.length / 2)]} s, p90 ${sorted[Math.floor(sorted.length * 0.9)]} s; all: ${shorts.map((x) => x.short).join(" ")}`);
  t.diagnostic(`last sample: ${shorts[shorts.length - 1].counts}`);
  assert.ok(stats.held > 0, "no tile read was held, so the route was never slowed and this run proves nothing");
  assert.ok(stats.feeds > 0, "the following pane opened no /ws/tiles/rows subscription: its rows arrive only by polling (T-893)");
  // **What is gated is STRUCTURAL: a feed opened, and the rows it pushed are on screen.** How far
  // below the live edge the top sits is also how often the SERVER pushes, and under load that is
  // the server's cadence (T-901 owns it: pushes stall 0.35-5 s about every 64 rows even on an idle
  // box). A wall-clock bound on it went red alone at load 19-32 (median 1.1 s) while the client's
  // lag behind the newest pushed row stayed at p90 0.03-0.04 s — so the shortfall is REPORTED here,
  // never asserted. For the record: on the pre-T-893 code this run reads p50 1.4 s with 60 of 75
  // samples over 0.5 s and opens 0 feeds (the `feeds` assertion above is what goes red there).
  const p50 = sorted[Math.floor(sorted.length / 2)];
  t.diagnostic(`top shortfall p50 ${p50} s (reported only; server push cadence is T-901's)`);
  const lags = shorts.map((x) => x.lag).filter((x) => x !== null).sort((a, b) => a - b);
  const lag90 = lags[Math.floor(lags.length * 0.9)];
  t.diagnostic(`client lag behind the newest PUSHED row: p50 ${lags[Math.floor(lags.length / 2)]?.toFixed(2)} s, ` +
    `p90 ${lag90?.toFixed(2)} s, worst ${lags[lags.length - 1]?.toFixed(2)} s over ${lags.length} samples`);
  // One readout decimal (0.1 s) plus a sample's worth of rows arriving between the two reads.
  assert.ok(lags.length > 0 && lag90 <= 0.35,
    `rows the server had already PUSHED were not on the pane: p90 ${lag90} s behind the newest pushed row — the ` +
    "feed is open but its rows do not reach the screen (T-893)");
  assert.deepEqual(page.exceptions, [], "uncaught exception while the live view ran");
});
