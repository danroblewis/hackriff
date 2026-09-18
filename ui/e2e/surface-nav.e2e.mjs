// **T-454's guard**: panning and zooming the surface must not exceed the tile route's own
// in-flight cap, and no backpressure refusal may reach the user (T-455).
//
// The defect this file exists for: `/api/tiles` answers `503` over `cost.in_flight_limit`
// concurrent reads, naming the cap. The client already had a cap, an `AbortController` per request
// and measured cancellation — and the `503` still reached the user. So the two facts under test are
// facts about **the wire and the screen**, not about the client's own bookkeeping:
//
//   - concurrency is counted from CDP's `Network` events, so a client whose internal counter is
//     wrong cannot certify itself. The cap it is compared against is read from the server's own
//     `cost.in_flight_limit`, not restated here;
//   - `503` is checked as a response status on any request the page made, and separately as the
//     `N backpressure` counter the status line renders to the user.
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

  await page.goto(`${ORIGIN}/surface.html#token=${TOKEN}`);
  await page.waitForSurfaceMounted();
  await page.waitFor("the first tile textures to be uploaded",
    `(${STATUS}.match(/(\\d+) uploads/)?.[1] | 0) > 0`, { timeoutMs: 90000 });

  const rect = await page.$rect('[data-slot="canvas"]');
  const mid = { x: rect.x + rect.w / 2, y: rect.y + rect.h * 0.35 };
  const before = await page.eval(`JSON.stringify(${READOUT})`);
  const imgBefore = await page.shot();

  // A pan, then zoom on each axis, then a pan of the whole-surface map at the bottom. Between
  // them the page is given real frames, because the fetch storm this is about is scheduled from
  // the render loop.
  const moved = [], clamped = [];
  const step = async (what, fn, { mustMove = true } = {}) => {
    const was = await page.eval(`JSON.stringify(${READOUT})`);
    await fn();
    await page.frames(6);
    await new Promise((r) => setTimeout(r, 900));
    const now = await page.eval(`JSON.stringify(${READOUT})`);
    if (mustMove) assert.notEqual(now, was, `${what} did not move the view — the gesture missed, so it tested nothing`);
    (now === was ? clamped : moved).push(what);
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

  // Let anything still queued drain, so a refusal has every chance to arrive before we look.
  await page.frames(10);
  await new Promise((r) => setTimeout(r, 1500));

  // ——— the evidence, gathered and PRINTED before anything is asserted ———
  // A failing guard whose first assertion hides the rest of the picture is a guard people bisect by
  // hand. Everything below is reported, then judged.
  const tileReqs = page.requests.filter((r) => r.url.includes("/api/tiles"));
  const refused = page.requests.filter((r) => r.status === 503);
  const status = await page.eval(STATUS);
  const backpressure = Number(status.match(/(\d+) backpressure/)?.[1] ?? -1);
  const cancelled = Number(status.match(/(\d+) cancelled/)?.[1] ?? 0);
  const canceledOnWire = page.requests.filter((r) => r.error === "canceled").length;
  t.diagnostic(`peak ${tiles.peak}/${limit} in flight · ${tileReqs.length} tile requests · ` +
    `${cancelled} cancelled (client), ${canceledOnWire} aborted on the wire · ` +
    `${refused.length} refused 503 on the wire · ${backpressure} backpressure reported to the user`);
  if (refused.length) {
    t.diagnostic(`first refusals: ${refused.slice(0, 3).map((r) => r.url.replace(ORIGIN, "")).join(" ")}`);
  }

  // ——— the two facts, on the wire and on the screen ———
  // `tiles.peak` is a **lower bound** on what the client had outstanding: Chrome opens at most six
  // HTTP/1.1 connections per origin, so a client holding twenty fetches still shows six on the
  // wire. That asymmetry is in the sound direction for this assertion — a measured excess is real,
  // and the cap (4) is below the connection limit (6), so a compliant client can still pass — but
  // it means the number must never be read as "the client had exactly this many".
  assert.ok(tiles.peak <= limit,
    `${tiles.peak} tile requests were in flight at once against a declared cap of ${limit} ` +
    "(and that is a lower bound — the browser's own 6-connection limit hides anything beyond it)");
  assert.ok(tiles.peak > 1, `only ${tiles.peak} tile request was ever in flight — the cap was never approached, so this proves nothing`);

  assert.equal(refused.length, 0,
    `the tile route refused ${refused.length} of ${tileReqs.length} tile requests with 503 — backpressure reached the client`);
  assert.equal(backpressure, 0, `the page told the user about ${backpressure} backpressure refusals: "${status}"`);
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
  const ca = census(imgAfter, box), cb = census(imgBefore, box);
  assert.ok(ca.distinct >= 32 && ca.dominantShare < 0.92,
    `after navigating, the canvas is a flat fill: ${ca.distinct} colours, dominant ${ca.dominant} at ${(ca.dominantShare * 100).toFixed(1)} %`);
  assert.notEqual(`${ca.dominant}:${ca.distinct}`, `${cb.dominant}:${cb.distinct}`,
    "the canvas is pixel-identical before and after five viewport changes — the renderer is not following the view");
  assert.notEqual(await page.eval(`JSON.stringify(${READOUT})`), before, "the view returned to exactly where it started");
});
