// T-1026 (user 2026-09-25 via the supervisor): the detail card is a Google-Maps PLACE CARD, in the
// real app over the replayed fixture, at 1280x800 and at 400 px.
//
//   "The bottom right accordion panel is kind of dumb to show all the time, it should be hidden all
//    the time until someone clicks on something, like on Google Maps how someone clicks something and
//    it opens the detailed view, and if they click on the back of the map it goes away, or if they
//    click on another interest point it changes."
//
// The journey, in the order the user described it:
//   (1) the first paint has NO card — nothing of it on screen, and nothing of it along the bottom edge;
//   (2) clicking a detection's BOX opens the card on that box's identity (the same centre frequency the
//       box's own accessible name states), pressed OFF THE BOX'S CENTRE so what is proved is a polygon
//       hit test (docs/23 §10.6 rule 6) rather than a click on a point;
//   (3) clicking another feature SWAPS the card's content — the card is never closed and re-opened in
//       between (a MutationObserver on `hidden` records every transition, and there must be none);
//   (4) Escape closes it, and (5) a click on bare map closes it — both leaving every pixel to the map;
//   (6) an inventory pill (T-997) opens it ON ITS LIST with no selection invented.
// Nothing above may reach a device route: selection is view state (docs/23 §10.4).
//
// The unit tier (`ui/test/app-card.test.ts`) proves the state machine, the dismiss gestures and the
// wiring over a fake DOM with a spy client. What needs a browser, and is only here, is the real hit
// test: which element a click at a point lands on, and that a click where no feature is lands on the
// map itself.
import test from "node:test";
import assert from "node:assert/strict";
import path from "node:path";
import { Browser } from "./harness.mjs";
import { settled } from "./app-chrome.mjs";

const ORIGIN = process.env.HK_E2E_ORIGIN, TOKEN = process.env.HK_E2E_TOKEN;
const SHOTS = process.env.HK_E2E_SHOTS;
const CONTROL = /\/api\/control\/(center|rate|window|gains|bias_tee|baseband_filter)/;
// The minimap strip along the canvas's bottom edge (`MINIMAP_PX` in `app/centre/surface.ts`): a click
// there is a click on the minimap, not on bare map.
const MINIMAP_PX = 110;

/** Everything about the card, in one read. */
const CARD = `JSON.stringify((() => {
  const el = document.querySelector('.sheet');
  if (!el) return { mounted: false };
  const r = el.getBoundingClientRect();
  return { mounted: true, hidden: !!el.hidden, open: el.dataset.open ?? null, snap: el.dataset.snap ?? null,
    h: Math.round(r.height), y: Math.round(r.y), bottom: Math.round(r.bottom),
    title: document.querySelector('.sheet-title')?.textContent ?? null,
    bigf: document.querySelector('.focus .detail .bigf')?.textContent ?? null,
    tab: document.querySelector('.side-inv .tab[aria-selected="true"]')?.dataset.tab ?? null };
})())`;
const card = async (page) => JSON.parse(await page.eval(CARD));

/** The detection box nearest the live edge, as its own invisible hit area over the drawn rectangle
 * (`surface/pins.ts`: `.sf-pin.detection.area` is placed exactly on the box). A box too small to be
 * drawn is generalized to a symbol (docs/23 §10.6 rule 6), which is then what there is to click. */
const BOX_AT = `(() => {
  const c = document.querySelector('.sf-canvas').getBoundingClientRect();
  let best = null;
  for (const p of document.querySelectorAll('.sf-pins .sf-pin.detection')) {
    const r = p.getBoundingClientRect();
    const inside = r.x > c.x + 2 && r.right < c.right - 2 && r.y > c.y + 2 && r.bottom < c.bottom - ${MINIMAP_PX} / (window.devicePixelRatio || 1);
    if (!inside) continue;
    const q = { id: p.dataset.pin, label: p.getAttribute('aria-label'), area: p.classList.contains('area'),
      x: r.x, y: r.y, w: r.width, h: r.height, cx: r.x + r.width / 2, cy: r.y + r.height / 2 };
    if (!best || q.y < best.y) best = q;
  }
  return best;
})()`;

/** Where to press for a box: a point INSIDE it but away from its centre (a polygon hit, not a
 * centre one) — a fifth of the way in from its top-left corner, with a 3 px floor so a thin box
 * still gets a point inside itself. A generalized symbol has only its own point. */
function pressPoint(box) {
  if (!box.area) return { x: box.cx, y: box.cy, offCentre: false };
  const dx = Math.max(3, Math.min(box.w / 5, box.w / 2 - 1));
  const dy = Math.max(3, Math.min(box.h / 5, box.h / 2 - 1));
  return { x: box.x + dx, y: box.y + dy, offCentre: box.w > 10 && box.h > 10 };
}

/** A point on the canvas that is BARE MAP: no pin/box hit area, no floating chrome, no card, not the
 * minimap strip — and the browser's own hit test agrees the canvas is what is there. */
const BARE_AT = `(() => {
  const c = document.querySelector('.sf-canvas').getBoundingClientRect();
  const dpr = window.devicePixelRatio || 1;
  const boxes = [...document.querySelectorAll('.sf-pins .sf-pin, .map-ctl > *, .sheet, .sf-readout, .sf-chrome, .sf-note, .sf-status-line, .sf-scale, .sf-maptip, .research:not([hidden])')]
    .map((e) => e.getBoundingClientRect()).filter((r) => r.width > 0 && r.height > 0);
  const free = (x, y) => !boxes.some((r) => x >= r.x - 8 && x <= r.right + 8 && y >= r.y - 8 && y <= r.bottom + 8);
  const y1 = c.bottom - ${MINIMAP_PX} / dpr - 8;
  for (let fy = 0.75; fy > 0.1; fy -= 0.05) {
    for (let fx = 0.5; fx < 0.98; fx += 0.04) {
      const x = c.x + c.width * fx, y = c.y + (y1 - c.y) * fy;
      if (!free(x, y)) continue;
      const top = document.elementFromPoint(x, y);
      if (top && top.classList.contains('sf-canvas')) return { x, y };
    }
  }
  return null;
})()`;

/** Every `hidden` transition of the card since `watchCard`, so a "swap" that secretly closed and
 * re-opened the card is caught (the user asked for it to CHANGE, not blink). */
const watchCard = (page) => page.eval(`(() => {
  const el = document.querySelector('.sheet');
  window.__cardHidden = [];
  new MutationObserver(() => window.__cardHidden.push(!!el.hidden)).observe(el, { attributes: true, attributeFilter: ['hidden'] });
  return true;
})()`);
const cardHiddenLog = (page) => page.eval("JSON.stringify(window.__cardHidden ?? [])").then(JSON.parse);

/** For a failure message. */
const STATE = `JSON.stringify({
  pins: [...document.querySelectorAll('.sf-pins .sf-pin')].map((p) => [p.className, p.dataset.pin?.slice(0, 8)]),
  pills: document.querySelector('.map-inv')?.textContent ?? null,
  rows: [...document.querySelectorAll('.side-inv .row[data-id]')].map((r) => r.dataset.id.slice(0, 8)),
  chrome: document.querySelector('.sf-chrome')?.textContent?.slice(0, 200) ?? null,
  where: document.querySelector('.sf-where')?.textContent?.slice(0, 200) ?? null, // T-996's one-line readout
  note: document.querySelector('.sf-note')?.textContent?.slice(0, 200) ?? null,
})`;

const mhz = (s) => (s ?? "").match(/[\d.]+/)?.[0] ?? null;

for (const [width, height] of [[1280, 800], [400, 800]]) {
  test(`at ${width}x${height} the detail card is hidden until a feature is clicked, and closes on bare map`, async (t) => {
    const browser = await Browser.open();
    t.after(() => browser.close());
    const page = await browser.page(undefined, { width, height });
    const shot = async (name) => { if (SHOTS) await page.shot(path.join(SHOTS, `card-${width}-${name}.png`)); };
    assert.equal(await page.goto(`${ORIGIN}/#token=${TOKEN}`), "load");
    // T-907: the surface's own mounted/failed event, before any other wait.
    await page.waitForSurfaceMounted({ timeoutMs: 60000 });
    await page.waitFor("the canvas, the card's host and the inventory pills",
      `!!document.querySelector('.sf-canvas') && document.querySelector('.sf-canvas').width > 200 &&
       !!document.querySelector('.sheet') && !!document.querySelector('.map-inv .map-pill')`, { timeoutMs: 60000 });

    // ---- (1) the default paint has no card ----
    const first = await card(page);
    t.diagnostic(`default: ${JSON.stringify(first)}`);
    assert.equal(first.hidden, true, "the map opened with the detail card already on it");
    assert.equal(first.open, "false");
    assert.equal(first.h, 0, "a hidden card still takes pixels — `.sheet[hidden]` is not `display: none`");
    // Nothing of it is on the bottom edge: the point where its strip used to sit is the map.
    const edge = await page.eval(`(() => { const c = document.querySelector('.sf-canvas').getBoundingClientRect();
      const top = document.elementFromPoint(c.x + c.width - 100, c.bottom - 80);
      return top ? (top.className || top.tagName) : 'nothing'; })()`);
    assert.doesNotMatch(String(edge), /sheet/, `the card still covers the bottom edge (${edge})`);
    await shot("1-no-card");

    // ---- (2) clicking a detection box opens the card on that box ----
    await page.waitFor("blind detection to draw a box on the canvas", `!!${BOX_AT}`, { timeoutMs: 120000, everyMs: 1000 })
      .catch(async (e) => { throw new Error(`${e.message}\nstate: ${await page.eval(STATE)}`); });
    let box = JSON.parse(await page.eval(`JSON.stringify(${BOX_AT})`));
    t.diagnostic(`box: ${JSON.stringify(box)}`);
    // The canvas is what a click there lands on (the hit areas do not take the gesture — T-809), so
    // the press below goes through the surface's own polygon hit test.
    const at0 = pressPoint(box);
    assert.equal(await page.eval(`document.elementFromPoint(${at0.x}, ${at0.y})?.classList.contains('sf-canvas')`), true,
      "the canvas is not under the box, so this press would not be a map hit test");

    let opened = null;
    for (let i = 0; i < 5 && !opened; i++) {
      box = JSON.parse(await page.eval(`JSON.stringify(${BOX_AT})`)) ?? box;
      const at = pressPoint(box);
      t.diagnostic(`press ${i} at ${JSON.stringify(at)} inside ${JSON.stringify(box)}`);
      await page.mouse("mouseMoved", at.x, at.y);
      await page.mouse("mousePressed", at.x, at.y, { buttons: 1, clickCount: 1 });
      await page.mouse("mouseReleased", at.x, at.y, { buttons: 0, clickCount: 1 });
      await page.frames(3);
      const c = await card(page);
      if (!c.hidden) opened = c;
      else await new Promise((r) => setTimeout(r, 250));
    }
    if (!opened) throw new Error(`clicking inside a detection box opened no card; state: ${await page.eval(STATE)}`);
    await page.waitFor("the card to name the selected signal and show its detail",
      `/^Selected signal/.test(document.querySelector('.sheet-title')?.textContent ?? '') &&
       !!document.querySelector('.focus .detail .bigf')`, { timeoutMs: 15000 });
    await settled(page, ".sheet", "the card's opening");
    const shown = await card(page);
    t.diagnostic(`card: ${JSON.stringify(shown)}`);
    assert.equal(shown.snap, "half", "the card opens at half, so what was clicked is visible");
    assert.ok(shown.h > 100, `the card is on screen (${shown.h} px tall)`);
    // The card is about the box that was clicked: same centre frequency the box's own name states.
    assert.equal(mhz(shown.bigf), mhz(box.label),
      `the card names ${shown.bigf}, the box clicked ${box.label}`);
    if (at0.offCentre) t.diagnostic("the press was inside the box but off its centre: a polygon hit test");
    await shot("2-card");

    // ---- (3) another feature swaps the card's content, without closing it ----
    await watchCard(page);
    const others = JSON.parse(await page.eval(`JSON.stringify([...document.querySelectorAll('.sf-pins .sf-pin.detection')]
      .filter((p) => p.dataset.pin !== ${JSON.stringify(box.id)}).map((p) => { const r = p.getBoundingClientRect();
        return { id: p.dataset.pin, label: p.getAttribute('aria-label'), area: p.classList.contains('area'),
          x: r.x, y: r.y, w: r.width, h: r.height, cx: r.x + r.width / 2, cy: r.y + r.height / 2 }; })
      .filter((q) => q.w > 0 && q.h > 0))`));
    if (others.length > 0) {
      const at = pressPoint(others[0]);
      await page.mouse("mousePressed", at.x, at.y, { buttons: 1, clickCount: 1 });
      await page.mouse("mouseReleased", at.x, at.y, { buttons: 0, clickCount: 1 });
      await page.waitFor("the card to swap to the other box",
        `/^Selected signal/.test(document.querySelector('.sheet-title')?.textContent ?? '') &&
         (document.querySelector('.focus .detail .bigf')?.textContent ?? '') !== ${JSON.stringify(shown.bigf)}`,
        { timeoutMs: 15000 });
      t.diagnostic(`swapped to the second box ${JSON.stringify(others[0].label)}`);
    } else {
      // One detection in this window: the other feature on the canvas is a region, marked with the
      // one gesture that never changes meaning (shift+drag, docs/23 §10.4). It is a feature with its
      // own box, so it is the same swap.
      const r = await page.$rect(".sf-canvas");
      const dpr = await page.eval("window.devicePixelRatio || 1");
      const bottom = r.y + r.h - MINIMAP_PX / dpr - 40;
      await page.drag({ x: r.x + r.w * 0.2, y: r.y + r.h * 0.25 }, { x: r.x + r.w * 0.34, y: bottom }, 10, { shift: true });
      await page.waitFor("the card to swap to the selected region",
        `/Selected region/.test(document.querySelector('.sheet-title')?.textContent ?? '')`, { timeoutMs: 20000 });
      t.diagnostic("swapped to a marked region (this window held one detection)");
    }
    assert.deepEqual((await cardHiddenLog(page)).filter((h) => h === true), [],
      "clicking another feature CLOSED the card and re-opened it, instead of changing its content");
    await shot("3-swapped");

    // ---- (4) Escape closes it ----
    await page.key("Escape");
    await page.waitFor("Escape to close the card", `document.querySelector('.sheet').hidden === true`, { timeoutMs: 5000 });
    const afterEsc = await card(page);
    assert.equal(afterEsc.h, 0, "Escape left some of the card on screen");
    await shot("4-escape");

    // ---- (5) a click on bare map closes it ----
    let reopened = null;
    for (let i = 0; i < 5 && !reopened; i++) {
      box = JSON.parse(await page.eval(`JSON.stringify(${BOX_AT})`)) ?? box;
      const at = pressPoint(box);
      await page.mouse("mousePressed", at.x, at.y, { buttons: 1, clickCount: 1 });
      await page.mouse("mouseReleased", at.x, at.y, { buttons: 0, clickCount: 1 });
      await page.frames(3);
      const c = await card(page);
      if (!c.hidden) reopened = c;
      else await new Promise((r) => setTimeout(r, 250));
    }
    assert.ok(reopened, "a dismissed card did not re-open when the same feature was clicked again");
    // T-958's rule, and the reason this failed at 400 px first time round: a point is only bare map
    // once the card has ARRIVED. Mid-rise its box is still most of the way down the screen, so a
    // point chosen then is canvas at that instant and under the card by the time the press lands.
    await settled(page, ".sheet", "the card's re-opening");
    const bare = JSON.parse(await page.eval(`JSON.stringify(${BARE_AT})`));
    assert.ok(bare, "no point of bare map on the canvas to click");
    t.diagnostic(`bare map at ${JSON.stringify(bare)}`);
    assert.equal(await page.eval(`document.elementFromPoint(${bare.x}, ${bare.y})?.classList.contains('sf-canvas')`), true,
      "the point chosen as bare map is not the canvas at the moment of the press");
    await page.mouse("mousePressed", bare.x, bare.y, { buttons: 1, clickCount: 1 });
    await page.mouse("mouseReleased", bare.x, bare.y, { buttons: 0, clickCount: 1 });
    await page.waitFor("the click on bare map to close the card",
      `document.querySelector('.sheet').hidden === true`, { timeoutMs: 10000 });
    assert.equal((await card(page)).h, 0, "a click on the back of the map left some of the card on screen");
    // ...and it gave the columns back: the point it covered is the canvas again.
    assert.equal(await page.eval(`document.elementFromPoint(${bare.x}, ${bare.y})?.classList.contains('sf-canvas')`), true);
    await shot("5-bare-map");

    // ---- (6) an inventory pill opens the card on its list ----
    for (const list of ["candidate", "confirmed"]) {
      await page.click(`document.querySelector('.map-inv .map-pill[data-list="${list}"]')`);
      await page.waitFor(`the pill to open the card on the ${list} list`,
        `document.querySelector('.sheet').hidden === false &&
         document.querySelector('.side-inv .tab[data-tab="${list}"]')?.getAttribute('aria-selected') === 'true'`,
        { timeoutMs: 10000 });
      await settled(page, ".sheet", `the card's rise for ${list}`);
      const c = await card(page);
      assert.equal(c.tab, list);
      assert.match(c.title ?? "", list === "candidate" ? /^Candidate signals/ : /^Confirmed signals/,
        `the card names the list it opened on (${c.title})`);
      // The list is inside the card's own box, not somewhere behind it.
      const inside = JSON.parse(await page.eval(`JSON.stringify((() => {
        const s = document.querySelector('.sheet').getBoundingClientRect();
        const inv = document.querySelector('.sheet .side-inv')?.getBoundingClientRect();
        return inv ? { top: Math.round(inv.top), sheetTop: Math.round(s.top), sheetBottom: Math.round(s.bottom) } : null; })())`));
      assert.ok(inside && inside.top >= inside.sheetTop && inside.top < inside.sheetBottom,
        `the ${list} list is not inside the card (${JSON.stringify(inside)})`);
    }
    await shot("6-list");
    // Its × closes it, from the list state as much as from a selection.
    await page.click("document.querySelector('.sheet .sheet-close')");
    await page.waitFor("the × to close the card", `document.querySelector('.sheet').hidden === true`, { timeoutMs: 5000 });

    assert.deepEqual(page.requests.filter((r) => CONTROL.test(r.url)).map((r) => r.url), [],
      "opening, swapping or closing the card reached a device route");
    assert.deepEqual(page.exceptions, [], "uncaught exception");
  });
}
