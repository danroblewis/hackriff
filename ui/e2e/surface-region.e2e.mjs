// **T-458: the region gesture, confirmed in a real browser.**
//
// The user's instruction for T-456 was "confirm the chosen modifiers actually work in a real
// browser", and it is the whole reason that ticket depended on this tier: a unit test cannot tell
// you whether the OS or the browser ate your gesture. The same instruction carries here, because
// shift+drag is a *second meaning on the pointer stream* and the ways it can fail are all outside
// node — the browser could start a native text/image drag instead, the OS could claim the chord, or
// the modifier could simply not survive the trip to the page.
//
// ——— THE TRAP THIS FILE IS WRITTEN AGAINST ———
//
// T-456's own sharp version: **an assertion that the view changed is not an assertion that the
// browser delivered your gesture.** A page that ignored the modifier entirely and just panned would
// also change the view. So the first assertion here reads the events **as the canvas received
// them** — `shiftKey` set by Chrome from the CDP bitmask, on the `pointerdown` the product's own
// listener got — before anything is concluded from what the page then did. The second reads what
// the page did *not* do, which for this gesture is the load-bearing half: the viewport must not
// move under a rectangle being drawn on it.
//
// ——— WHAT CDP CAN AND CANNOT SAY ———
//
// A CDP event enters at the renderer, so a green run here says the *browser* delivers shift+drag to
// the page and does not turn it into a native drag or a context menu. It cannot say what a window
// server would do with the chord before Chrome sees it — which is exactly the caveat T-456 wrote for
// ctrl+wheel, and exactly why ctrl was left unbound there and is not used here either. The check
// that ctrl does NOT mark out a region is below for that reason: it is the failure mode with no
// in-browser guard, so the binding avoids it rather than testing for it.
//
// ——— NON-VACUITY, MEASURED ———
//
// Three faults were built into `ui/src`, rebuilt, and run against this file (T-458):
//
//   `dragIntent` always returns "pan" (the modifier ignored)  -> tests 1, 3 and 4 RED
//   the region branch falls through and pans too              -> tests 1 and 3 RED
//   the tap gate removed (`far = true`)                       -> test 3 RED, alone
//
// `ui/e2e/selftest.mjs` cannot hold these as standing faults: its `build()` compiles only the
// `/surface.html` bundle, and this file drives the APP at `/` (as `app-surface.e2e.mjs` does), so
// under a selftest run both fail for every fault and the per-guard attribution it exists to give
// would be meaningless. Recorded here rather than left implied; building the app bundle there too
// would fix it for both files.
import test, { after } from "node:test";
import assert from "node:assert/strict";
import { Browser } from "./harness.mjs";

const ORIGIN = process.env.HK_E2E_ORIGIN, TOKEN = process.env.HK_E2E_TOKEN;

/**
 * The per-viewport chrome readout the page draws, projected to the part a GESTURE can move.
 *
 * WHY A PROJECTION. The `where` line ends in the pane's offset from the LIVE EDGE — `LIVE`, or
 * `−281 ms`. That offset is `edge − t1`, and **`edge` is not view state**: it advances as capture
 * arrives, with no gesture touching it. Comparing the raw string therefore makes every assertion
 * here load-sensitive — an `equal` ("it must not have panned") goes red, and, worse because it is
 * silent, a `notEqual` ("a plain drag must pan") can go GREEN on drift alone, with no pan at all.
 *
 * T-478 FIRST DROPPED THE OFFSET ONLY WHILE FOLLOWING, AND THAT WAS EXACTLY BACKWARDS. The reasoning
 * was that a paused pane's offset is "a fixed property of the view". It is not. A FOLLOWING pane
 * tracks the edge, so its offset stays near zero and only jitters; a PAUSED pane holds a fixed
 * capture time while the edge runs away from it, so its offset GROWS WITHOUT BOUND — which is
 * precisely the failure that survived the first fix and blocked three merges:
 *
 *     actual   "99.787 MHz ± 1.68 MHz · −281 ms" … "following": false
 *     expected "99.787 MHz ± 1.68 MHz · −0 ms"   … "following": false
 *
 * Same centre, same span, same level, same follow state, and the view had not moved.
 *
 * So the offset is dropped in BOTH states. What a pan actually changes is the WINDOW — centre and
 * span — and whether the pane still follows; both are compared exactly, and a pan in time that
 * leaves the edge also flips `following`, which is compared. Nothing a gesture can do is excluded.
 */
const READOUT = `JSON.stringify([...document.querySelectorAll('.hk-surface-viewport')].map((v) => ({
  where: (v.querySelector('.hk-surface-where')?.textContent ?? '').split('\u00b7')[0].trim(),
  level: v.querySelector('.hk-surface-level')?.textContent ?? '',
  following: v.getAttribute('data-following') === 'true',
})))`;

/** Every pointer-ish event the CANVAS ITSELF received, in order, with the flags the browser set. */
const PROBE = `(() => {
  const c = document.querySelector('.sf-canvas');
  window.__ev = [];
  for (const t of ['pointerdown', 'pointermove', 'pointerup', 'contextmenu', 'dragstart', 'selectstart']) {
    c.addEventListener(t, (e) => window.__ev.push({
      type: e.type, shiftKey: !!e.shiftKey, altKey: !!e.altKey, ctrlKey: !!e.ctrlKey,
      button: e.button ?? null, buttons: e.buttons ?? null,
    }), true);
  }
  return true;
})()`;

/**
 * **One browser and one app page for the whole file**, opened lazily and cached — the *rejection*
 * cached too.
 *
 * Four independent opens is four chances to pay the mount timeout. `e2e/selftest.mjs` deliberately
 * serves a partial dist, where every open fails, and a per-test open turned one 60 s diagnosis into
 * four; caching the promise makes a broken page cost the wait once and every later test fail
 * instantly with the same words. The tests below are order-independent by construction: each takes
 * its own `before` snapshot of the readout and the selection count rather than assuming a fresh
 * page.
 */
let opening = null;
const app = () => (opening ??= openApp());
after(async () => { (await opening?.catch(() => null))?.browser?.close(); });

async function openApp() {
  const browser = await Browser.open();
  const page = await browser.page();
  page.browser = browser;
  assert.equal(await page.goto(`${ORIGIN}/#token=${TOKEN}`), "load", "the app page never fired load");
  // Two waits, not one, and the short one first: if `/` did not serve the app at all there is no
  // canvas to address, and waiting a minute for a surface to finish mounting on a page that has no
  // shell reports a broken harness as a broken product. `e2e/selftest.mjs` serves a deliberately
  // partial dist, so this is the common case there rather than a hypothetical.
  await page.waitFor("the app shell to mount its surface slot",
    `!!document.querySelector('.sf-canvas')`, { timeoutMs: 15000 });
  await page.waitFor("the app's surface to finish addressing",
    `(document.querySelector('.sf-note')?.textContent ?? "").length > 0`, { timeoutMs: 60000 });
  const note = (await page.$text(".sf-note")) ?? "";
  assert.ok(!/could not be addressed|WebGL2 is unavailable/.test(note), `the surface refused to mount: ${note}`);
  await page.waitFor("a viewport readout to exist", `document.querySelectorAll('.hk-surface-viewport').length > 0`);
  await page.frames(3);
  return page;
}

/**
 * How many regions this page has committed, counted **on the wire**.
 *
 * Not `[data-sel]`: that sidebar is deliberately scoped to the window on screen (T-386), so a test
 * that pans and then counts rows is asking a different question — and it caught this file out
 * exactly once, reading a meta-drag's *pan* as a selection having been deleted. The `POST` is the
 * unambiguous fact, and it is the client's own request rather than its bookkeeping (T-454).
 */
const committed = (page) =>
  page.requests.filter((r) => r.method === "POST" && /\/api\/selections$/.test(new URL(r.url).pathname)).length;

/** A box well inside the canvas and clear of the map strip along its bottom (110 device px). */
async function field(page) {
  const r = await page.$rect(".sf-canvas");
  assert.ok(r && r.w > 300 && r.h > 260, `the canvas has no usable box: ${JSON.stringify(r)}`);
  const bottom = r.y + r.h - 130;
  return { x0: r.x + r.w * 0.3, y0: r.y + 60, x1: r.x + r.w * 0.6, y1: bottom - 40 };
}

test("shift+drag: the browser delivers it, the page marks out a region, and the view does NOT move", async (t) => {
  const page = await app();
  await page.eval(PROBE);
  const f = await field(page);

  const before = await page.eval(READOUT);
  const selsBefore = committed(page);
  await page.drag({ x: f.x0, y: f.y0 }, { x: f.x1, y: f.y1 }, 10, { shift: true });
  await page.frames(3);

  // (1) READ WHAT ARRIVED. Not "the page behaved as if shift were held" — the flag Chrome set on
  // the event the product's own listener received. If the browser had swallowed the chord, turned
  // it into a native drag, or dropped the modifier, this is where it shows, with the event list in
  // the failure message rather than a mystery about the view.
  const ev = await page.eval("JSON.stringify(window.__ev)").then(JSON.parse);
  const down = ev.find((e) => e.type === "pointerdown");
  assert.ok(down, `no pointerdown reached the canvas at all: ${JSON.stringify(ev.slice(0, 6))}`);
  assert.equal(down.shiftKey, true, `the browser did not deliver shift on pointerdown: ${JSON.stringify(down)}`);
  assert.equal(down.button, 0, "shift+drag must stay a PRIMARY press — a secondary one would open the menu");
  const moves = ev.filter((e) => e.type === "pointermove");
  assert.ok(moves.length >= 5, `the moves did not arrive: ${moves.length}`);
  assert.ok(moves.every((e) => e.shiftKey), "shift was dropped partway through the stream");
  assert.equal(ev.filter((e) => e.type === "contextmenu").length, 0,
    "shift+drag produced a context menu — the gesture is colliding with the right-button path");
  assert.equal(ev.filter((e) => e.type === "dragstart").length, 0,
    "the browser started a NATIVE drag: the page's own gesture never got the stream");

  // (2) The load-bearing negative: the viewport did not move. A rectangle drawn over a view that
  // panned under it describes a window that has already gone.
  assert.equal(await page.eval(READOUT), before,
    "the viewport moved during a region stroke — shift+drag is still panning");

  // (3) …and it produced a region: one POST of the stroke, and the page's own words about it. The
  // app's sidebar then lists it, which is also the page agreeing about *where* it is — a selection
  // is listed only when it falls inside the window on screen (T-386).
  // The store pushes the new region asynchronously, so this waits for the POST rather than assuming
  // it has already gone out — polled on the harness's own request log, not in the page.
  for (let i = 0; committed(page) === selsBefore && i < 100; i++) await new Promise((r) => setTimeout(r, 100));
  assert.equal(committed(page), selsBefore + 1, "the stroke did not commit exactly one region");
  assert.ok(await page.$count("[data-sel]") > 0, "the committed region is not listed in the window it was drawn in");
  const toast = (await page.$text("#toast")) ?? "";
  assert.match(toast, /^Region: /, `the page did not report a region: "${toast}"`);
  t.diagnostic(`regions committed: ${selsBefore} → ${committed(page)}; toast: ${toast}`);
});

test("THE CONTROL: a PLAIN drag over the same path pans and marks out nothing", async (t) => {
  // Without this, the test above proves only that *something* happened when shift was held — the
  // non-vacuity leg, and the one that separates "the modifier is read" from "the gesture works".
  const page = await app();
  await page.eval(PROBE);
  const f = await field(page);

  const before = await page.eval(READOUT);
  const selsBefore = committed(page);
  await page.drag({ x: f.x0, y: f.y0 }, { x: f.x1, y: f.y1 }, 10);
  await page.frames(3);

  const ev = await page.eval("JSON.stringify(window.__ev)").then(JSON.parse);
  assert.equal(ev.find((e) => e.type === "pointerdown").shiftKey, false, "the control drag was not modifier-free");
  assert.notEqual(await page.eval(READOUT), before, "a plain drag must pan: T-456's binding is unchanged");
  assert.equal(committed(page), selsBefore, "a plain drag created a region");
});

test("a shift-TAP marks out nothing, and the modifiers T-456 left alone still do not", async (t) => {
  const page = await app();
  const f = await field(page);
  const selsBefore = committed(page);
  const before = await page.eval(READOUT);

  // T-407's first defect, in the browser: a fat-fingered tap must not become a region. 3 px of real
  // movement, under the 6 px gate.
  await page.drag({ x: f.x0, y: f.y0 }, { x: f.x0 + 2, y: f.y0 + 2 }, 2, { shift: true });
  await page.frames(3);
  assert.equal(committed(page), selsBefore, "a shift-tap marked out a region");
  assert.equal(await page.eval(READOUT), before, "…and it must not have panned either");

  // ALT is T-456's TIME axis on the wheel and binds nothing on a drag; META is unbound. Neither may
  // quietly acquire the region gesture, or the vocabulary has two answers for one stroke.
  for (const mods of [{ alt: true }, { meta: true }]) {
    const mark = await page.eval(READOUT);
    await page.drag({ x: f.x0, y: f.y0 }, { x: f.x1, y: f.y1 }, 10, mods);
    await page.frames(3);
    assert.equal(committed(page), selsBefore,
      `${JSON.stringify(mods)}+drag marked out a region: only shift may`);
    assert.notEqual(await page.eval(READOUT), mark, `${JSON.stringify(mods)}+drag must still pan`);
  }
});

test("T-340's control, in the browser: no drag of any kind — region stroke included — reaches a device route", async (t) => {
  // The repo's oldest standing control, restated over the gesture this ticket adds. It is asserted
  // ON THE WIRE, from CDP's own request log, rather than from anything the client says about
  // itself — a client whose bookkeeping is wrong cannot certify itself (T-454's lesson).
  const page = await app();
  const f = await field(page);
  const mark = page.requests.length;

  for (const mods of [{}, { shift: true }, { alt: true }]) {
    await page.drag({ x: f.x0, y: f.y0 }, { x: f.x1, y: f.y1 }, 8, mods);
    await page.drag({ x: f.x1, y: f.y1 }, { x: f.x0, y: f.y0 }, 8, mods);
  }
  await page.frames(5);

  const after = page.requests.slice(mark);
  const device = after.filter((r) => /\/api\/control\/(center|rate|gains|bias_tee|baseband_filter|record)/.test(r.url));
  assert.deepEqual(device.map((r) => `${r.method} ${r.url}`), [],
    "a drag reached a device route: no pointer stream may command the radio");
  // Non-vacuity: the drags did reach the page and made it work, so an empty device list is a
  // finding rather than an artefact of nothing having happened.
  assert.ok(after.some((r) => r.url.includes("/api/tiles")), "the drags moved nothing at all, so this proves nothing");
  const regions = after.filter((r) => r.method === "POST" && /\/api\/selections/.test(r.url));
  assert.ok(regions.length > 0, "no region was committed, so the region leg of this control is vacuous");
  t.diagnostic(`${after.length} requests after the drags; ${regions.length} selection POSTs; 0 device calls`);
});
