// T-993 (MMAP): the app-shell top bar RETIRES over the map. docs/23 §10 P1 (the map's pixels are
// the map's) and P4 (small controls for big view changes). User, 2026-09-25 on staging: the top bar
// with "the Explore/Decode/History tab buttons, the Review button, the Theme button, and the nudge
// buttons" was still there after T-801.
//
// What each assertion is a property of, at 1280 x 800 and at 400 x 820:
//  1. NO BAR — the page's own layout: the canvas's top row is the page's first pixel row
//     (`.sf-canvas` top == 0), the old `<header class="bar">` takes no box, and no chrome element
//     outside the canvas spans the width across the top (the shape of a bar, whatever its class).
//  2. REACHABLE — a REAL click (the harness clicks at the element's centre, so anything on top of it
//     takes the click instead) on every former bar action, in at most two clicks:
//       Explore/Decode/History (1 click each, the floating pill, and the framed bar back in Decode);
//       Review (1 click: the top-right cluster's icon button opens the drawer);
//       Theme (2 clicks: the cluster's ⋯, then Theme — the button's own label changes);
//       a tuning nudge (1 click, beside Go-to) reaches the device through the one gated DeviceAction
//       path — against the MOCK SDR device, the only way a nudge is enabled without a radio.
//  3. VIEW/DEVICE LINE — nothing but the nudge press reaches a device route.
//
// `HK_E2E_SHOTS=<dir>` saves a screenshot per width (and one of the ⋯ menu open).
import test, { after } from "node:test";
import assert from "node:assert/strict";
import path from "node:path";
import { Browser } from "./harness.mjs";
import { startBackend } from "./backend.mjs";

const SHOTS = process.env.HK_E2E_SHOTS ?? null;
const CONTROL = /\/api\/control\/(center|rate|window|gains|bias_tee|baseband_filter)/;

// Its own backend on the MOCK device (lane base + 24; the other second-backend specs use +16), so
// the nudges are live and enabled; a plain `--replay` states them disabled and proves only that half.
let backendP = null;
const backend = () => (backendP ??= startBackend({ port: Number(process.env.HK_E2E_PORT ?? 8791) + 24, mockDevice: true }));
after(async () => { (await backendP?.catch(() => null))?.stop(); });

/** Visible, top-of-page elements outside the canvas that span most of the width: a bar, by shape. */
const BAR_SHAPED = `JSON.stringify([...document.querySelectorAll('body *')].filter((e) => {
  const r = e.getBoundingClientRect();
  if (!r.width || !r.height || r.top > 60 || r.height > 120 || r.width < innerWidth * 0.8) return false;
  const cs = getComputedStyle(e);
  if (cs.visibility === 'hidden' || cs.display === 'none') return false;
  // The canvas and its own ancestors are the map, not chrome over it.
  return !e.contains(document.querySelector('.sf-canvas'));
}).map((e) => e.tagName + '.' + String(e.className?.baseVal ?? e.className)))`;

/** A real click lands on it: on screen, >= 24 px, and it is what a press at its centre hits. */
const pressable = (sel) => `JSON.stringify([...document.querySelectorAll(${JSON.stringify(sel)})].map((el) => {
  const r = el.getBoundingClientRect();
  const top = r.width && r.height ? document.elementFromPoint(r.x + r.width / 2, r.y + r.height / 2) : null;
  return { sel: String(el.id || el.className), w: Math.round(r.width), h: Math.round(r.height),
    covered: top ? String(top.className?.baseVal ?? top.className ?? top.tagName) : 'nothing',
    ok: !!top && (top === el || el.contains(top)) && r.width >= 24 && r.height >= 24 &&
        r.left >= 0 && r.top >= 0 && r.right <= innerWidth && r.bottom <= innerHeight };
}).filter((b) => !b.ok))`;

for (const [W, H] of [[1280, 800], [400, 820]]) test(`at ${W} px the top bar is gone and every one of its actions is a click or two away`, async (t) => {
  const be = await backend();
  const browser = await Browser.open();
  t.after(() => browser.close());
  const page = await browser.page(undefined, { width: W, height: H });
  assert.equal(await page.goto(`${be.origin}/#token=${be.token}`), "load");
  await page.waitForSurfaceMounted({ timeoutMs: 240000 });
  await page.waitFor("the bar's controls to float on the map",
    `!!document.querySelector('.map-status .modes') && !!document.querySelector('.map-topright #review-btn')
     && !!document.querySelector('.map-nudge .nudge-btn') && !!document.querySelector('.map-more-body #theme-btn')`, { timeoutMs: 120000 });
  await page.frames(5);
  if (SHOTS) await page.shot(path.join(SHOTS, `top-chrome-${W}.png`));

  // (1) No bar.
  const geo = JSON.parse(await page.eval(`JSON.stringify({
    canvasTop: document.querySelector('.sf-canvas').getBoundingClientRect().top,
    bar: (() => { const r = document.querySelector('.app > .bar').getBoundingClientRect(); return [r.width, r.height]; })(),
    barDisplay: getComputedStyle(document.querySelector('.app > .bar')).display,
    sw: Math.max(document.documentElement.scrollWidth, document.body.scrollWidth), iw: innerWidth })`));
  t.diagnostic(`at ${W}: ${JSON.stringify(geo)}`);
  assert.equal(geo.canvasTop, 0, "the canvas's top row is not the page's first pixel row");
  assert.equal(geo.barDisplay, "none", "the old top bar still takes a box over the map");
  assert.deepEqual(geo.bar, [0, 0]);
  assert.deepEqual(JSON.parse(await page.eval(BAR_SHAPED)), [], "a chrome element spans the width across the top — a bar by another name");
  assert.ok(geo.sw <= geo.iw, `the page scrolls sideways at ${W} px`);

  // Every former bar control on the map face is pressable where it sits (Theme is inside ⋯, below).
  const FACE = ".map-status .mode, .map-topright #review-btn, .map-topright .map-more-btn, .map-nudge .nudge-btn, .map-goto input, .map-topright button:not([hidden])";
  assert.deepEqual(JSON.parse(await page.eval(pressable(FACE))), [], "a former top-bar control is off screen, too small or covered");
  // Go-to never slides under the cluster, nor the nudges under the pill.
  const overlap = (a, b) => page.eval(`(() => { const A = document.querySelector(${JSON.stringify(a)}).getBoundingClientRect(),
    B = document.querySelector(${JSON.stringify(b)}).getBoundingClientRect();
    return A.left < B.right && A.right > B.left && A.top < B.bottom && A.bottom > B.top; })()`);
  for (const [a, b] of [[".map-goto", ".map-topright"], [".map-nudge", ".map-status"], [".map-goto", ".map-status"], [".map-status", ".map-topright"], [".map-nudge", ".map-topright"]]) {
    assert.equal(await overlap(a, b), false, `${a} overlaps ${b}`);
  }

  // (2) Review: one click on the cluster's icon button opens the drawer; its own Close shuts it.
  await page.click("document.querySelector('.map-topright #review-btn')");
  await page.waitFor("the review drawer to open", "!document.querySelector('#review').hidden && !!document.querySelector('#review .rv-head button')", { timeoutMs: 5000 });
  await page.click("document.querySelector('#review .rv-head button')");
  await page.waitFor("the review drawer to close", "document.querySelector('#review').hidden", { timeoutMs: 5000 });

  // Theme: two clicks — ⋯, then Theme. The button states the theme it set.
  const before = await page.$text("#theme-btn");
  await page.click("document.querySelector('.map-more-btn')");
  await page.waitFor("the ⋯ menu to open", "!document.querySelector('#map-more-menu').hidden", { timeoutMs: 5000 });
  if (SHOTS) await page.shot(path.join(SHOTS, `top-chrome-${W}-more.png`));
  await page.click("document.querySelector('#map-more-menu #theme-btn')");
  await page.waitFor("the theme to change", `document.querySelector('#theme-btn').textContent !== ${JSON.stringify(before)}`, { timeoutMs: 5000 });
  await page.click("document.querySelector('#map-more-menu .map-more-close')");
  await page.waitFor("the ⋯ menu to close", "document.querySelector('#map-more-menu').hidden", { timeoutMs: 5000 });

  // Modes: one click each. In Decode/History the framed shell's bar carries the switch back.
  await page.click("document.querySelector('.map-status .mode[data-mode=decode]')");
  await page.waitFor("Decode to show", "!document.querySelector('#view-decode').hidden && document.querySelector('#view-explore').hidden", { timeoutMs: 5000 });
  await page.click("document.querySelector('.app > .bar .mode[data-mode=history]')");
  await page.waitFor("History to show", "!document.querySelector('#view-history').hidden", { timeoutMs: 5000 });
  await page.click("document.querySelector('.app > .bar .mode[data-mode=explore]')");
  await page.waitFor("Explore to show with its floating switch back",
    "!document.querySelector('#view-explore').hidden && !!document.querySelector('.map-status .mode[data-mode=explore][aria-pressed=true]')", { timeoutMs: 5000 });
  assert.equal(await page.eval("document.querySelectorAll('#review-btn').length"), 1, "a control was copied, not moved");
  assert.deepEqual(page.requests.filter((r) => CONTROL.test(r.url)).map((r) => r.url), [],
    "Review, Theme or a mode switch reached a device route");

  // A nudge: one click beside Go-to, and it goes to the device (the one gated DeviceAction path).
  await page.waitFor("a nudge to be enabled (the mock device is live)",
    "!!document.querySelector('.map-nudge .nudge-btn[data-ideal]:not(:disabled)')", { timeoutMs: 60000 });
  await page.click("document.querySelector('.map-nudge .nudge-btn[data-ideal]:not(:disabled)')");
  const deadline = Date.now() + 15000;
  while (!page.requests.some((r) => CONTROL.test(r.url)) && Date.now() < deadline) await new Promise((r) => setTimeout(r, 100));
  assert.ok(page.requests.some((r) => CONTROL.test(r.url)), "the nudge press reached no device route");
  assert.deepEqual(page.exceptions, [], "uncaught exception");
});
