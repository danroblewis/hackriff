// T-803 (MAP-03): the bottom sheet, in the real app. The unit tier (`ui/test/app-sheet.test.ts`)
// proves the snap model and the spy-client rule over a fake DOM; this proves the three things only a
// browser can say: a real pointer drag on the grab handle snaps it, the canvas BESIDE an open sheet
// is still the canvas (hit test) and still pans (non-modal), and nothing the sheet or that pan does
// reaches a device route. It also checks the per-viewer snap state survives a reload.
import test from "node:test";
import assert from "node:assert/strict";
import { Browser } from "./harness.mjs";

const ORIGIN = process.env.HK_E2E_ORIGIN, TOKEN = process.env.HK_E2E_TOKEN;
const CONTROL = /\/api\/control\/(center|rate|window|gains|bias_tee|baseband_filter)/;

test("the sheet drags between peek, half and full, and the canvas beside it stays live", async (t) => {
  const browser = await Browser.open();
  t.after(() => browser.close());
  const page = await browser.page();
  assert.equal(await page.goto(`${ORIGIN}/#token=${TOKEN}`), "load");
  await page.waitFor("the app's surface to draw and the sheet to mount",
    `!!document.querySelector('.sf-canvas') && document.querySelector('.sf-canvas').width > 200 &&
     document.querySelector('.sheet')?.dataset.snap === 'peek'`, { timeoutMs: 60000 });

  const vh = await page.eval("window.innerHeight");
  const snap = () => page.eval("document.querySelector('.sheet').dataset.snap");
  const height = () => page.eval("document.querySelector('.sheet').getBoundingClientRect().height");
  const waitSnap = (s) => page.waitFor(`the sheet to settle at ${s}`,
    `document.querySelector('.sheet').dataset.snap === ${JSON.stringify(s)} &&
     Math.abs(document.querySelector('.sheet').getBoundingClientRect().height - parseFloat(document.querySelector('.sheet').style.height)) < 1`);

  // (1) Peek is a title strip that states the (empty) selection.
  assert.ok((await height()) < 80, `peek is a strip, got ${await height()} px`);
  assert.match(await page.$text(".sheet-title"), /nothing yet/);

  // (2) A real drag on the grab handle, released near the half-height mark, snaps to half.
  const grab = await page.$rect(".sheet-grab");
  const g = { x: grab.x + grab.w / 2, y: grab.y + grab.h / 2 };
  await page.drag(g, { x: g.x, y: g.y - vh * 0.4 }, 10);
  await waitSnap("half");
  const half = await height();
  assert.ok(half > vh * 0.3 && half < vh * 0.6, `half is ~45 vh, got ${half} of ${vh}`);

  // (3) Non-modal: beside the open sheet, the browser's own hit test lands on the surface, and a
  // drag there pans the view — with the sheet still open.
  const sheet = await page.$rect(".sheet");
  const canvas = await page.$rect(".sf-canvas");
  const at = { x: Math.max(canvas.x + 20, sheet.x - 120), y: canvas.y + canvas.h * 0.4 };
  assert.ok(at.x < sheet.x, "there is canvas beside the sheet at this width");
  assert.equal(await page.eval(`!!document.elementFromPoint(${at.x}, ${at.y})?.closest('.surface')`), true,
    "a point beside the sheet is the surface, not an overlay");
  // Waited on the surface's own drawn state (its per-pane readout names the pane once addressed),
  // never on a clock: under load addressing takes tens of seconds, and a pan before it is not a pan.
  await page.waitFor("the surface to address its pane",
    `/pane/.test(document.querySelector('.sf-chrome')?.textContent ?? "")`, { timeoutMs: 90000 });
  const before = (await page.$text(".sf-chrome")) ?? "";
  await page.drag(at, { x: at.x - 150, y: at.y });
  await page.waitFor("the canvas to pan with the sheet open",
    `(document.querySelector('.sf-chrome')?.textContent ?? "") !== ${JSON.stringify(before)}`, { timeoutMs: 15000 });
  assert.equal(await snap(), "half", "panning the canvas leaves the sheet where it was");

  // (4) Clicking the handle cycles to full, which still leaves the top chrome clear.
  await page.click("document.querySelector('.sheet-grab')");
  await waitSnap("full");
  const full = await page.$rect(".sheet");
  const bar = await page.$rect(".app > .bar");
  assert.ok(full.y >= bar.y + bar.h, `full stops below the top bar (${full.y} vs ${bar.y + bar.h})`);

  // (5) Per-viewer state: a reload comes back at full.
  // (A reload, not a goto: the app strips `#token=` from the address bar, so navigating back to the
  // same URL is a fragment change that never reloads. The token survives in this tab's storage.)
  await page.eval("window.__beforeReload = true");
  await page.eval("setTimeout(() => location.reload(), 0), true");
  await page.waitFor("the page to reload and the sheet to remount",
    "!window.__beforeReload && !!document.querySelector('.sheet')?.dataset.snap", { timeoutMs: 60000 });
  assert.equal(await snap(), "full", "the viewer's snap state was not remembered");

  assert.deepEqual(page.requests.filter((r) => CONTROL.test(r.url)).map((r) => r.url), [],
    "a sheet gesture or a pan reached a device route");
  assert.deepEqual(page.exceptions, [], "uncaught exception");
});
