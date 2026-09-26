// T-1000 (MMAP split view, docs/23 §10.7): the ACTIVE pane, made visible — in the real app, over the
// product's own server, at a desktop width and a phone width.
//
// Before this, Go-to, zoom, the layers menu, the (since retired, T-1001) follow-live FAB and the
// viewport menu acted on a
// HIDDEN active pane (whichever was pressed or wheeled last) and a right-click did not even change
// it. With two panes open the user could not tell which pane a press of the chrome would move.
//
// With two panes, each of these must move the outline AND the named chrome inside the very event
// dispatch that caused it (checked by a listener that runs after the canvas's own handler, on the
// same event — so before any render frame could have caught up):
//   1. a plain press on the other pane;
//   2. a RIGHT-click on the other pane;
//   3. the keys: `[` / `]` step, `1`-`9` pick, and `L` toggles Live on the ACTIVE pane only.
// And the outline sits on the pane it names; with one pane there is no outline and no badge; and
// none of it reaches a device route (docs/23 §10.4).
import test from "node:test";
import assert from "node:assert/strict";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { Browser } from "./harness.mjs";
import { paneAct } from "./app-chrome.mjs";

const ORIGIN = process.env.HK_E2E_ORIGIN, TOKEN = process.env.HK_E2E_TOKEN;
const CONTROL = /\/api\/control\/(center|rate|window|gains|bias_tee|baseband_filter)/;
const ART = path.join(path.dirname(fileURLToPath(import.meta.url)), "artifacts");

/** Everything the chrome says about the active pane, plus the outline's box, in one round trip. */
const NAMED = `JSON.stringify((() => {
  const o = document.querySelector('.sf-active-pane');
  const r = o.getBoundingClientRect();
  const q = (s) => document.querySelector(s);
  return {
    outline: o.hidden ? null : o.dataset.pane, outlineId: o.hidden ? null : o.dataset.paneId,
    box: { x: r.x, y: r.y, w: r.width, h: r.height },
    goto: q('.map-goto-pane').hidden ? null : q('.map-goto-pane').textContent,
    gotoLabel: q('.map-goto input').getAttribute('aria-label'),
    layers: q('.map-layers-btn').getAttribute('aria-label'),
    zoom: q('.map-zoom').getAttribute('aria-label'),
    zoomBadge: q('.map-zoom-pane').hidden ? null : q('.map-zoom-pane').textContent,
    // T-1001: Live/Freeze is per pane, inside each pane's rectangle — one button per pane, each
    // naming its own pane, so the chrome's active-pane name is not the one that says "Live".
    live: [...document.querySelectorAll('.sf-pane-live-btn')].map((b) => b.dataset.pane),
  };
})())`;

/** Install listeners that record what the chrome says at the END of the same event dispatch that
 * reached the canvas (document, bubble phase: after the canvas's own handler, before any frame). */
const RECORDER = `(() => {
  window.__t1000 = [];
  const rec = (ev) => { const o = document.querySelector('.sf-active-pane');
    window.__t1000.push({ type: ev.type, button: ev.button ?? null, key: ev.key ?? null,
      outline: o.hidden ? null : o.dataset.pane, goto: document.querySelector('.map-goto-pane').textContent,
      layers: document.querySelector('.map-layers-btn').getAttribute('aria-label'),
      following: document.querySelector('.sf-pane-live-btn').classList.contains('following') }); };
  for (const t of ['pointerdown', 'contextmenu', 'keydown']) document.addEventListener(t, rec);
  return true; })()`;
const LAST = (type) => `JSON.stringify((window.__t1000 || []).filter((r) => r.type === ${JSON.stringify(type)}).at(-1) ?? null)`;

/** A point inside the canvas, in page px, within [x0, x1] and away from the floating chrome: the
 * browser's own hit test must land on the canvas there, so a press reaches the pane. */
const clearPoint = (x0, x1) => `JSON.stringify((() => {
  const c = document.querySelector('.sf-canvas'), r = c.getBoundingClientRect();
  for (const fy of [0.45, 0.35, 0.55, 0.3, 0.6, 0.25, 0.65]) for (const fx of [0.5, 0.35, 0.65, 0.25, 0.75]) {
    const x = ${x0} + (${x1} - ${x0}) * fx, y = r.y + r.height * fy;
    if (document.elementFromPoint(x, y) === c) return { x, y };
  }
  return null; })())`;

/** Each pane's follow state, from its own status row (`data-following`, written per frame). */
const FOLLOWS = `JSON.stringify(Object.fromEntries([...document.querySelectorAll('.hk-surface-viewport[data-viewport="pane"]')]
  .map((r) => [r.querySelector('.hk-surface-id').textContent, r.dataset.following === 'true'])))`;

for (const [width, height] of [[1280, 800], [400, 820]]) test(`at ${width} px: the active pane is outlined and named; click, right-click and keys move both at once`, async (t) => {
  const browser = await Browser.open();
  t.after(() => browser.close());
  const page = await browser.page(undefined, { width, height });
  assert.equal(await page.goto(`${ORIGIN}/#token=${TOKEN}`), "load");
  await page.waitForSurfaceMounted({ timeoutMs: 60000 });
  await page.waitFor("the floating cluster to mount", "!!document.querySelector('.map-pane-btn') && !!document.querySelector('.sf-active-pane')", { timeoutMs: 30000 });
  await page.waitFor("a pane row with its level stated",
    "!!document.querySelector('.hk-surface-viewport[data-viewport=\"pane\"] .hk-surface-level')?.textContent", { timeoutMs: 90000 });

  // One pane: nothing to disambiguate, so no outline and no badge (docs/23 §10.6 P1).
  const one = JSON.parse(await page.eval(NAMED));
  assert.equal(one.outline, null, "an outline is drawn around the only pane");
  assert.equal(one.goto, null, "Go-to names a pane when there is only one");
  assert.equal(one.zoomBadge, null);
  assert.equal(one.layers, "Layers");
  // T-1001: with one pane its Live button says just "Live" — there is nothing to disambiguate.
  assert.deepEqual(one.live, [""], `one pane, one unnumbered Live button: ${JSON.stringify(one.live)}`);

  // Split: the new (right-hand) pane is active, and every per-pane control says so.
  await paneAct(page, "split");
  await page.waitFor("the outline on the new pane", "document.querySelector('.sf-active-pane').dataset.pane === '2' && !document.querySelector('.sf-active-pane').hidden", { timeoutMs: 10000 });
  await page.frames(3);
  const two = JSON.parse(await page.eval(NAMED));
  t.diagnostic(`split, pane 2 active: ${JSON.stringify(two)}`);
  assert.deepEqual(
    { goto: two.goto, gotoLabel: two.gotoLabel, layers: two.layers, zoom: two.zoom, zoomBadge: two.zoomBadge },
    { goto: "pane 2", gotoLabel: "Go to frequency · pane 2 of 2", layers: "Layers · pane 2 of 2", zoom: "Zoom · pane 2 of 2", zoomBadge: "2" },
    "the per-pane chrome does not name the active pane");
  // T-1001: Live/Freeze is NOT part of that chrome — each pane has its own button, inside itself,
  // each naming its own pane. Two panes, two buttons, numbered in layout order.
  assert.deepEqual(two.live, ["1", "2"],
    `each pane must carry its own Live button, named for its own pane: ${JSON.stringify(two.live)}`);
  const canvas = await page.$rect(".sf-canvas");
  // The outline is ON the pane it names: pane 2 of a column split is the right half.
  assert.ok(two.box.x > canvas.x + canvas.w * 0.4 && two.box.x + two.box.w <= canvas.x + canvas.w + 1,
    `pane 2's outline is not on the right-hand pane: ${JSON.stringify(two.box)} in ${JSON.stringify(canvas)}`);
  assert.ok(two.box.w > canvas.w * 0.3 && two.box.h > 40, `the outline is not a pane-sized box: ${JSON.stringify(two.box)}`);
  await page.shot(path.join(ART, `app-active-pane-${width}-split.png`));

  await page.eval(RECORDER);
  const left = JSON.parse(await page.eval(clearPoint(canvas.x + 4, canvas.x + canvas.w * 0.45)));
  const right = JSON.parse(await page.eval(clearPoint(canvas.x + canvas.w * 0.55, canvas.x + canvas.w - 4)));
  assert.ok(left && right, `no clear point on the canvas to press: ${JSON.stringify({ left, right })}`);

  // (1) A plain press on pane 1 moves the outline and the named chrome in the SAME dispatch.
  await page.mouse("mousePressed", left.x, left.y, { buttons: 1, clickCount: 1 });
  const pressed = JSON.parse(await page.eval(LAST("pointerdown")));
  await page.mouse("mouseReleased", left.x, left.y, { buttons: 0, clickCount: 1 });
  assert.deepEqual({ outline: pressed?.outline, goto: pressed?.goto, layers: pressed?.layers },
    { outline: "1", goto: "pane 1", layers: "Layers · pane 1 of 2" },
    `a press on pane 1 did not move the outline and the chrome in the same event: ${JSON.stringify(pressed)}`);
  await page.frames(2);
  const onOne = JSON.parse(await page.eval(NAMED));
  assert.ok(onOne.box.x + onOne.box.w < canvas.x + canvas.w * 0.6, `pane 1's outline is not on the left pane: ${JSON.stringify(onOne.box)}`);

  // (2) A RIGHT-click on pane 2 does the same (it did not, before this ticket). The point is found
  // again: the press above may have selected a mark and raised the sheet over part of the canvas.
  const right2 = JSON.parse(await page.eval(clearPoint(canvas.x + canvas.w * 0.55, canvas.x + canvas.w - 4)));
  assert.ok(right2, "no clear point left on pane 2 to right-click");
  Object.assign(right, right2);
  await page.mouse("mousePressed", right.x, right.y, { button: "right", buttons: 2, clickCount: 1 });
  const rdown = JSON.parse(await page.eval(LAST("pointerdown")));
  await page.mouse("mouseReleased", right.x, right.y, { button: "right", buttons: 0, clickCount: 1 });
  assert.equal(rdown?.button, 2, `the right press did not arrive as button 2: ${JSON.stringify(rdown)}`);
  assert.deepEqual({ outline: rdown.outline, goto: rdown.goto, layers: rdown.layers },
    { outline: "2", goto: "pane 2", layers: "Layers · pane 2 of 2" },
    `a right-click on pane 2 did not make it active in the same event: ${JSON.stringify(rdown)}`);
  // Close any context menu a mark under the pointer may have opened, so the keys reach the page.
  await page.key("Escape");

  // (3) Keys. `[` steps back to pane 1, `]` forward to pane 2, `1` picks pane 1.
  for (const [key, want] of [["[", "1"], ["]", "2"], ["1", "1"]]) {
    await page.key(key);
    const k = JSON.parse(await page.eval(LAST("keydown")));
    assert.equal(k?.outline, want, `key ${key} did not make pane ${want} active in the same event: ${JSON.stringify(k)}`);
    assert.equal(k?.goto, `pane ${want}`, `key ${key}: Go-to still names another pane`);
  }

  // `L` toggles Live on the ACTIVE pane (pane 1) and leaves pane 2 alone.
  const ids = JSON.parse(await page.eval(`JSON.stringify([...document.querySelectorAll('.hk-surface-viewport[data-viewport="pane"] .hk-surface-id')].map((e) => e.textContent))`));
  const activeId = await page.eval("document.querySelector('.sf-active-pane').dataset.paneId");
  const otherId = ids.find((i) => i !== activeId);
  assert.ok(activeId && otherId, `two pane rows expected: ${JSON.stringify({ ids, activeId })}`);
  const before = JSON.parse(await page.eval(FOLLOWS));
  await page.key("l", { code: "KeyL", keyCode: 76 });
  await page.waitFor("L to flip the active pane's follow state",
    `(${FOLLOWS.replace(/^JSON\.stringify/, "")})[${JSON.stringify(activeId)}] === ${!before[activeId]}`, { timeoutMs: 10000 });
  const after = JSON.parse(await page.eval(FOLLOWS));
  t.diagnostic(`L: ${JSON.stringify({ before, after, activeId })}`);
  assert.equal(after[otherId], before[otherId], "L on pane 1 changed pane 2's follow state");
  // T-1001: the ACTIVE pane's own button — the one `L` pressed — states that pane's follow state,
  // and the other pane's button still states its own (unchanged) one.
  const stated = JSON.parse(await page.eval(`JSON.stringify(Object.fromEntries(
    [...document.querySelectorAll('.sf-pane-live-btn')].map((b) => [b.dataset.paneId, b.classList.contains('following')])))`));
  t.diagnostic(`the panes' Live buttons: ${JSON.stringify(stated)}`);
  assert.equal(stated[activeId], after[activeId], "the active pane's Live button does not state its own follow state");
  assert.equal(stated[otherId], after[otherId], "the other pane's Live button does not state its own follow state");
  // And back, so a second press is the button's other half.
  await page.key("l", { code: "KeyL", keyCode: 76 });
  await page.waitFor("L again to restore it",
    `(${FOLLOWS.replace(/^JSON\.stringify/, "")})[${JSON.stringify(activeId)}] === ${before[activeId]}`, { timeoutMs: 10000 });

  // Typing into Go-to is typing: an "l" or "2" there switches nothing.
  await page.click("document.querySelector('.map-goto input')");
  const typedBefore = await page.eval("document.querySelector('.sf-active-pane').dataset.pane");
  await page.key("2", { code: "Digit2", keyCode: 50 });
  assert.equal(await page.eval("document.querySelector('.sf-active-pane').dataset.pane"), typedBefore, "a digit typed into Go-to switched panes");
  await page.eval("document.activeElement.blur(), true");

  await page.frames(3);
  await page.shot(path.join(ART, `app-active-pane-${width}-pane1.png`));

  // None of it touched the front end: choosing a pane is view state (docs/23 §10.4).
  assert.deepEqual(page.requests.filter((r) => CONTROL.test(r.url)).map((r) => r.url), [],
    "choosing the active pane reached a device route");
  assert.deepEqual(page.exceptions, [], "uncaught exception");
});
