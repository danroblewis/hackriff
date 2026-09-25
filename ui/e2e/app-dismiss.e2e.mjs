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
  { name: "the layers menu", open: ".map-layers-btn", close: ".map-layers-close", box: ".map-layers",
    isOpen: "!document.querySelector('.map-layers').hidden" },
];

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
  }
  await page.frames(3);

  // (1) Each overlay alone: open from its small control, visible close, map back after.
  for (const o of OVERLAYS) {
    assert.equal(await page.eval(o.isOpen), false, `${o.name} is open before its control was pressed`);
    await page.click(`document.querySelector(${JSON.stringify(o.open)})`);
    await page.waitFor(`${o.name} to open`, o.isOpen, { timeoutMs: 5000 });
    await page.frames(2);
    const box = await page.$rect(o.box);
    const canvas = await page.$rect(".sf-canvas");
    // The canvas rows the open overlay covers.
    const y0 = Math.max(box.y, canvas.y) + 1, y1 = Math.min(box.y + box.h, canvas.y + canvas.h) - 1;
    assert.ok(y1 > y0, `${o.name} covers no canvas rows (${JSON.stringify({ box, canvas })})`);
    const during = await page.unoccludedColumns(".sf-canvas", { y0, y1 });
    assert.equal(await page.eval(pressable(o.close)), true, `${o.name}'s close (×) is not visible and pressable`);
    await page.click(`document.querySelector(${JSON.stringify(o.close)})`);
    await page.waitFor(`${o.name} to close`, `!(${o.isOpen})`, { timeoutMs: 5000 });
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
    await page.click(`document.querySelector(${JSON.stringify(o.close)})`);
    await page.waitFor(`${o.name} to close again`, `!(${o.isOpen})`, { timeoutMs: 5000 });
    await page.frames(2);
    const again = await page.unoccludedColumns(".sf-canvas", { y0, y1 });
    assert.equal(again.occluded, after.occluded, `${o.name}: a second close left ${again.occluded - after.occluded} more columns covered`);
  }
  assert.deepEqual(OVERLAYS.map(() => false), await Promise.all(OVERLAYS.map((o) => page.eval(o.isOpen))));

  // (2) Escape closes the topmost only — the one opened last — one per press.
  for (const o of OVERLAYS) {
    await page.click(`document.querySelector(${JSON.stringify(o.open)})`);
    await page.waitFor(`${o.name} to open`, o.isOpen, { timeoutMs: 5000 });
  }
  for (let i = OVERLAYS.length - 1; i >= 0; i--) {
    await page.key("Escape");
    await page.waitFor(`Escape to close ${OVERLAYS[i].name}`, `!(${OVERLAYS[i].isOpen})`, { timeoutMs: 5000 });
    for (let j = 0; j < i; j++) {
      assert.equal(await page.eval(OVERLAYS[j].isOpen), true, `Escape closed ${OVERLAYS[j].name} as well as the topmost`);
    }
  }

  assert.deepEqual(page.requests.filter((r) => CONTROL.test(r.url)).map((r) => r.url), [],
    "opening or closing an overlay reached a device route");
  assert.deepEqual(page.exceptions, [], "uncaught exception");
});
