// T-804 (MAP-04): the detail sheet, in the real app over the replayed FM fixture. The unit tier
// (`ui/test/app-detail.test.ts`) proves what each line says for a served row; this proves the
// journey: a signal blind detection found is selected, the T-803 sheet rises from peek to half on
// its own, and it shows that signal — its frequency (the same one its list row shows), liveness,
// the Measured block with the time it was measured over, the ranked explanations framed as
// suggestions, and the action row — while selecting reaches no device route.
import test from "node:test";
import assert from "node:assert/strict";
import { Browser } from "./harness.mjs";

const ORIGIN = process.env.HK_E2E_ORIGIN, TOKEN = process.env.HK_E2E_TOKEN;
const CONTROL = /\/api\/control\/(center|rate|window|gains|bias_tee|baseband_filter)/;

// A detected row in either inventory tab. Confirmed is the default tab and may still be empty while
// the fixture's station is a candidate, so each look that finds none flips to the other tab (a
// store-only toggle, like any tab click).
const ROW_PRESENT = `(() => {
  const r = document.querySelector('.side-inv .row[data-id]');
  if (!r) {
    const tabs = [...document.querySelectorAll('.side-inv .tab')];
    const cur = tabs.findIndex((t) => t.getAttribute('aria-selected') === 'true');
    tabs[(cur + 1) % tabs.length]?.click();
  }
  return !!r;
})()`;

test("selecting a detected signal opens its detail sheet over the still-live canvas", async (t) => {
  const browser = await Browser.open();
  t.after(() => browser.close());
  const page = await browser.page();
  assert.equal(await page.goto(`${ORIGIN}/#token=${TOKEN}`), "load");
  await page.waitFor("the sheet to mount collapsed",
    "document.querySelector('.sheet')?.dataset.snap === 'peek'", { timeoutMs: 60000 });
  // T-895: the lists are a chip by default; a real click on a row needs them open.
  await page.click("document.querySelector('.side-chip')");
  await page.waitFor("blind detection to list a signal", ROW_PRESENT, { timeoutMs: 120000, everyMs: 1000 });

  const rowF = (await page.$text(".side-inv .row[data-id] .f")) ?? "";
  const mhz = rowF.match(/[\d.]+/)?.[0];
  assert.ok(mhz, `the row states a frequency (${rowF})`);
  await page.click("document.querySelector('.side-inv .row[data-id]')");

  await page.waitFor("the sheet to rise to half with the signal's detail",
    `document.querySelector('.sheet')?.dataset.snap === 'half' && !!document.querySelector('.focus .detail .bigf')`,
    { timeoutMs: 15000 });
  assert.match((await page.$text(".focus .detail .bigf")) ?? "", new RegExp(`^${mhz.replace(".", "\\.")}`),
    "the sheet's big frequency is the selected row's");
  assert.match((await page.$text(".sheet-title")) ?? "", /^Selected signal · [\d.]+ MHz$/, "the peek strip names it");
  assert.match((await page.$text(".focus .detail .liveness")) ?? "", /^(On air|Ended|Not on air|Liveness not reported)/);
  assert.match((await page.$text(".focus .detail .cols")) ?? "", /Measured[\s\S]*Centre[\s\S]*Bandwidth/);
  assert.match((await page.$text(".focus .detail .at")) ?? "", /^(measured at \d\d:\d\d:\d\d UTC over |No level measured yet)/,
    "a level is shown with the time it was measured over, or said to be absent");
  assert.match((await page.$text(".focus .detail .cols")) ?? "", /ranked suggestions, never truth/);
  const labels = JSON.parse(await page.eval(
    "JSON.stringify([...document.querySelectorAll('.focus .detail .actions button')].map((b) => b.textContent))"));
  for (const want of ["Listen", "Record clip", "Stream out", "Analyze"]) assert.ok(labels.includes(want), `${want} in ${labels}`);
  assert.ok(labels.some((l) => /Decode|Open in Decode/.test(l)), `a Decode action in ${labels}`);
  assert.ok(labels.some((l) => /^Delete/.test(l)), `a Delete action in ${labels}`);

  // docs/23 §10.6 P4: the device actions are a compact cluster of small buttons (>= 24 px hit
  // targets, each far smaller than the sheet), not a large surface.
  const sizes = JSON.parse(await page.eval(`JSON.stringify((() => {
    const s = document.querySelector('.sheet').getBoundingClientRect();
    const bar = document.querySelector('.focus .detail .actions[role=toolbar]');
    const b = [...bar.querySelectorAll('button')].map((x) => x.getBoundingClientRect());
    return { sheetW: s.width, minH: Math.min(...b.map((r) => r.height)), minW: Math.min(...b.map((r) => r.width)),
      maxH: Math.max(...b.map((r) => r.height)), maxW: Math.max(...b.map((r) => r.width)) };
  })())`));
  assert.ok(sizes.minH >= 24 && sizes.minW >= 24, `>= 24 px hit targets: ${JSON.stringify(sizes)}`);
  assert.ok(sizes.maxH <= 32 && sizes.maxW < sizes.sheetW / 3, `each action is a small button: ${JSON.stringify(sizes)}`);

  // Non-modal: the canvas beside the sheet is still the surface.
  const sheet = await page.$rect(".sheet");
  const canvas = await page.$rect(".sf-canvas");
  const at = { x: Math.max(canvas.x + 20, sheet.x - 120), y: canvas.y + canvas.h * 0.4 };
  assert.equal(await page.eval(`!!document.elementFromPoint(${at.x}, ${at.y})?.closest('.surface')`), true,
    "a point beside the open detail sheet is the surface");

  // docs/23 §10.6 P1: a visible dismiss returns the sheet's pixels to the map.
  await page.click("document.querySelector('.sheet .sheet-close')");
  await page.waitFor("the dismiss to collapse the sheet", "document.querySelector('.sheet')?.dataset.snap === 'peek'", { timeoutMs: 5000 });
  await page.waitFor("the sheet to shrink to its peek strip",
    "document.querySelector('.sheet').getBoundingClientRect().height <= 60 && document.querySelector('.sheet .sheet-close').hidden", { timeoutMs: 5000 });

  assert.deepEqual(page.requests.filter((r) => CONTROL.test(r.url)).map((r) => r.url), [],
    "selecting a signal or opening its sheet reached a device route");
  assert.deepEqual(page.exceptions, [], "uncaught exception");
});
