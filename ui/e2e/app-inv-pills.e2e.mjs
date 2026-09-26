// T-997 (user 2026-09-25, via the supervisor): the inventory counts, in the real app.
//
// WHAT THE USER SAW. The T-895 chip — a hamburger plus "1 cand, 1 conf" — floated at the LEFT EDGE,
// MID-HEIGHT, which is where each pane's time ruler prints its labels: "I don't like where this is
// placed, in the vertical center, it overlaps the timeline markers and isn't very useful."
//
// WHAT THIS PROVES, at the two widths the ticket names (1280 x 800 and 400 px), and only a browser's
// own layout can: the counts are two pills docked TOP-LEFT under Go-to, their box intersects no
// time-ruler label and no other piece of chrome, each pill is pressable, and pressing one OPENS THE
// BOTTOM SHEET ON ITS LIST (that list's tab selected, its rows on screen inside the sheet). Nothing
// here reaches a device route. The unit tier (`ui/test/app-inv-pills.test.ts`) proves the counts,
// the labels and the wiring; this is the placement the unit tier cannot see.
import test from "node:test";
import assert from "node:assert/strict";
import path from "node:path";
import { Browser } from "./harness.mjs";
import { settled } from "./app-chrome.mjs";

const ORIGIN = process.env.HK_E2E_ORIGIN, TOKEN = process.env.HK_E2E_TOKEN;
const SHOTS = process.env.HK_E2E_SHOTS ?? null;
const CONTROL = /\/api\/control\/(center|rate|window|gains|bias_tee|baseband_filter)/;

/** Every other piece of chrome the pills share the screen with. */
const OTHER_CHROME = ".map-goto, .map-nudge, .map-status, .map-topright, .map-zoom, .sf-pane-live-btn, .map-offer:not([hidden]), .map-mode:not([hidden]), .sheet";

/** Boxes of everything `sel` matches that is actually drawn, with a name. */
const rects = (sel) => `JSON.stringify([...document.querySelectorAll(${JSON.stringify(sel)})]
  .map((e) => ({ cls: String(e.className?.baseVal ?? e.className ?? e.tagName), text: (e.textContent ?? '').trim().slice(0, 40),
                 r: e.getBoundingClientRect().toJSON() }))
  .filter((b) => b.r.width > 0 && b.r.height > 0))`;

/** What `sel`'s box overlaps among `others`, by name — the ticket's bounding-rect test. */
const intersects = (sel, others) => `JSON.stringify((() => {
  const a = document.querySelector(${JSON.stringify(sel)})?.getBoundingClientRect();
  if (!a) return [{ cls: 'MISSING', note: ${JSON.stringify(sel)} }];
  return [...document.querySelectorAll(${JSON.stringify(others)})]
    .map((e) => ({ cls: String(e.className?.baseVal ?? e.className ?? e.tagName), text: (e.textContent ?? '').trim().slice(0, 30), r: e.getBoundingClientRect() }))
    .filter(({ r }) => r.width > 0 && r.height > 0 && a.left < r.right && a.right > r.left && a.top < r.bottom && a.bottom > r.top)
    .map(({ cls, text, r }) => ({ cls, text, r: { x: Math.round(r.x), y: Math.round(r.y), w: Math.round(r.width), h: Math.round(r.height) } }));
})())`;

/** Is a press at each pill's centre the pill itself? (T-528's hit test, per pill.) */
const UNPRESSABLE = `JSON.stringify([...document.querySelectorAll('.map-inv .map-pill')].map((el) => {
  const r = el.getBoundingClientRect();
  const top = r.width && r.height ? document.elementFromPoint(r.x + r.width / 2, r.y + r.height / 2) : null;
  return { list: el.dataset.list, w: Math.round(r.width), h: Math.round(r.height),
           covered: top ? String(top.className?.baseVal ?? top.className ?? top.tagName) : 'nothing',
           ok: !!top && (top === el || el.contains(top)) && r.width >= 24 && r.height >= 24 };
}).filter((b) => !b.ok))`;

for (const [width, height] of [[1280, 800], [400, 800]]) {
  test(`at ${width}x${height} the inventory pills dock top-left, clear of the time ruler, and open the sheet on their list`, async (t) => {
    const browser = await Browser.open();
    t.after(() => browser.close());
    const page = await browser.page(undefined, { width, height });
    const shot = async (name) => { if (SHOTS) await page.shot(path.join(SHOTS, `inv-pills-${width}-${name}.png`)); };
    assert.equal(await page.goto(`${ORIGIN}/#token=${TOKEN}`), "load");
    // T-907: the surface's own mounted/failed event (`data-surface`), before any other wait.
    await page.waitForSurfaceMounted({ timeoutMs: 60000 });
    await page.waitFor("the surface, its floating chrome, the sheet and both pills",
      `!!document.querySelector('.map-topright button') && !!document.querySelector('.map-ctl .map-zoom-in') &&
       document.querySelector('.sheet')?.hidden === true &&
       document.querySelectorAll('.map-inv .map-pill').length === 2 &&
       document.querySelectorAll('.sf-hud-label.time').length > 0`, { timeoutMs: 60000 });
    await page.frames(3);
    await shot("1-docked");

    // (0) The retired chrome is GONE, not restyled: no chip, no floating left column.
    assert.equal(await page.eval("document.querySelectorAll('.side-chip, .side-close').length"), 0,
      "the retired hamburger chip is still in the page");

    // (1) Docked TOP-LEFT, under Go-to — never at mid-height, which is what the user rejected.
    const box = await page.$rect(".map-inv");
    const goto = await page.$rect(".map-goto");
    t.diagnostic(`pills ${JSON.stringify(box)} · go-to ${JSON.stringify(goto)}`);
    assert.ok(box.h > 0 && box.w > 0, "the pills row is not drawn");
    assert.ok(box.y >= goto.y + goto.h, `the pills are not under Go-to (${box.y} < ${goto.y + goto.h})`);
    assert.ok(box.x < 24, `the pills are not docked to the left edge (x = ${box.x})`);
    assert.ok(box.y + box.h < height / 3, `the pills are not in the top third (${box.y + box.h} of ${height})`);

    // (2) THE TICKET'S TEST: the pills' box intersects no time-ruler label, and no other chrome.
    const onRuler = JSON.parse(await page.eval(intersects(".map-inv", ".sf-hud-label.time")));
    t.diagnostic(`time labels drawn: ${await page.eval("document.querySelectorAll('.sf-hud-label.time:not([hidden])').length")}; under the pills: ${JSON.stringify(onRuler)}`);
    assert.deepEqual(onRuler, [], "the pills overlap the time ruler's labels");
    const onChrome = JSON.parse(await page.eval(intersects(".map-inv", OTHER_CHROME)));
    t.diagnostic(`chrome under the pills: ${JSON.stringify(onChrome)}`);
    assert.deepEqual(onChrome, [], "the pills overlap another piece of chrome");
    assert.deepEqual(JSON.parse(await page.eval(UNPRESSABLE)), [], "a pill is too small or covered");

    // (3) Each pill opens the sheet ON ITS LIST: the sheet rises and that tab is the selected one,
    // with its rows inside the sheet's own box.
    for (const list of ["candidate", "confirmed"]) {
      await page.click(`document.querySelector('.map-inv .map-pill[data-list="${list}"]')`);
      await page.waitFor(`the card to open on the ${list} list`,
        `document.querySelector('.sheet')?.hidden === false &&
         document.querySelector('.side-inv .tab[data-tab="${list}"]')?.getAttribute('aria-selected') === 'true'`,
        { timeoutMs: 10000 });
      await settled(page, ".sheet", `the sheet's rise for ${list}`);
      const inside = JSON.parse(await page.eval(`JSON.stringify((() => {
        const s = document.querySelector('.sheet').getBoundingClientRect();
        const inv = document.querySelector('.side-inv').getBoundingClientRect();
        return { inSheet: !!document.querySelector('.sheet .side-inv'), h: Math.round(inv.height),
                 top: Math.round(inv.top), sheetTop: Math.round(s.top), sheetBottom: Math.round(s.bottom) };
      })())`));
      t.diagnostic(`${list} list in the sheet: ${JSON.stringify(inside)}`);
      assert.equal(inside.inSheet, true, "the lists are not in the sheet the pill opened");
      assert.ok(inside.h > 40, `the ${list} list is not on screen (${inside.h} px tall)`);
      assert.ok(inside.top >= inside.sheetTop && inside.top < inside.sheetBottom,
        `the ${list} list is not inside the sheet's box (${JSON.stringify(inside)})`);
      await shot(`2-${list}`);
    }

    // (4) The pills are still where they were, and still clear of everything, with the sheet open.
    assert.deepEqual(JSON.parse(await page.eval(intersects(".map-inv", `${OTHER_CHROME}, .sf-hud-label.time`))), [],
      "with the sheet open the pills overlap chrome or the time ruler");

    assert.deepEqual(page.requests.filter((r) => CONTROL.test(r.url)).map((r) => r.url), [],
      "pressing a pill reached a device route");
    assert.deepEqual(page.exceptions, [], "uncaught exception");
    t.diagnostic(`pills read: ${JSON.stringify(await page.$text(".map-inv"))}`);
  });
}
