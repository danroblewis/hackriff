// T-900 (user P1, docs/23 §10.6 rule 1): overlays are CLOSED, not faded. In the real app, at three
// widths, every band-2/3 overlay over the map — the bottom sheet, the left column, the layers menu —
// opens from its small control, has a visible close (×) that is what a click at its centre lands on,
// and closing it gives the map back: the canvas columns it covered are clear again (the
// `unoccludedColumns` hit test, same rows before, open and after). Escape then closes the TOPMOST
// open overlay only, one per press. Nothing here reaches a device route.
//
// (The Research slide-in is not built yet — T-821; it registers on the same stack, `chrome/dismiss.ts`.)
import test from "node:test";
import assert from "node:assert/strict";
import { Browser } from "./harness.mjs";
import * as chrome from "./app-chrome.mjs";

const ORIGIN = process.env.HK_E2E_ORIGIN, TOKEN = process.env.HK_E2E_TOKEN;
const CONTROL = /\/api\/control\/(center|rate|window|gains|bias_tee|baseband_filter)/;

// Is a click at `sel`'s centre the element itself?
const pressable = (sel) => `(() => { const e = document.querySelector(${JSON.stringify(sel)}); if (!e) return false;
  const r = e.getBoundingClientRect(); if (!(r.width > 0 && r.height > 0)) return false;
  const top = document.elementFromPoint(r.x + r.width / 2, r.y + r.height / 2); return !!top && (top === e || e.contains(top)); })()`;

const OVERLAYS = [
  { name: "the bottom sheet", open: ".sheet-head", close: ".sheet-close", box: ".sheet",
    isOpen: "document.querySelector('.sheet').dataset.snap !== 'peek'" },
  { name: "the left column", open: ".side-chip", close: ".side-close", box: "#view-explore > .side",
    isOpen: "document.querySelector('#view-explore > .side').classList.contains('is-open')" },
  { name: "the layers menu", open: ".map-layers-btn", close: ".map-layers .map-layers-close", box: ".map-layers",
    isOpen: "!document.querySelector('.map-layers').hidden" },
  // The viewport menu and the layers menu replace each other (one menu at a time), so it is checked
  // alone, not in the Escape-order sequence below.
  { name: "the viewport menu", open: ".map-pane-btn", close: ".map-pane-close", box: ".map-pane-menu",
    isOpen: "!document.querySelector('.map-pane-menu').hidden", alone: true },
];
const STACKED = OVERLAYS.filter((o) => !o.alone);
// T-918: an overlay that animates between its states (the sheet's height transition, sheet.css
// .28 s) is measured once it has ARRIVED, never at a frame of the way there — a box read two frames
// into its opening, or its closing, is most of the way back where it started.
// T-958: through the shared `arrived` predicate, which adds the second half of "has it arrived" —
// the rendered height against the one the product set. `getAnimations()` alone is empty in the
// window between the click's style mutation and the style recalc that creates the transition, i.e.
// exactly when this poll first runs.
const settled = (page, o, what) => chrome.settled(page, o.box, `${o.name}'s ${what}`);

for (const width of [1440, 1000, 420]) test(`at ${width} px every overlay closes back to the map, and Escape closes the topmost`, async (t) => {
  const browser = await Browser.open();
  t.after(() => browser.close());
  const page = await browser.page(undefined, { width, height: 860 });
  assert.equal(await page.goto(`${ORIGIN}/#token=${TOKEN}`), "load");
  await page.waitFor("the surface, its floating controls, the sheet and the side chip",
    `!!document.querySelector('.sf-canvas') && document.querySelector('.sf-canvas').width > 200 &&
     !!document.querySelector('.map-ctl .map-layers-btn') && !!document.querySelector('.sheet-close') &&
     !!document.querySelector('.side-chip')`, { timeoutMs: 60000 });
  // Every overlay starts closed (a stored snap state from nothing: fresh profile).
  if (await page.eval(OVERLAYS[0].isOpen)) {
    await page.click("document.querySelector('.sheet-close')");
    await page.waitFor("the sheet at peek", `!(${OVERLAYS[0].isOpen})`, { timeoutMs: 5000 });
    // T-958: and arrived, before the loop below measures it shut and presses its head open again.
    await settled(page, OVERLAYS[0], "reset");
  }
  await page.frames(3);
  // T-918 (docs/23 §10.1): the canvas is 100vw x 100vh and no chrome subtracts from it — so every
  // overlay below is OVER map pixels, and closing it has map pixels to give back. A canvas framed
  // short of the sheet (T-801 round 3's padding plus the rows under the stage) is the defect.
  const bleed = await page.$rect(".sf-canvas");
  assert.deepEqual([bleed.x, bleed.y, bleed.w, bleed.h].map(Math.round), [0, 0, width, 860],
    `the canvas is not full-bleed at ${width} px: ${JSON.stringify(bleed)}`);

  // T-995: the minimap is retired (user, 2026-09-25) — no map viewport row, at any width.
  assert.equal(await page.$count('.hk-surface-viewport[data-viewport="minimap"]'), 0,
    `a minimap viewport is still drawn at ${width} px`);
  // T-933, carried to the panes: what used to be the minimap's lift is now the panes' bottom edge,
  // and it must clear the sheet's peek strip (never hidden — T-803) at every width. The sheet
  // starts at peek here (the reset above), so this is the every-width, no-interaction case;
  // `insetBottom` is `surface.ts`'s own lift, read off the canvas the way the surface itself
  // states it (`canvas.dataset.insetBottom`), never a second guess at it.
  const insets = await page.canvasInsets();
  const paneBottom = bleed.y + bleed.h - insets.bottom;
  const peek = await page.$rect(".sheet");
  t.diagnostic(`at ${width} px pane bottom y ${Math.round(paneBottom)}, sheet peek y ${Math.round(peek.y)}-${Math.round(peek.y + peek.h)}`);
  assert.ok(paneBottom <= peek.y + 0.5,
    `the panes' bottom edge (y ${Math.round(paneBottom)}) runs under the sheet's peek strip (y ${Math.round(peek.y)}-${Math.round(peek.y + peek.h)}) at ${width} px`);

  // (1) Each overlay alone: open from its small control, visible close, map back after.
  for (const o of OVERLAYS) {
    assert.equal(await page.eval(o.isOpen), false, `${o.name} is open before its control was pressed`);
    // Where the overlay sits CLOSED (the sheet's peek strip; nothing for a hidden menu): closing
    // cannot give those rows back, so they are not rows this check may measure (T-918).
    const shut = await page.$rect(o.box);
    await page.click(`document.querySelector(${JSON.stringify(o.open)})`);
    await page.waitFor(`${o.name} to open`, o.isOpen, { timeoutMs: 5000 });
    await settled(page, o, "opening");
    await page.frames(2);
    const box = await page.$rect(o.box);
    const canvas = await page.$rect(".sf-canvas");
    // The canvas rows the open overlay covers — the tallest band of them that no OTHER fixed chrome
    // crosses (Go-to, the range readout, the zoom stack sit in some of those rows whatever is open,
    // and would otherwise count as covered both before and after).
    const { y0, y1 } = await page.eval(`(() => {
      const me = document.querySelector(${JSON.stringify(o.box)});
      const lo = Math.max(${box.y}, ${canvas.y}) + 1, hi = Math.min(${box.y + box.h}, ${canvas.y + canvas.h}) - 1;
      const cuts = [...document.querySelectorAll('[data-band="chrome"] > *')]
        .filter((c) => !c.contains(me) && !me.contains(c))
        .map((c) => c.getBoundingClientRect())
        .filter((r) => r.width > 0 && r.height > 0 && r.right > ${box.x} && r.left < ${box.x + box.w})
        .map((r) => [r.top - 1, r.bottom + 1])
        .concat(${shut && shut.w > 0 && shut.h > 0 ? `[[${shut.y - 1}, ${shut.y + shut.h + 1}]]` : "[]"})
        .sort((a, b) => a[0] - b[0]);
      let best = { y0: lo, y1: lo }, at = lo;
      for (const [t, b] of [...cuts, [hi, hi]]) {
        const top = Math.min(t, hi);
        if (top - at > best.y1 - best.y0) best = { y0: at, y1: top };
        at = Math.max(at, b);
      }
      return best;
    })()`);
    assert.ok(y1 > y0, `${o.name} covers no canvas rows (${JSON.stringify({ box, canvas })})`);
    const during = await page.unoccludedColumns(".sf-canvas", { y0, y1 });
    assert.equal(await page.eval(pressable(o.close)), true, `${o.name}'s close (×) is not visible and pressable`);
    await page.click(`document.querySelector(${JSON.stringify(o.close)})`);
    await page.waitFor(`${o.name} to close`, `!(${o.isOpen})`, { timeoutMs: 5000 });
    await settled(page, o, "closing");
    await page.frames(2);
    const after = await page.unoccludedColumns(".sf-canvas", { y0, y1 });
    t.diagnostic(`at ${width} px ${o.name}: box ${JSON.stringify(box)}; occluded columns open ${during.occluded} → closed ${after.occluded}`);
    assert.ok(after.occluded < during.occluded, `closing ${o.name} gave no columns back to the map`);
    // Nothing of the closed overlay is left over the map: a point inside where it was is not it.
    const cx = box.x + box.w / 2, cy = (y0 + y1) / 2;
    assert.equal(await page.eval(`(() => { const e = document.elementFromPoint(${cx}, ${cy});
      return !!e?.closest(${JSON.stringify(o.box)}) && !e.closest('.side-chip'); })()`), false, `${o.name} still covers the map after its close`);
    // ...and closed is exactly the state before it opened: re-open, close again, same columns clear.
    await page.click(`document.querySelector(${JSON.stringify(o.open)})`);
    await page.waitFor(`${o.name} to re-open`, o.isOpen, { timeoutMs: 5000 });
    await settled(page, o, "re-opening");
    await page.click(`document.querySelector(${JSON.stringify(o.close)})`);
    await page.waitFor(`${o.name} to close again`, `!(${o.isOpen})`, { timeoutMs: 5000 });
    await settled(page, o, "closing again");
    await page.frames(2);
    const again = await page.unoccludedColumns(".sf-canvas", { y0, y1 });
    assert.equal(again.occluded, after.occluded, `${o.name}: a second close left ${again.occluded - after.occluded} more columns covered`);
  }
  assert.deepEqual(OVERLAYS.map(() => false), await Promise.all(OVERLAYS.map((o) => page.eval(o.isOpen))));

  // (2) Escape closes the topmost only — the one opened last — one per press.
  for (const o of STACKED) {
    await page.click(`document.querySelector(${JSON.stringify(o.open)})`);
    await page.waitFor(`${o.name} to open`, o.isOpen, { timeoutMs: 5000 });
  }
  for (let i = STACKED.length - 1; i >= 0; i--) {
    await page.key("Escape");
    await page.waitFor(`Escape to close ${STACKED[i].name}`, `!(${STACKED[i].isOpen})`, { timeoutMs: 5000 });
    for (let j = 0; j < i; j++) {
      assert.equal(await page.eval(STACKED[j].isOpen), true, `Escape closed ${STACKED[j].name} as well as the topmost`);
    }
  }

  assert.deepEqual(page.requests.filter((r) => CONTROL.test(r.url)).map((r) => r.url), [],
    "opening or closing an overlay reached a device route");
  assert.deepEqual(page.exceptions, [], "uncaught exception");
});
