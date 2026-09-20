// **The cutover's own guard** (T-445): the APP — `/`, the page the user actually opens — loads,
// mounts the unified surface, and draws it, under the product's own Content-Security-Policy.
//
// This tier exists because of exactly the failure this ticket could repeat. T-450's renderer was
// proved on 114 973 of 115 200 pixels by T-441 and **could not load in a browser at all**: `new
// Function` at module scope against `default-src 'self'`. The unit tier could not see it, because
// node has no CSP and nothing in `ui/src` imported the module. T-445 now puts that same renderer on
// the app's critical path and deletes the waterfall it replaces, so "the app still comes up" stops
// being a property of an additive preview and becomes the product.
//
// What this asserts that `surface-load.e2e.mjs` does not: that assertion is about `/surface.html`,
// a page with its own bundle and its own entry. Passing it says nothing about `/`, whose bundle is
// built by a different esbuild line, with `--splitting`, and whose mount runs inside the app shell
// beside a store, a control-state poll and a stream socket. The two are different subjects — and
// naming the subject is the discipline this milestone keeps having to relearn.
import test from "node:test";
import assert from "node:assert/strict";
import path from "node:path";
import { Browser } from "./harness.mjs";
import { UI_DIR } from "./backend.mjs";

const ORIGIN = process.env.HK_E2E_ORIGIN, TOKEN = process.env.HK_E2E_TOKEN;
const ART = process.env.HK_E2E_ARTIFACTS ?? path.join(UI_DIR, "e2e", "artifacts");

test("GET / mounts the unified surface in the app, under the product CSP", async (t) => {
  const browser = await Browser.open();
  t.after(() => browser.close());
  const page = await browser.page();
  const tiles = page.watchConcurrency("tiles", (u) => u.includes("/api/tiles"));

  const t0 = Date.now();
  assert.equal(await page.goto(`${ORIGIN}/#token=${TOKEN}`), "load", "the app page never fired load");

  // (1) The CSP mechanism, named — T-450's defect, on the app's own bundle this time.
  const violations = await page.eval("JSON.stringify(window.__cspViolations ?? [])");
  assert.equal(violations, "[]", `the app violated its own CSP: ${violations}`);

  // (2) Nothing threw on the way up. The app mounts four areas eagerly; a throw in any of them
  // leaves the rest unmounted, and before this tier that was invisible.
  assert.deepEqual(page.exceptions, [], "uncaught exception during load");

  // (3) The retired surfaces are not in the page, and the one that replaced them is. Asserted in
  // the DOM rather than in the source, because the source assertion is `surface-cutover.test.ts`'s
  // and this is the tier that can say the browser agrees.
  assert.equal(await page.$count('[data-slot="surface"]'), 1, "the surface slot is not on the page");
  for (const gone of ["timenav", "freqnav", "live", "axis"]) {
    assert.equal(await page.$count(`[data-slot="${gone}"]`), 0, `the retired ${gone} widget is still mounted`);
  }
  // The rest of Explore is still there: the cutover replaced the centre, not the app.
  for (const kept of ["inventory", "selections", "capture", "focus", "outputs"]) {
    assert.equal(await page.$count(`[data-slot="${kept}"]`), 1, `${kept} was lost in the cutover`);
  }

  // (4) It addressed the surface, or said why it could not. `.sf-note` carries T-450's orientation
  // sentence on success and the failure text on every abort path, so waiting on "not still
  // addressing" and then reading it is the difference between a diagnosis and a 30 s timeout.
  await page.waitFor("the app's surface to finish addressing",
    `!!document.querySelector('.sf-canvas') &&
     ((document.querySelector('.sf-note')?.textContent ?? "").length > 0)`, { timeoutMs: 60000 });
  const note = (await page.$text(".sf-note")) ?? "";
  assert.ok(!/could not be addressed|WebGL2 is unavailable/.test(note),
    `the app's surface refused to mount: ${note}`);
  t.diagnostic(`orientation: ${note}`);

  // (5) It drew. A drawing buffer equal to the element's CSS box at this dpr first — a canvas sized
  // 0, or sized to the wrong box, renders "successfully" into nothing.
  const rect0 = await page.$rect(".sf-canvas");
  assert.ok(rect0 && rect0.w > 200 && rect0.h > 100, `the app's canvas has no box: ${JSON.stringify(rect0)}`);
  const buf = await page.eval(`(() => { const c = document.querySelector('.sf-canvas');
    return { w: c.width, h: c.height, dpr: window.devicePixelRatio }; })()`);
  assert.equal(buf.w, Math.round(rect0.w * buf.dpr), "drawing-buffer width is not the CSS box at this dpr");
  assert.equal(buf.h, Math.round(rect0.h * buf.dpr), "drawing-buffer height is not the CSS box at this dpr");

  // Then the histogram, the same shape of claim `surface-load` makes: enough distinct colours that
  // this is a ramp and not a clear, no single colour owning the frame, and not black. The black
  // Live waterfall is one of the defects the whole-UI window rule is named after; this is the
  // assertion that would see it.
  const drawn = (c) => c.distinct >= 16 && c.dominantShare < 0.97;
  const { census: c, rect, ms } = await page.waitForCanvas(".sf-canvas", drawn,
    { timeoutMs: 90000, saveAs: path.join(ART, "app-surface.png") });
  t.diagnostic(`app canvas ${rect.w}×${rect.h} drawn after ${ms} ms: ${c.distinct} distinct colours, ` +
    `dominant ${c.dominant} at ${(c.dominantShare * 100).toFixed(1)} %, mean luma ${c.meanLuma.toFixed(1)}`);

  // (6) The tile route is the surface's data path, and the in-flight cap is the server's (T-454).
  assert.ok(page.requests.some((r) => r.url.includes("/api/tiles") && r.status === 200),
    "the app never fetched a tile — the centre is not reading the pyramid");
  assert.ok(tiles.peak > 0, "no tile request was ever in flight");

  // (7) **Nothing the page did on its own reached the front end.** Loading is not a gesture, and
  // the cutover put a retune offer on this surface (T-444) — so the control this asserts is that it
  // is an OFFER: no device route is touched until a button is pressed.
  const control = page.requests.filter((r) => /\/api\/control\/(center|rate|gains|bias_tee|baseband_filter)/.test(r.url));
  assert.deepEqual(control.map((r) => r.url), [], "the app commanded the front end just by opening");

  // (8) **Every control in the toolbar is actually pressable** (T-528).
  //
  // The defect this exists for: `.sf-actions` was a shrinkable flex container, so once its buttons
  // were wider than the bar the *container* shrank and the buttons overflowed to the right, under
  // `.sf-range` — a later sibling, therefore painted on top. They stayed visible, focusable and
  // `offsetParent !== null`; they stopped being *clickable*, because a real click lands on whatever
  // `elementFromPoint` says is on top. Adding a fourth toggle to this bar silently disabled `Split`,
  // `Close` and `Whole surface`, and the only thing that noticed was two assertions in another file
  // reporting that a split had not added a viewport.
  //
  // So the guard is the hit test itself, over **every** button rather than over the three that
  // happened to break, and it is written here because this is the file that owns the app's surface
  // chrome. Any future control that makes the row too wide fails here, naming itself, instead of
  // making an unrelated spec fail somewhere else.
  const unclickable = JSON.parse(await page.eval(`JSON.stringify(
    [...document.querySelectorAll('.sf-actions button')].map((el) => {
      const r = el.getBoundingClientRect();
      const top = document.elementFromPoint(r.x + r.width / 2, r.y + r.height / 2);
      return { label: (el.textContent ?? '').trim(), w: Math.round(r.width),
               covered: top ? (top.className || top.tagName) : 'nothing',
               ok: !!top && (top === el || el.contains(top)) };
    }).filter((b) => !b.ok))`));
  assert.deepEqual(unclickable, [],
    "a toolbar button is not clickable at its own centre — something is drawn over it, or it has "
    + "overflowed the bar. A control a user can see is a control a user can press.");
  const bar = JSON.parse(await page.eval(`JSON.stringify((() => {
    const b = document.querySelector('.sf-bar').getBoundingClientRect();
    const s = document.querySelector('.sf-stage').getBoundingClientRect();
    const last = [...document.querySelectorAll('.sf-bar > *')].map((e) => e.getBoundingClientRect().bottom);
    return { bottom: b.bottom, stageTop: s.top, contentBottom: Math.max(...last) };
  })())`));
  assert.ok(bar.contentBottom <= bar.stageTop + 0.5,
    `the bar's contents reach ${bar.contentBottom.toFixed(1)} px, past the stage at ${bar.stageTop.toFixed(1)} px: `
    + "chrome is being drawn over the picture");

  t.diagnostic(`load-to-drawn ${Date.now() - t0} ms · ${page.requests.length} requests`);
});

test("a drag on the app's surface moves the view and still reaches no device route", async (t) => {
  // T-340's control, one surface over and in a real browser: a pan is a pan. The unit tier asserts
  // it against a spy client (T-442 over the whole pane vocabulary); this asserts it against the
  // wire, which is the only place a second path could actually appear.
  const browser = await Browser.open();
  t.after(() => browser.close());
  const page = await browser.page();

  assert.equal(await page.goto(`${ORIGIN}/#token=${TOKEN}`), "load");
  await page.waitFor("the app's surface to draw",
    `!!document.querySelector('.sf-canvas') && document.querySelector('.sf-canvas').width > 200`,
    { timeoutMs: 60000 });
  const rect = await page.$rect(".sf-canvas");

  const before = (await page.$text(".sf-chrome")) ?? "";
  await page.drag(
    { x: rect.x + rect.w * 0.6, y: rect.y + rect.h * 0.4 },
    { x: rect.x + rect.w * 0.3, y: rect.y + rect.h * 0.4 });
  await page.waitFor("the per-viewport readout to change after a drag",
    `(document.querySelector('.sf-chrome')?.textContent ?? "") !== ${JSON.stringify(before)}`,
    { timeoutMs: 15000 });

  const control = page.requests.filter((r) => /\/api\/control\/(center|rate|gains|bias_tee|baseband_filter)/.test(r.url));
  assert.deepEqual(control.map((r) => r.url), [], "a drag reached the front end");
  assert.deepEqual(page.exceptions, [], "uncaught exception while dragging");
});
