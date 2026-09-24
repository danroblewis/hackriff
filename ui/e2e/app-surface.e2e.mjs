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
  for (const kept of ["inventory", "selections", "focus", "outputs"]) {
    assert.equal(await page.$count(`[data-slot="${kept}"]`), 1, `${kept} was lost in the cutover`);
  }
  // T-506: the Capture panel is gone — its roles are on the canvas (the test at the bottom).
  assert.equal(await page.$count('[data-slot="capture"]'), 0, "the retired Capture panel is still mounted");

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
  //
  // **Waited for, then asserted — never one read** (the 09-22 gate red, `680 !== 560`). The buffer
  // follows the box through a ResizeObserver, which the browser delivers at its next rendering
  // step; the box itself changes whenever the chrome above the stage re-wraps (in the gate red the
  // box was 120 px shorter than the buffer, on a run whose orientation sentence — just waited for —
  // carried an extra clause). A read taken between the layout and that rendering step sees the new
  // box and the old buffer. Under load that gap is long enough to land
  // in, so the claim is made the only way it is true: the buffer SETTLES on the box, within a bound.
  // A canvas that never tracks its box (the defect) still times out here and fails, naming both.
  // Box and buffer are read in ONE evaluation, so every reading compares a box with the buffer of
  // the same instant; the loop keeps the last reading, and the assertions judge that one.
  const BUF = `(() => { const c = document.querySelector('.sf-canvas'); const r = c.getBoundingClientRect();
    return { w: c.width, h: c.height, cssW: r.width, cssH: r.height, dpr: window.devicePixelRatio }; })()`;
  const fits = (b) => b.w === Math.round(b.cssW * b.dpr) && b.h === Math.round(b.cssH * b.dpr);
  const SETTLE_MS = 10000;
  const settleFrom = Date.now();
  let buf = await page.eval(BUF), readings = 1;
  while (!fits(buf) && Date.now() - settleFrom < SETTLE_MS) {
    await page.frames(1);
    buf = await page.eval(BUF);
    readings++;
  }
  assert.ok(buf.cssW > 200 && buf.cssH > 100, `the app's canvas has no box: ${JSON.stringify(buf)}`);
  t.diagnostic(`drawing buffer ${buf.w}×${buf.h} for a ${buf.cssW}×${buf.cssH} box at dpr ${buf.dpr}, ` +
    `after ${readings} reading(s) over ${Date.now() - settleFrom} ms`);
  assert.equal(buf.w, Math.round(buf.cssW * buf.dpr),
    `drawing-buffer width is not the CSS box at this dpr, ${SETTLE_MS} ms after the surface addressed`);
  assert.equal(buf.h, Math.round(buf.cssH * buf.dpr),
    `drawing-buffer height is not the CSS box at this dpr, ${SETTLE_MS} ms after the surface addressed`);

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
  const control = page.requests.filter((r) => /\/api\/control\/(center|rate|window|gains|bias_tee|baseband_filter)/.test(r.url));
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

  const control = page.requests.filter((r) => /\/api\/control\/(center|rate|window|gains|bias_tee|baseband_filter)/.test(r.url));
  assert.deepEqual(control.map((r) => r.url), [], "a drag reached the front end");
  assert.deepEqual(page.exceptions, [], "uncaught exception while dragging");
});

test("T-802: the floating controls are pressable, move only the view, and offer (never command) a retune", async (t) => {
  // MAP-02 in a real browser: Go-to, layers, zoom and the follow-live FAB float over the canvas,
  // each is clickable at its own centre (T-528's hit test — a control a user can see is a control a
  // user can press), each changes the SCREEN, and none reaches a device route. A Go-to to spectrum
  // no tuned window covers shows the retune offer; the test does not press it, and asserts that
  // showing it commanded nothing.
  const browser = await Browser.open();
  t.after(() => browser.close());
  const page = await browser.page();
  assert.equal(await page.goto(`${ORIGIN}/#token=${TOKEN}`), "load");
  await page.waitFor("the surface to draw and the floating controls to mount",
    `!!document.querySelector('.sf-canvas') && document.querySelector('.sf-canvas').width > 200 &&
     !!document.querySelector('.map-ctl .map-fab') &&
     / MHz ± /.test(document.querySelector('.hk-surface-viewport[data-viewport="pane"]')?.children[1]?.textContent ?? "")`,
    { timeoutMs: 60000 });

  const covered = JSON.parse(await page.eval(`JSON.stringify(
    ['.map-goto input', '.map-layers-btn', '.map-zoom-in', '.map-zoom-out', '.map-fab'].map((sel) => {
      const el = document.querySelector(sel); const r = el.getBoundingClientRect();
      const top = document.elementFromPoint(r.x + r.width / 2, r.y + r.height / 2);
      return { sel, w: r.width, h: r.height, on: top ? (top.className?.baseVal ?? top.className ?? top.tagName) : 'nothing',
               ok: !!top && (top === el || el.contains(top)) && r.width >= 24 && r.height >= 24 };
    }).filter((b) => !b.ok))`));
  assert.deepEqual(covered, [], "a floating control is not pressable at its own centre, or is under 24 px");

  const headline = `document.querySelector('.hk-surface-viewport[data-viewport="pane"]')?.children[1]?.textContent ?? ""`;
  // Zoom in: the pane's stated window changes.
  let before = await page.eval(headline);
  await page.click("document.querySelector('.map-zoom-in')");
  await page.waitFor("zoom-in to change the pane's window", `(${headline}) !== ${JSON.stringify(before)}`, { timeoutMs: 15000 });
  before = await page.eval(headline);
  await page.click("document.querySelector('.map-zoom-out')");
  await page.waitFor("zoom-out to change the pane's window", `(${headline}) !== ${JSON.stringify(before)}`, { timeoutMs: 15000 });

  // Pause through the toolbar, then the FAB re-pins the pane to the growing edge.
  await page.click("document.querySelector('.sf-live')");
  await page.waitFor("the FAB to say the pane is frozen", `document.querySelector('.map-fab').classList.contains('frozen')`, { timeoutMs: 10000 });
  await page.click("document.querySelector('.map-fab')");
  await page.waitFor("the FAB to follow the live edge again",
    `document.querySelector('.map-fab').classList.contains('following') && document.querySelector('.sf-live').textContent === 'Live'`,
    { timeoutMs: 10000 });

  // Layers opens a menu and closes again.
  await page.click("document.querySelector('.map-layers-btn')");
  await page.waitFor("the layers menu to open", `!document.querySelector('#map-layers').hidden &&
    document.querySelectorAll('#map-layers input[type=checkbox]').length >= 2`, { timeoutMs: 5000 });
  await page.click("document.querySelector('.map-layers-btn')");
  await page.waitFor("the layers menu to close", `document.querySelector('#map-layers').hidden`, { timeoutMs: 5000 });

  // Go-to, far outside any tuned window: the view moves and the OFFER appears — nothing is sent.
  before = await page.eval(headline);
  await page.click("document.querySelector('.map-goto input')");
  await page.eval(`(() => { const i = document.querySelector('.map-goto input'); i.value = '2400M';
    document.querySelector('.map-goto').requestSubmit(); })()`);
  await page.waitFor("go-to to move the pane", `(${headline}) !== ${JSON.stringify(before)}`, { timeoutMs: 15000 });
  await page.waitFor("the retune offer to appear beside Go-to",
    `!document.querySelector('.map-offer').hidden && (document.querySelector('.map-offer-why').textContent ?? '').length > 0`,
    { timeoutMs: 5000 });
  await page.frames(3);

  const control = page.requests.filter((r) => /\/api\/control\/(center|rate|window|gains|bias_tee|baseband_filter)/.test(r.url));
  assert.deepEqual(control.map((r) => r.url), [], "a floating view control reached the front end");
  assert.deepEqual(page.exceptions, [], "uncaught exception while using the floating controls");
});

test("T-806: the layers menu has two axes, and a toggle changes only the active pane", async (t) => {
  // MAP-06 in a real browser: the menu lists base styles (radios, exactly one) and overlays
  // (checkboxes) for the ACTIVE pane. Split, then change the new pane's base style and hide its
  // detections and capture rules: the new pane's readouts say so, and closing it returns to pane 1,
  // whose registry is untouched. Nothing reaches a device route.
  const browser = await Browser.open();
  t.after(() => browser.close());
  const page = await browser.page();
  assert.equal(await page.goto(`${ORIGIN}/#token=${TOKEN}`), "load");
  await page.waitFor("the surface to draw and the floating controls to mount",
    `!!document.querySelector('.sf-canvas') && document.querySelector('.sf-canvas').width > 200 &&
     !!document.querySelector('.map-layers-btn') && /^IQ ring/.test(document.querySelector('.sf-ring')?.textContent ?? '')`,
    { timeoutMs: 60000 });
  const menu = `JSON.stringify({
    head: [...document.querySelectorAll('#map-layers h4')].map((e) => e.textContent),
    bases: [...document.querySelectorAll('#map-layers input[type=radio]')].map((i) => [i.value, i.checked]),
    overlays: [...document.querySelectorAll('#map-layers input[data-layer]')].map((i) => [i.dataset.layer, i.checked]),
    signals: document.querySelector('.sf-signalsbtn').getAttribute('aria-pressed'),
  })`;
  await page.click("document.querySelector('.map-layers-btn')");
  await page.waitFor("the layers menu to open", `!document.querySelector('#map-layers').hidden`, { timeoutMs: 5000 });
  const one = JSON.parse(await page.eval(menu));
  assert.deepEqual(one.bases, [["ramp", true], ["phosphor", false]], "base style: exactly one, ramp by default");
  assert.deepEqual(one.overlays, [["rules", true], ["detections", true]], "overlays in paint order, defaults on");
  assert.match(one.head[0], /Base style · this pane/);
  assert.match(one.head[1], /Overlays · this pane/);
  assert.equal(one.signals, "true");

  // Split: the new pane is active, inherits pane 1's registry, and the menu says which pane it is.
  await page.eval(`[...document.querySelectorAll('.sf-actions button')].find((b) => b.textContent.startsWith('Split')).click()`);
  await page.waitFor("the menu to act on pane 2", `/pane 2 of 2/.test(document.querySelector('#map-layers h4')?.textContent ?? '')`, { timeoutMs: 10000 });
  assert.deepEqual(JSON.parse(await page.eval(menu)).overlays, [["rules", true], ["detections", true]], "a split must inherit the registry");
  await page.click(`document.querySelector('#map-layers input[data-base="phosphor"]')`);
  await page.click(`document.querySelector('#map-layers input[data-layer="detections"]')`);
  await page.click(`document.querySelector('#map-layers input[data-layer="rules"]')`);
  await page.waitFor("pane 2's readouts to state its layers",
    `/hidden on this pane/.test(document.querySelector('.sf-ring').textContent) &&
     /phosphor style/.test(document.querySelector('.sf-trace').textContent) &&
     document.querySelector('.sf-signalsbtn').getAttribute('aria-pressed') === 'false'`, { timeoutMs: 10000 });
  const two = JSON.parse(await page.eval(menu));
  assert.deepEqual(two.bases, [["ramp", false], ["phosphor", true]]);
  assert.deepEqual(two.overlays, [["rules", false], ["detections", false]]);

  // Close pane 2: pane 1 is active again, and none of pane 2's toggles reached it.
  await page.eval(`[...document.querySelectorAll('.sf-actions button')].find((b) => b.textContent === 'Close').click()`);
  const said = `JSON.stringify({ head: document.querySelector('#map-layers h4')?.textContent,
    ring: document.querySelector('.sf-ring').textContent, trace: document.querySelector('.sf-trace').textContent })`;
  try {
    await page.waitFor("pane 1's own layers back in the menu and the readouts",
      `/this pane/.test(document.querySelector('#map-layers h4')?.textContent ?? '') &&
       /^IQ ring/.test(document.querySelector('.sf-ring').textContent) &&
       !/phosphor style/.test(document.querySelector('.sf-trace').textContent)`, { timeoutMs: 20000 });
  } catch (e) { t.diagnostic(`page said: ${await page.eval(said)}`); throw e; }
  const back = JSON.parse(await page.eval(menu));
  assert.deepEqual(back.bases, one.bases, "pane 2's base style leaked into pane 1");
  assert.deepEqual(back.overlays, one.overlays, "pane 2's overlay toggles leaked into pane 1");
  assert.equal(back.signals, "true");

  const control = page.requests.filter((r) => /\/api\/control\/(center|rate|window|gains|bias_tee|baseband_filter)/.test(r.url));
  assert.deepEqual(control.map((r) => r.url), [], "a layer toggle reached the front end");
  assert.deepEqual(page.exceptions, [], "uncaught exception while toggling layers");
});

test("T-506: the canvas draws the IQ horizon and the retention bound where the ring window says", async (t) => {
  // The retired Capture panel was the only place either boundary was drawn. This measures them on
  // the canvas in a real browser: the ink is found in the screenshot, row by row, and its position
  // is checked against the capture instants the page states it drew from — and those instants
  // against what `GET /api/timeline` itself reports.
  const browser = await Browser.open();
  t.after(() => browser.close());
  const page = await browser.page();
  assert.equal(await page.goto(`${ORIGIN}/#token=${TOKEN}`), "load");
  await page.waitFor("the canvas to draw and the ring readout to hold an IQ horizon",
    `!!document.querySelector('.sf-canvas') && document.querySelector('.sf-canvas').width > 200 &&
     !!document.querySelector('.sf-ring')?.dataset.iqS`, { timeoutMs: 60000 });

  // The capture clock's request is the window alone: no band, no t0/t1 (T-367: assert the request).
  const clockReqs = page.requests.filter((r) => r.url.includes("/api/timeline"));
  assert.ok(clockReqs.some((r) => r.url.endsWith("/api/timeline?columns=1&rows=1") && r.status === 200),
    `the capture clock never asked for the window: ${JSON.stringify(clockReqs.map((r) => r.url))}`);
  assert.ok(clockReqs.every((r) => !/[?&](t0|t1)=/.test(r.url)), "the window is the server's to state");

  // Whole surface: the extent now reaches the retained window, so both rules are on screen.
  // A REAL click, not `el.click()`: T-506 found "Whole surface" hit-tested to the range sentence
  // painted over it, so the button looked pressable and did nothing.
  await page.click(`[...document.querySelectorAll('.sf-actions button')].find((b) => b.textContent === "Whole surface")`);
  await page.waitFor("Whole surface to freeze the pane on the whole extent",
    `document.querySelector('.sf-ring')?.dataset.backing !== "live"`, { timeoutMs: 10000 });
  // Let the edge advance a little, so the retention bound (edge − retention) climbs off the frozen
  // pane's bottom edge and is measured inside the pane rather than against its border.
  await new Promise((r) => setTimeout(r, 3000));
  await page.frames(3);

  const read = () => page.eval(`(() => { const d = document.querySelector('.sf-ring').dataset;
    return { ret: +d.retentionS, iq: +d.iqS, edge: +d.edgeS, ringT0: d.ringT0S ? +d.ringT0S : null,
      dropT0: d.dropT0S ? +d.dropT0S : null,
      ringT1: d.ringT1S ? +d.ringT1S : null,
      t0: +d.paneT0S, t1: +d.paneT1S, top: +d.paneTopPx, h: +d.paneHPx,
      left: +d.paneLeftPx, w: +d.paneWPx, backing: d.backing, text: document.querySelector('.sf-ring').textContent }; })()`);
  const st = await read();
  t.diagnostic(`readout: ${st.text}`);
  assert.ok(st.t0 <= st.ret && st.ret <= st.t1, `the retention bound is not inside the pane: ${JSON.stringify(st)}`);
  assert.ok(st.t0 <= st.iq && st.iq <= st.t1, `the IQ horizon is not inside the pane: ${JSON.stringify(st)}`);

  // The instants are the server's: retention bound = the live edge − retention_s, IQ horizon =
  // buffered.t0_s (or later, once the ring is full).
  //
  // **Judged against the snapshot the page drew from, not against a later question** (the 09-22
  // gate red). The page draws both rules from the edge it has and the ring window it last polled
  // (`CAPTURE_CLOCK_MS`, 5 s); the server's ring moves on after that. Once the ring has been up
  // longer than its retention, its oldest sample advances — with the retention bound, and in
  // eviction steps (7.5 s measured on this fixture) — so a `GET /api/timeline` asked after the page
  // drew can report a newer `buffered.t0_s` than the horizon a correct page drew, and the old
  // `st.iq >= buffered.t0_s` then failed; the longer the gap between drawing and asking (load), the
  // likelier. Alone, this spec runs first against a ring seconds old whose oldest sample never
  // moves, which is why it only ever failed in a lane that ran it late. So the page states what it
  // derived the rules from (`data-edge-s`, `data-ring-t0-s`/`-t1-s`), the derivation is checked
  // exactly against those, and those inputs are checked against the server for what can hold
  // across the gap: the ring's oldest never moves backwards and never holds more than retention.
  const tl = await (await fetch(`${ORIGIN}/api/timeline?columns=1&rows=1`, { headers: { authorization: `Bearer ${TOKEN}` } })).json();
  const w = tl.window;
  t.diagnostic(`server window: retention_s ${w.retention_s}, t0 ${w.t0_s}, t1 ${w.t1_s}, buffered ${JSON.stringify(w.buffered)}`);
  if (w.buffered && st.ringT1 !== null) {
    t.diagnostic(`page drew from: edge ${st.edge}, ring ${st.ringT0}…${st.ringT1} (poll ${(w.t1_s - st.ringT1).toFixed(2)} s ` +
      `older than the server's answer); horizon ${st.iq} is ${(st.iq - w.buffered.t0_s).toFixed(3)} s from the server's ` +
      "later buffered.t0_s — the cross-snapshot gap the old assertion judged");
  }
  assert.ok(Math.abs((w.t1_s - w.retention_s) - st.ret) < 15, `retention bound ${st.ret} is not t1 − retention_s ${w.t1_s - w.retention_s}`);
  assert.ok(Number.isFinite(st.edge) && st.ringT0 !== null && st.ringT1 !== null,
    `the ring readout does not state the edge and ring window it drew from: ${JSON.stringify(st)}`);
  // The retention bound is the page's own edge − the server's retention, exactly.
  assert.ok(Math.abs(st.edge - w.retention_s - st.ret) < 1e-6,
    `retention bound ${st.ret} is not the drawn edge ${st.edge} − retention_s ${w.retention_s}`);
  // THE CLAIM: the horizon never claims IQ the ring (as the page last heard it) does not hold — it is
  // never older than the ring's oldest sample nor than the retention bound — and it is not newer
  // than both either, which would hide IQ that IS held.
  assert.ok(st.iq >= st.ringT0 - 1e-6 && st.iq >= st.ret - 1e-6,
    `the IQ horizon ${st.iq} claims IQ the ring does not hold: it drew from a ring holding ${st.ringT0}…${st.ringT1} ` +
    `with a retention bound at ${st.ret}`);
  // T-845: or the oldest sample a scheduled whole-slot drop the page applied leaves (`data-drop-t0-s`).
  assert.ok(Math.abs(st.iq - Math.max(st.ringT0, st.ret, st.dropT0 ?? -Infinity)) < 1e-6,
    `the IQ horizon ${st.iq} is not the newest of the ring's oldest sample ${st.ringT0}, the retention bound ${st.ret} ` +
    `and the applied ring drop ${st.dropT0}`);
  // …and that ring window is one this server served: its oldest sample only moves forward, and it
  // never holds more than the retention.
  assert.ok(w.buffered && st.ringT0 <= w.buffered.t0_s + 1e-6,
    `the page drew from a ring whose oldest sample ${st.ringT0} is NEWER than the server's later ${w.buffered?.t0_s}`);
  assert.ok(st.ringT1 - st.ringT0 <= w.retention_s + 1e-3,
    `the page drew from a ring holding ${st.ringT1 - st.ringT0} s, more than the ${w.retention_s} s retention`);

  // The pixels. Opaque inks (capture-window.ts): IQ horizon rgb(51,242,89), retention rgb(255,64,217).
  const canvas = await page.$rect(".sf-canvas");
  const img = await page.shot(path.join(ART, "app-surface-ring-rules.png"));
  const st2 = await read();
  const scale = img.width / (await page.eval("window.innerWidth"));
  const near = (d, c) => Math.abs(img.data[d] - c[0]) <= 8 && Math.abs(img.data[d + 1] - c[1]) <= 8 && Math.abs(img.data[d + 2] - c[2]) <= 8;
  const IQ = [51, 242, 89], RET = [255, 64, 217];
  const x0 = Math.round((canvas.x + st.left) * scale), x1 = Math.round((canvas.x + st.left + st.w) * scale);
  const rowsWith = (ink, share) => {
    const out = [];
    const y0 = Math.round((canvas.y + st.top) * scale), y1 = Math.round((canvas.y + st.top + st.h) * scale);
    for (let y = y0; y < y1; y++) {
      let n = 0;
      for (let x = x0; x < x1; x++) if (near((y * img.width + x) * 4, ink)) n++;
      if (n >= share * (x1 - x0)) out.push(y);
    }
    return out;
  };
  const iqRows = rowsWith(IQ, 0.6), retRows = rowsWith(RET, 0.3);
  t.diagnostic(`IQ-horizon ink rows ${JSON.stringify(iqRows)}; retention ink rows ${JSON.stringify(retRows)}`);
  assert.ok(iqRows.length > 0, "no full-width IQ-horizon line on the canvas");
  assert.ok(retRows.length > 0, "no retention-bound line on the canvas");
  assert.ok(iqRows[iqRows.length - 1] - iqRows[0] <= 6 * scale, "the IQ ink is one line, not a wash");
  assert.ok(retRows[retRows.length - 1] - retRows[0] <= 8 * scale, "the retention ink is one line, not a wash");

  // Where they should be: screen y from the pane's top is (t1 − t) / (t1 − t0) of its height.
  const expectY = (tS, s) => (canvas.y + s.top + ((s.t1 - tS) / (s.t1 - s.t0)) * s.h) * scale;
  const mid = (rows) => (rows[0] + rows[rows.length - 1]) / 2;
  for (const [name, rows, pick] of [["IQ horizon", iqRows, (s) => s.iq], ["retention bound", retRows, (s) => s.ret]]) {
    const lo = Math.min(expectY(pick(st), st), expectY(pick(st2), st2)) - 4 * scale;
    const hi = Math.max(expectY(pick(st), st), expectY(pick(st2), st2)) + 4 * scale;
    t.diagnostic(`${name}: measured y ${mid(rows).toFixed(1)} px, expected ${lo.toFixed(1)}…${hi.toFixed(1)}`);
    assert.ok(mid(rows) >= lo && mid(rows) <= hi, `${name} drawn at y ${mid(rows)}, expected ${lo}…${hi}`);
  }
  // A ring younger than its retention: the two boundaries are different instants and different rows,
  // with the IQ (newer) above the retention bound.
  if (st.iq - st.ret > 5) assert.ok(mid(iqRows) < mid(retRows) - 2, "the IQ horizon is drawn above (newer than) the retention bound");
  assert.match(st.text, /green line: oldest IQ/);
  assert.match(st.text, /magenta dashes: retention bound/);
  assert.deepEqual(page.exceptions, [], "uncaught exception");
});
