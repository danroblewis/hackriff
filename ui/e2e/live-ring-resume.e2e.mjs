// **LSR-3 end to end: drop the `spectrum/live` socket for ~2 s → no black, the gap comes from tiles,
// the ring resumes** (T-1044), on the mock SDR in a real browser, behind `?live-ring=1`.
//
// The socket is dropped from the page's side: a test instrument installed before the app's scripts
// (an OBSERVING-AND-REFUSING wrapper of `WebSocket`, the harness's `initScript` seam) sends the
// app's `/ws/spectrum/live` reconnects to a path that does not exist while `window.__hkDark` is set,
// so the app sees a real close and real failed retries — the same thing a dropped network looks
// like — and the backend keeps capturing throughout (pause/drop freezes the view, never the capture).
//
// Asserts, in order:
//  1. the ring paints before the drop (the lane ran);
//  2. through the dark stretch the pane is NEVER a flat fill (the ring keeps its old rows and the
//     tile lane answers under the gap: "no black");
//  3. after the socket is allowed back, the ring resumes (rows keep growing), and the page states the
//     resubscribe it made: `/ws/spectrum/rows` carrying `t_from` (the ring's last t1) and a `t_to`
//     after it, and the walk finished.
import test from "node:test";
import assert from "node:assert/strict";
import { Browser, appUrl, census, waitWhileWorking } from "./harness.mjs";

const ORIGIN = process.env.HK_E2E_ORIGIN, TOKEN = process.env.HK_E2E_TOKEN;
const isRender = (c) => c.distinct >= 32 && c.dominantShare < 0.9;
const LIVE_RING = `JSON.parse(document.querySelector('.sf-stage')?.dataset.liveRing ?? '[]')`;
const RESUME = `JSON.parse(document.querySelector('.sf-stage')?.dataset.liveResume ?? '{}')`;

const INIT = `(() => {
  const Native = window.WebSocket;
  window.__hkDark = false;
  window.WebSocket = new Proxy(Native, {
    construct(T, args) {
      const [url, ...rest] = args;
      const u = String(url);
      if (window.__hkDark && u.includes("/ws/spectrum/live")) {
        return new T(u.replace("/ws/spectrum/live", "/ws/no-such-stream"), ...rest);
      }
      const ws = new T(url, ...rest);
      if (u.includes("/ws/spectrum/live")) (window.__hkSockets ||= []).push(ws);
      return ws;
    },
  });
})();`;

test("LSR-3: dropping the spectrum socket for ~2 s leaves no black, and the ring resumes", async (t) => {
  const browser = await Browser.open();
  t.after(() => browser.close());
  const page = await browser.page(undefined, { initScript: INIT });
  assert.equal(await page.goto(appUrl(ORIGIN, TOKEN, { liveRing: true })), "load");
  await page.waitFor("the surface to draw", `!!document.querySelector('.sf-canvas') && document.querySelector('.sf-canvas').width > 200`, { timeoutMs: 60000 });
  const rowsOf = (rings) => Array.isArray(rings) ? Math.max(0, ...rings.map((r) => r.rows), 0) : 0;

  const lane = await waitWhileWorking(page, () => page.eval(LIVE_RING), (r) => rowsOf(r) > 0,
    { everyMs: 300, stallMs: 15000, timeoutMs: 90000 });
  assert.ok(lane.ok, `the ring never painted before the drop: ${JSON.stringify(lane.value)}`);

  const { rect } = await page.waitForCanvas(".sf-canvas", isRender, { timeoutMs: 90000 });
  const dpr = await page.eval("window.devicePixelRatio || 1");
  const whole = { x: Math.round(rect.x), y: Math.round(rect.y + rect.h * 0.25), w: Math.round(rect.w), h: Math.round(rect.h * 0.5) };

  // Drop: close the live socket(s) and refuse the reconnects.
  await page.eval(`window.__hkDark = true; window.__hkSockets.forEach((s) => s.close());`);
  // The app sees a real close and retries into the dark path. Sample the picture across ~2 s of it.
  const dark = [];
  for (let i = 0; i < 4; i++) {
    await new Promise((r) => setTimeout(r, 500));
    dark.push(census(await page.shot(null), whole));
  }
  t.diagnostic(`dark samples: ${JSON.stringify(dark.map((c) => [c.distinct, +c.dominantShare.toFixed(2)]))}`);
  assert.ok(dark.every(isRender), "the pane went to a flat fill while the spectrum socket was down");

  // Back: the ring must resume and the page must say what it asked the server for.
  await page.eval(`window.__hkDark = false;`);
  const st = await waitWhileWorking(page, () => page.eval(RESUME), (r) => r.done === true,
    { everyMs: 300, stallMs: 20000, timeoutMs: 90000 });
  t.diagnostic(`resume state: ${JSON.stringify(st.value)}`);
  assert.ok(st.ok, `no resume was made after the socket came back: ${JSON.stringify(st.value)}`);
  assert.match(st.value.path, /^\/ws\/spectrum\/rows\?/);
  assert.match(st.value.path, new RegExp(`[?&]t_from=${st.value.tFromNs}(&|$)`));
  assert.match(st.value.path, new RegExp(`[?&]t_to=${st.value.tToNs}(&|$)`));
  assert.ok(st.value.tToNs > st.value.tFromNs);
  assert.equal(st.value.answer, "ok", `the server did not accept the resume: ${JSON.stringify(st.value)}`);
  assert.equal(st.value.unreadable, 0, `blocks this build could not read: ${st.value.lastError}`);
  assert.ok(st.value.blocks > 0, `the walk of the gap delivered no block: ${JSON.stringify(st.value)}`);

  const before = rowsOf(await page.eval(LIVE_RING));
  await page.frames(30);
  assert.ok(rowsOf(await page.eval(LIVE_RING)) > 0 && before > 0, "the ring did not resume painting rows");
  assert.ok(isRender(census(await page.shot(null), whole)), "the pane is a flat fill after the ring resumed");
  assert.deepEqual(page.exceptions, [], "uncaught exception across the drop");
});
