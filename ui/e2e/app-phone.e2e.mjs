// T-824 (MAP-24): the phone-width + fade/immersive pass, in the real app — what only a browser's own
// layout, hit test and touch pipeline can show. docs/23 §10.2 (fade) and §10.5 (400 px, touch).
//
//  1. At 400 px nothing scrolls the page sideways, there is no top bar (T-993 retired it; its controls
//     float as the nudge row and the mode/status pill), and every floating control (Go-to, the
//     top-right cluster, the nudges, the mode switch, zoom, the FAB, the inventory pills) is on screen,
//     at least 24 px, and what a press at its centre lands on — Go-to never under the top-right cluster.
//     The card (hidden until clicked — T-1026) covers none of the surface's statements.
//  2. Idle: after ~6 s untouched the floating chrome — the cluster (with the bar's former controls), the chip —
//     fades to ~35 %, while the sheet and the honesty statements do not; a touch brings it all back.
//  3. Sheets and menus stay reachable and closeable: a pill opens the card and its close shuts
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

// T-1026: the card's own handle and × are NOT in this list — the card is hidden until something is
// clicked, so before that they have no box at all. They are checked, with the same hit test, in (3)
// once a pill has opened the card (`CARD_CONTROLS`). T-1001: the retired FAB's place is each pane's
// own Live button.
const GUARDED = ".map-goto input, .map-topright button:not([hidden]), .map-nudge .nudge-btn, .map-status .mode, .map-zoom-in, .map-zoom-out, .sf-pane-live-btn, .map-inv .map-pill";
const CARD_CONTROLS = ".sheet-grab, .sheet-close";
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
// The active pane's frequency span, as the kept status line states it ("115.200 MHz ± 53.8 MHz").
// T-996: `.sf-where` — the per-viewport panel that used to carry this sentence is retired.
const PANE_SPAN = `(() => { const m = /±\\s*([\\d.]+)\\s*(k|M|G)?Hz/.exec(document.querySelector('.sf-where')?.textContent ?? '');
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
  await page.waitFor("the floating controls, the closed card and the inventory pills",
    `!!document.querySelector('.map-ctl .map-zoom-in') && !!document.querySelector('.sf-pane-live-btn') &&
     document.querySelector('.sheet')?.hidden === true &&
     !!document.querySelector('.map-inv .map-pill') && !!document.querySelector('.sf-where')?.textContent`, { timeoutMs: 240000 });
  await page.frames(5);
  await shot("1-open");

  // (1) Fits.
  const fit = JSON.parse(await page.eval(`JSON.stringify({
    sw: Math.max(document.documentElement.scrollWidth, document.body.scrollWidth), iw: innerWidth,
    bar: document.querySelector('.app > .bar').getBoundingClientRect().height,
    canvasTop: document.querySelector('.sf-canvas').getBoundingClientRect().top })`));
  t.diagnostic(`fit: ${JSON.stringify(fit)}`);
  assert.ok(fit.sw <= fit.iw, `the page scrolls sideways at ${W} px (${fit.sw} > ${fit.iw})`);
  // T-993: T-824's "the bar is one row" became "there is no bar": its pixels are the map's.
  assert.equal(fit.bar, 0, `the retired top bar is still ${fit.bar} px tall at ${W} px`);
  assert.equal(fit.canvasTop, 0, "the canvas's top row is not the page's first pixel row");
  assert.deepEqual(JSON.parse(await page.eval(unpressable(GUARDED))), [], "a floating control is off screen, too small or covered");
  assert.equal(await page.eval(overlap(".map-goto", ".map-topright, .map-status")), 0, "Go-to slides under the top-right cluster or the status pill");
  assert.equal(await page.eval(overlap(".map-nudge", ".map-topright, .map-status")), 0, "the nudges slide under the cluster or the status pill");
  // T-997: the inventory pills are a row of the same left stack — under everything above them, over
  // nothing, at phone width too.
  assert.equal(await page.eval(overlap(".map-inv", ".map-goto, .map-nudge, .map-topright, .map-status, .sheet")), 0,
    "the inventory pills collide with another piece of chrome at phone width");
  // T-1001: the FAB left this list with the FAB; the pane's own Live button took its place, and it
  // is inside the pane at the top — it must not land under the zoom stack or the top-right cluster.
  assert.equal(await page.eval(overlap(".map-zoom", ".map-topright")), 0, "the zoom stack collides with the top-right cluster");
  assert.equal(await page.eval(overlap(".sf-pane-live-btn", ".map-zoom, .map-topright, .map-goto")), 0,
    "the pane's Live button collides with the floating chrome");
  // T-1026: with nothing clicked there is no card at all, so the first thing to state is that —
  // a closed card cannot cover an honesty statement because it is not on screen.
  assert.equal(await page.eval("Math.round(document.querySelector('.sheet').getBoundingClientRect().height)"), 0,
    "the card is on screen before anything was clicked");
  // T-996: what is ON the picture at the bottom is the one status line and each pane's scale block.
  assert.equal(await page.eval(overlap(".sheet", ".sf-status-line, .sf-scale")), 0,
    "the card covers the status line or a pane's scale bar");

  // (2) Idle fade, and back on a touch.
  await page.waitFor("the chrome to go idle (~6 s)", "document.body.classList.contains('chrome-idle')", { timeoutMs: 15000 });
  await new Promise((r) => setTimeout(r, 800)); // the .5 s opacity transition
  await shot("2-idle");
  // T-994: the Outputs dock bar is retired — there is no bottom bar left to fade, and none at all
  // while nothing is open (the Active-outputs strip that replaced it exists only then).
  assert.equal(await page.eval(`document.querySelector('.app > .dock')`), null, "the Outputs dock bar is retired");
  assert.equal(await page.eval(`document.querySelector('.app > .out-strip').hidden`), true, "nothing open: no outputs strip");
  const idle = JSON.parse(await page.eval(`JSON.stringify({
    status: ${opacity(".map-status")}, nudge: ${opacity(".map-nudge")}, zoom: ${opacity(".map-zoom")},
    live: ${opacity(".sf-pane-live-btn")}, pills: ${opacity(".map-inv")}, sheet: ${opacity(".sheet")},
    where: ${opacity(".sf-status-line")}, scale: ${opacity(".sf-scale")} })`));
  t.diagnostic(`idle opacities: ${JSON.stringify(idle)}`);
  // T-994: the dock bar is retired, so there is no bottom bar left to fade.
  for (const k of ["status", "nudge", "zoom", "pills"]) assert.ok(idle[k] < 0.5, `${k} did not fade when idle (${idle[k]})`);
  // T-1001: a pane's Live button is also its statement of whether what it shows is live, and an
  // honesty statement never fades (docs/23 §10.2) — it is on the picture, not in the cluster.
  // T-996: the one status line and each pane's scale block are the honesty statements on the picture.
  for (const k of ["sheet", "where", "scale", "live"]) assert.equal(idle[k], 1, `${k} faded — the sheet and honesty statements never fade`);
  // T-1025: the wake-up touch lands on the DEVICE chip, found by its own box, not on a fixed
  // (200, 110) that happened to be over the Explore button while the status pill was one wide box.
  // The chips made that point "Decode" — the touch switched view, and everything after it was
  // asserted against the framed shell. A touch meant to say "a finger on the chrome" must not be a
  // press on whatever control the layout has moved under the coordinate.
  const wake = JSON.parse(await page.eval(`JSON.stringify((() => {
    const r = document.querySelector('.map-status #device').getBoundingClientRect();
    return [Math.round(r.x + r.width / 2), Math.round(r.y + r.height / 2)]; })())`));
  t.diagnostic(`wake touch at ${JSON.stringify(wake)}`);
  await touch("touchStart", [wake]);
  await touch("touchEnd", []);
  await page.waitFor("a touch to bring the chrome back", "!document.body.classList.contains('chrome-idle')", { timeoutMs: 5000 });

  // (3) Sheet and Research: reachable one-handed and closeable.
  // T-958: the close (×) and the grab handle ride the sheet's head, which travels ~313 px over the
  // .28 s height transition the press before it started — and `dataset.snap` flips at the START of
  // that move, not at its end. So a state wait that is followed by another press on the sheet is
  // followed by `settled` too, the page's own report that it has arrived; without it the press is
  // aimed where the button WAS and lands in the body below (1 run in 6 alone on a loaded box, and
  // on main).
  // T-1026: the grab handle only exists while the card is on screen, so what opens it at phone width
  // is the same small control as everywhere else — an inventory pill.
  const openCard = "document.querySelector('.map-inv .map-pill[data-list=\"confirmed\"]')";
  await page.click(openCard);
  await page.waitFor("the card to open", "document.querySelector('.sheet').hidden === false", { timeoutMs: 5000 });
  await settled(page, ".sheet", "the card's opening");
  assert.deepEqual(JSON.parse(await page.eval(unpressable(CARD_CONTROLS))), [],
    "the open card's handle or close is off screen, too small or covered at phone width");
  await shot("3-sheet");
  await page.click("document.querySelector('.sheet-close')");
  await page.waitFor("the card's close to take it off the screen", "document.querySelector('.sheet').hidden === true", { timeoutMs: 5000 });
  await settled(page, ".sheet", "the card's close");
  await page.click(openCard);
  await page.waitFor("the card to open again", "document.querySelector('.sheet').hidden === false", { timeoutMs: 5000 });
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
