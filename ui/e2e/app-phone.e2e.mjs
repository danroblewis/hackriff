// T-824 (MAP-24): the phone-width + fade/immersive pass, in the real app — what only a browser's own
// layout, hit test and touch pipeline can show. docs/23 §10.2 (fade) and §10.5 (400 px, touch).
//
//  1. At 400 px nothing scrolls the page sideways, the top bar is one row, and every floating control
//     (Go-to, the top-right cluster, zoom, the FAB, the sheet's handle, the lists' chip) is on screen,
//     at least 24 px, and what a press at its centre lands on — Go-to never under the top-right cluster.
//     The sheet's peek strip covers none of the surface's statements (coverage sentence, pane rows).
//  2. Idle: after ~6 s untouched the floating chrome — the cluster, the top bar, the dock, the chip —
//     fades to ~35 %, while the sheet and the honesty statements do not; a touch brings it all back.
//  3. Sheets and menus stay reachable and closeable: the sheet's handle opens it and its close shuts
//     it; Research opens as a full-height panel above the cluster, the sheet drops to peek, and its
//     close (×) dismisses it.
//  4. The view/device line survives touch: a two-finger pinch zooms the view (the pane's span
//     changes) and a held finger's drag marks a region (a selection is committed), and NOT ONE request
//     reaches a device route across the whole run.
//
// `HK_E2E_SHOTS=<dir>` saves a screenshot per step, for a person to look at.
import test from "node:test";
import assert from "node:assert/strict";
import path from "node:path";
import { Browser } from "./harness.mjs";
import { settled } from "./app-chrome.mjs";

const ORIGIN = process.env.HK_E2E_ORIGIN, TOKEN = process.env.HK_E2E_TOKEN;
const SHOTS = process.env.HK_E2E_SHOTS ?? null;
const CONTROL = /\/api\/control\/(center|rate|window|gains|bias_tee|baseband_filter)/;
const W = 400, H = 820;

const GUARDED = ".map-goto input, .map-topright button:not([hidden]), .map-zoom-in, .map-zoom-out, .map-fab, .sheet-grab, .side-chip";
// T-528's hit test at phone width: on screen, >= 24 px, and what a press at its centre lands on.
const unpressable = (sel) => `JSON.stringify([...document.querySelectorAll(${JSON.stringify(sel)})].map((el) => {
  const r = el.getBoundingClientRect();
  const top = r.width && r.height ? document.elementFromPoint(r.x + r.width / 2, r.y + r.height / 2) : null;
  return { cls: String(el.className?.baseVal ?? el.className ?? el.tagName), w: Math.round(r.width), h: Math.round(r.height),
           covered: top ? String(top.className?.baseVal ?? top.className ?? top.tagName) : 'nothing',
           ok: !!top && (top === el || el.contains(top)) && r.width >= 24 && r.height >= 24 &&
               r.left >= 0 && r.top >= 0 && r.right <= innerWidth && r.bottom <= innerHeight };
}).filter((b) => !b.ok))`;
const overlap = (a, b) => `(() => {
  const A = document.querySelector(${JSON.stringify(a)})?.getBoundingClientRect();
  return [...document.querySelectorAll(${JSON.stringify(b)})].map((e) => e.getBoundingClientRect())
    .filter((B) => A && B.width > 0 && B.height > 0 && A.left < B.right && A.right > B.left && A.top < B.bottom && A.bottom > B.top).length;
})()`;
const opacity = (sel) => `Number(getComputedStyle(document.querySelector(${JSON.stringify(sel)})).opacity)`;
// The active pane's frequency span, as the pane's own row states it ("115.200 MHz ± 53.8 MHz").
const PANE_SPAN = `(() => { const m = /±\\s*([\\d.]+)\\s*(k|M|G)?Hz/.exec(document.querySelector('.sf-chrome')?.textContent ?? '');
  return m ? Number(m[1]) * ({ k: 1e3, M: 1e6, G: 1e9 }[m[2]] ?? 1) : null; })()`;

test(`at ${W} px the floating chrome fits, fades when idle, and touch keeps to the view/device line`, async (t) => {
  const browser = await Browser.open();
  t.after(() => browser.close());
  const page = await browser.page(undefined, { width: W, height: H });
  const shot = async (name) => { if (SHOTS) await page.shot(path.join(SHOTS, `phone-${name}.png`)); };
  const touch = (type, points) => page.conn.send("Input.dispatchTouchEvent", {
    type, touchPoints: points.map(([x, y], id) => ({ x, y, id, radiusX: 4, radiusY: 4, force: 1 })),
  }, page.sessionId);
  await page.conn.send("Emulation.setTouchEmulationEnabled", { enabled: true, maxTouchPoints: 5 }, page.sessionId);
  assert.equal(await page.goto(`${ORIGIN}/#token=${TOKEN}`), "load");
  await page.waitForSurfaceMounted({ timeoutMs: 240000 });
  await page.waitFor("the floating controls, the sheet and the chip",
    `!!document.querySelector('.map-ctl .map-fab') && document.querySelector('.sheet')?.dataset.snap === 'peek' &&
     !!document.querySelector('.side-chip') && !!document.querySelector('.sf-chrome')`, { timeoutMs: 240000 });
  await page.frames(5);
  await shot("1-open");

  // (1) Fits.
  const fit = JSON.parse(await page.eval(`JSON.stringify({
    sw: Math.max(document.documentElement.scrollWidth, document.body.scrollWidth), iw: innerWidth,
    bar: document.querySelector('.app > .bar').getBoundingClientRect().height })`));
  t.diagnostic(`fit: ${JSON.stringify(fit)}`);
  assert.ok(fit.sw <= fit.iw, `the page scrolls sideways at ${W} px (${fit.sw} > ${fit.iw})`);
  assert.ok(fit.bar <= 48, `the top bar is ${fit.bar} px tall at ${W} px — it wrapped instead of being one row`);
  assert.deepEqual(JSON.parse(await page.eval(unpressable(GUARDED))), [], "a floating control is off screen, too small or covered");
  assert.equal(await page.eval(overlap(".map-goto", ".map-topright")), 0, "Go-to slides under the top-right cluster");
  assert.equal(await page.eval(overlap(".map-zoom", ".map-fab, .map-topright")), 0, "the zoom stack collides with the FAB or the top-right cluster");
  assert.equal(await page.eval(overlap(".sheet", ".sf-note, .sf-chrome, .sf-ring")), 0,
    "the sheet's peek strip covers one of the surface's honesty statements");

  // (2) Idle fade, and back on a touch.
  await page.waitFor("the chrome to go idle (~6 s)", "document.body.classList.contains('chrome-idle')", { timeoutMs: 15000 });
  await new Promise((r) => setTimeout(r, 800)); // the .5 s opacity transition
  await shot("2-idle");
  const idle = JSON.parse(await page.eval(`JSON.stringify({
    bar: ${opacity(".app > .bar")}, dock: ${opacity(".app > .dock")}, zoom: ${opacity(".map-zoom")},
    fab: ${opacity(".map-fab")}, chip: ${opacity(".side-chip")}, sheet: ${opacity(".sheet")},
    chrome: ${opacity(".sf-chrome")}, note: ${opacity(".sf-note")} })`));
  t.diagnostic(`idle opacities: ${JSON.stringify(idle)}`);
  for (const k of ["bar", "dock", "zoom", "fab", "chip"]) assert.ok(idle[k] < 0.5, `${k} did not fade when idle (${idle[k]})`);
  for (const k of ["sheet", "chrome", "note"]) assert.equal(idle[k], 1, `${k} faded — the sheet and honesty statements never fade`);
  await touch("touchStart", [[200, 110]]);
  await touch("touchEnd", []);
  await page.waitFor("a touch to bring the chrome back", "!document.body.classList.contains('chrome-idle')", { timeoutMs: 5000 });

  // (3) Sheet and Research: reachable one-handed and closeable.
  // T-958: the close (×) and the grab handle ride the sheet's head, which travels ~313 px over the
  // .28 s height transition the press before it started — and `dataset.snap` flips at the START of
  // that move, not at its end. So a state wait that is followed by another press on the sheet is
  // followed by `settled` too, the page's own report that it has arrived; without it the press is
  // aimed where the button WAS and lands in the body below (1 run in 6 alone on a loaded box, and
  // on main).
  await page.click("document.querySelector('.sheet-grab')");
  await page.waitFor("the sheet to open", "document.querySelector('.sheet').dataset.snap !== 'peek'", { timeoutMs: 5000 });
  await settled(page, ".sheet", "the sheet's opening");
  await shot("3-sheet");
  await page.click("document.querySelector('.sheet-close')");
  await page.waitFor("the sheet's close to collapse it", "document.querySelector('.sheet').dataset.snap === 'peek'", { timeoutMs: 5000 });
  await settled(page, ".sheet", "the sheet's collapse");
  await page.click("document.querySelector('.sheet-grab')");
  await page.waitFor("the sheet to open again", "document.querySelector('.sheet').dataset.snap !== 'peek'", { timeoutMs: 5000 });
  await page.click("document.querySelector('.map-research-btn')");
  await page.waitFor("Research to open, and the sheet to drop to peek",
    "!document.querySelector('.research').hidden && document.querySelector('.sheet').dataset.snap === 'peek'", { timeoutMs: 5000 });
  await page.frames(3);
  await shot("4-research");
  const research = JSON.parse(await page.eval(`JSON.stringify((() => {
    const r = document.querySelector('.research').getBoundingClientRect();
    const under = document.elementFromPoint(r.right - 30, r.top + r.height / 2);
    return { w: r.width, h: r.height, clusterOnTop: !!under?.closest('.map-ctl') };
  })())`));
  t.diagnostic(`research: ${JSON.stringify(research)}`);
  assert.ok(research.w >= W - 24, `Research is ${research.w} px wide at ${W} px, not the full-width panel`);
  assert.ok(research.h >= H * 0.7, `Research is ${research.h} px tall, not a full-height panel`);
  assert.equal(research.clusterOnTop, false, "the floating cluster paints over the open Research panel");
  assert.deepEqual(JSON.parse(await page.eval(unpressable(".research-x"))), [], "Research's close is not pressable");
  await page.click("document.querySelector('.research-x')");
  await page.waitFor("Research's close to dismiss it", "document.querySelector('.research').hidden", { timeoutMs: 5000 });

  // (4) Touch: pinch = zoom (view); held drag = region (a selection, the input to the retune OFFER).
  const stage = JSON.parse(await page.eval(`JSON.stringify(document.querySelector('.sf-canvas').getBoundingClientRect())`));
  const cx = Math.round(stage.x + stage.width / 2), cy = Math.round(stage.y + stage.height * 0.4);
  const span0 = await page.eval(PANE_SPAN);
  await touch("touchStart", [[cx - 30, cy], [cx + 30, cy]]);
  for (let k = 1; k <= 6; k++) await touch("touchMove", [[cx - 30 - 12 * k, cy], [cx + 30 + 12 * k, cy]]);
  await touch("touchEnd", []);
  await page.frames(5);
  const span1 = await page.eval(PANE_SPAN);
  t.diagnostic(`pinch: pane span ${span0} -> ${span1}`);
  assert.ok(span0 !== null && span1 !== null, "the pane's row does not state its span");
  assert.ok(span1 < span0, `spreading two fingers did not zoom the view in (${span0} -> ${span1})`);

  await touch("touchStart", [[cx - 40, cy - 30]]);
  await new Promise((r) => setTimeout(r, 700)); // held past HOLD_TO_MARK_MS
  for (let k = 1; k <= 6; k++) await touch("touchMove", [[cx - 40 + 12 * k, cy - 30 + 8 * k]]);
  await touch("touchEnd", []);
  await page.waitFor("the held drag to commit a region and focus it",
    "/Selected region/.test(document.querySelector('.sheet .sheet-title')?.textContent ?? '')", { timeoutMs: 15000 });
  await shot("5-region");

  assert.deepEqual(page.requests.filter((r) => CONTROL.test(r.url)).map((r) => r.url), [],
    "a phone-width layout, fade, sheet, menu or touch gesture reached a device route");
  assert.deepEqual(page.exceptions, []);
});
