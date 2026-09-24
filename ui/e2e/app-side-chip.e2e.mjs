// T-895 (user P1, docs/23 §10.6 rule 1): the left inventory column, in the real app. The unit tier
// (`ui/test/app-side-chip.test.ts`) proves the counts, the default and the spy-client rule over a
// fake DOM; this proves what only a browser's own hit test can: at every width the Explore map opens
// with the lists as a chip no taller than 56 px, that chip is pressable, and neither the chip nor the
// OPEN overlay covers the surface's toolbar (T-528), Go-to, the zoom stack, the FAB (T-802) or the
// sheet's handle (T-803); the close (×) then gives every pixel back to the map. Nothing here reaches
// a device route.
import test from "node:test";
import assert from "node:assert/strict";
import { Browser } from "./harness.mjs";

const ORIGIN = process.env.HK_E2E_ORIGIN, TOKEN = process.env.HK_E2E_TOKEN;
const CONTROL = /\/api\/control\/(center|rate|window|gains|bias_tee|baseband_filter)/;

// T-528's hit test, over every control this ticket must never cover: each is what a click at its
// own centre lands on.
const GUARDED = ".sf-actions button, .map-goto input, .map-zoom-in, .map-zoom-out, .map-fab, .sheet-grab";
const hitTest = (sel) => `JSON.stringify([...document.querySelectorAll(${JSON.stringify(sel)})].map((el) => {
  const r = el.getBoundingClientRect();
  if (r.width === 0 || r.height === 0) return { sel: el.className, ok: false, covered: 'not drawn' };
  const top = document.elementFromPoint(r.x + r.width / 2, r.y + r.height / 2);
  return { sel: el.className || el.tagName, label: (el.textContent ?? '').trim(), bySide: !!top?.closest('.side'),
           covered: top ? (top.className?.baseVal ?? top.className ?? top.tagName) : 'nothing',
           ok: !!top && (top === el || el.contains(top)) };
}).filter((b) => !b.ok))`;
// ...and the side's own box is disjoint from theirs (the cluster paints above it, so a hit test from
// the control's side alone passes while the control hides the lists).
const OVERLAPS_SIDE = `JSON.stringify((() => {
  const s = document.querySelector('#view-explore > .side').getBoundingClientRect();
  return [...document.querySelectorAll('.map-goto, .map-zoom, .map-fab, .sheet-grab, .sf-actions button')]
    .map((el) => ({ cls: el.className || el.tagName, r: el.getBoundingClientRect() }))
    .filter(({ r }) => r.width > 0 && r.left < s.right && r.right > s.left && r.top < s.bottom && r.bottom > s.top)
    .map(({ cls }) => cls);
})())`;

for (const width of [1440, 1000, 920, 420]) test(`at ${width} px the lists are a chip, open over the map, and close back to it`, async (t) => {
  const browser = await Browser.open();
  t.after(() => browser.close());
  const page = await browser.page(undefined, { width, height: 860 });
  assert.equal(await page.goto(`${ORIGIN}/#token=${TOKEN}`), "load");
  await page.waitFor("the surface, its floating controls, the sheet and the side chip",
    `!!document.querySelector('.sf-actions button') && !!document.querySelector('.map-ctl .map-fab') &&
     document.querySelector('.sheet')?.dataset.snap === 'peek' && !!document.querySelector('.side-chip')`,
    { timeoutMs: 60000 });
  await page.frames(3);

  // (1) Collapsed is the default: the side is exactly its chip, ≤ 56 px tall, and the lists take no pixels.
  const collapsed = JSON.parse(await page.eval(`JSON.stringify((() => {
    const side = document.querySelector('#view-explore > .side');
    const chip = side.querySelector('.side-chip').getBoundingClientRect();
    const box = side.getBoundingClientRect();
    const inv = side.querySelector('.side-inv').getBoundingClientRect();
    return { collapsed: side.classList.contains('is-collapsed'), chipH: chip.height, chipW: chip.width,
             sideH: box.height, sideW: box.width, invArea: inv.width * inv.height,
             expanded: side.querySelector('.side-chip').getAttribute('aria-expanded'),
             counts: side.querySelector('.side-chip').getAttribute('aria-label') };
  })())`));
  t.diagnostic(`at ${width} px collapsed: ${JSON.stringify(collapsed)}`);
  assert.equal(collapsed.collapsed, true, "the lists are not collapsed by default");
  assert.ok(collapsed.chipH > 0 && collapsed.chipH <= 56, `the chip is ${collapsed.chipH} px tall (≤ 56)`);
  assert.ok(collapsed.sideH <= 56, `the collapsed side is ${collapsed.sideH} px tall — more than its chip`);
  assert.equal(collapsed.invArea, 0, "the collapsed lists still take pixels");
  assert.equal(collapsed.expanded, "false");
  assert.match(collapsed.counts, /\d+ candidates?, \d+ confirmed/, "the chip states the lists' counts");
  // Only what the SIDE covers is this ticket's: below ~1000 px some toolbar buttons are already
  // pushed off-screen or under the wrapped top bar (app-sheet.e2e filters to `bySheet` for the same
  // reason), which the diagnostic records.
  const shut = JSON.parse(await page.eval(hitTest(GUARDED)));
  t.diagnostic(`at ${width} px collapsed, unpressable guarded controls: ${JSON.stringify(shut)}`);
  assert.deepEqual(shut.filter((b) => b.bySide), [], "the chip covers a guarded control");
  assert.deepEqual(JSON.parse(await page.eval(hitTest(".side-chip"))), [], "the chip itself is not pressable");
  assert.deepEqual(JSON.parse(await page.eval(OVERLAPS_SIDE)), [], "the chip overlaps a guarded control");

  // (2) Pressing the chip opens the lists over the map, with a visible close.
  await page.click("document.querySelector('.side-chip')");
  await page.waitFor("the lists to open", `document.querySelector('#view-explore > .side').classList.contains('is-open') &&
    document.querySelector('.side-inv').getBoundingClientRect().height > 40`, { timeoutMs: 5000 });
  await page.frames(3);
  assert.deepEqual(JSON.parse(await page.eval(hitTest(".side-close, .side-inv .tab"))), [], "the close or a list tab is not pressable");
  const openBad = JSON.parse(await page.eval(hitTest(GUARDED)));
  t.diagnostic(`at ${width} px open: side ${JSON.stringify(await page.$rect("#view-explore > .side"))}, uncovered failures ${JSON.stringify(openBad)}`);
  assert.deepEqual(openBad.filter((b) => b.bySide), [], "the open lists cover a guarded control");
  assert.deepEqual(openBad.filter((b) => !shut.some((c) => c.sel === b.sel && c.label === b.label)), [],
    "opening the lists made a guarded control unpressable that was pressable before");
  assert.deepEqual(JSON.parse(await page.eval(OVERLAPS_SIDE)), [], "the open lists overlap a guarded control");

  // (3) The close puts every pixel back to the map: the lists take none, and a point where they
  // were is the surface again.
  const was = await page.$rect(".side-inv");
  await page.click("document.querySelector('.side-close')");
  await page.waitFor("the lists to collapse", `document.querySelector('#view-explore > .side').classList.contains('is-collapsed')`, { timeoutMs: 5000 });
  const at = { x: was.x + was.w / 2, y: was.y + was.h / 2 };
  assert.equal(await page.eval(`(() => { const e = document.elementFromPoint(${at.x}, ${at.y}); return !!e?.closest('.surface'); })()`), true,
    "where the open lists were is not the map again");
  assert.equal(await page.eval("document.querySelector('.side-inv').getBoundingClientRect().height"), 0);

  assert.deepEqual(page.requests.filter((r) => CONTROL.test(r.url)).map((r) => r.url), [],
    "opening or closing the lists reached a device route");
  assert.deepEqual(page.exceptions, [], "uncaught exception");
});
