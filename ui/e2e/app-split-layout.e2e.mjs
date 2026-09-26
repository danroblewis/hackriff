// T-1005 (MMAP split view): the split layout's own controls — in the real app, over the product's
// own server, at a desktop width and a phone width.
//
// Before this ticket the app could only split side by side, the divider could not be moved
// (`PaneModel.setSplitFraction` had no caller), a pane could be closed only through the viewport
// menu and only if it was the active one, and a close silently made pane 1 active whatever the user
// was looking at. Here:
//   1. the divider is DRAGGED: its fraction follows the pointer, and at every step of the drag the
//      two panes and the divider are one consistent layout from one frame (pane 1's right edge, the
//      divider and pane 2's left edge coincide) — both panes re-lay-out together, never one a
//      frame behind the other;
//   2. rows ⇄ columns: the viewport menu flips the split, and Split ⇕ stacks a new one;
//   3. every pane has its own ×, and closing each works; after a close the active pane is the one
//      under the pointer, not "the first";
// and none of it reaches a device route (docs/23 §10.4): a layout is view state.
import test from "node:test";
import assert from "node:assert/strict";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { Browser } from "./harness.mjs";
import { paneAct, unclickable } from "./app-chrome.mjs";

const ORIGIN = process.env.HK_E2E_ORIGIN, TOKEN = process.env.HK_E2E_TOKEN;
const CONTROL = /\/api\/control\/(center|rate|window|gains|bias_tee|baseband_filter)/;
const ART = path.join(path.dirname(fileURLToPath(import.meta.url)), "artifacts");

/** The layout the last frame drew — pane boxes and dividers from ONE frame — in CSS px of the canvas. */
const LAYOUT = `(document.querySelector('.sf-stage').dataset.splitLayout || 'null')`;
const layout = async (page) => JSON.parse(await page.eval(LAYOUT));
/** Each pane row's follow state, keyed by pane id (`data-following`, written per frame). */
const FOLLOWS = `JSON.stringify(Object.fromEntries([...document.querySelectorAll('.hk-surface-viewport[data-viewport="pane"]')]
  .map((r) => [r.querySelector('.hk-surface-id').textContent, r.dataset.following === 'true'])))`;
const ACTIVE = "document.querySelector('.sf-active-pane').dataset.paneId";

/** Every pane's × and every divider handle must be pressable where it is drawn: not under the
 * floating chrome (the stacked split's bottom × landed under the zoom stack once). */
async function assertPressable(page, where) {
  await page.frames(2);
  const bad = JSON.parse(await page.eval(unclickable(".sf-pane-x, .sf-divider")));
  assert.deepEqual(bad, [], `${where}: a split control is covered or off screen`);
}

/** Two panes and one divider that abut: the invariant a drag must hold at every frame. */
function assertConsistent(l, dir, where) {
  assert.ok(l && l.panes.length === 2 && l.dividers.length === 1, `${where}: not two panes and one divider: ${JSON.stringify(l)}`);
  const [a, b] = [...l.panes].sort((x, y) => x.n - y.n);
  const d = l.dividers[0];
  assert.equal(d.dir, dir, `${where}: the divider is not a ${dir} divider`);
  if (dir === "columns") {
    const mid = d.left + d.width / 2;
    assert.ok(Math.abs(a.left + a.width - mid) <= 3 && Math.abs(b.left - mid) <= 3,
      `${where}: pane 1, the divider and pane 2 are not one layout: ${JSON.stringify({ a, b, d })}`);
    assert.ok(Math.abs(a.top - b.top) <= 1 && Math.abs(a.height - b.height) <= 1, `${where}: columns are not side by side`);
  } else {
    const mid = d.top + d.height / 2;
    assert.ok(Math.abs(a.top + a.height - mid) <= 3 && Math.abs(b.top - mid) <= 3,
      `${where}: pane 1 (top), the divider and pane 2 are not one layout: ${JSON.stringify({ a, b, d })}`);
    assert.ok(Math.abs(a.left - b.left) <= 1 && Math.abs(a.width - b.width) <= 1, `${where}: rows are not stacked`);
  }
  return { a, b, d };
}

for (const [width, height] of [[1280, 800], [400, 820]]) test(`at ${width} px: drag the divider, flip rows/columns, close each pane`, async (t) => {
  const browser = await Browser.open();
  t.after(() => browser.close());
  const page = await browser.page(undefined, { width, height });
  assert.equal(await page.goto(`${ORIGIN}/#token=${TOKEN}`), "load");
  await page.waitForSurfaceMounted({ timeoutMs: 60000 });
  await page.waitFor("the floating cluster to mount", "!!document.querySelector('.map-pane-btn') && !!document.querySelector('.sf-split')", { timeoutMs: 30000 });
  await page.waitFor("a pane row with its level stated",
    "!!document.querySelector('.hk-surface-viewport[data-viewport=\"pane\"] .hk-surface-level')?.textContent", { timeoutMs: 90000 });

  // One pane: no divider, no ×, no layout statement — nothing to resize or close.
  assert.equal(await layout(page), null, "a single pane states a split layout");
  assert.equal(await page.$count(".sf-divider, .sf-pane-x"), 0, "a single pane has split controls");

  // ---- (1) the divider drag ----
  await paneAct(page, "split");
  await page.waitFor("the divider and both ×", "document.querySelectorAll('.sf-divider').length === 1 && document.querySelectorAll('.sf-pane-x').length === 2", { timeoutMs: 10000 });
  const canvas = await page.$rect(".sf-canvas");
  const start = assertConsistent(await layout(page), "columns", "after the split");
  assert.ok(Math.abs(start.d.frac - 0.5) < 1e-9, `a split does not start at half: ${start.d.frac}`);
  const follows0 = JSON.parse(await page.eval(FOLLOWS));
  await assertPressable(page, "two columns");
  await page.shot(path.join(ART, `app-split-layout-${width}-columns.png`));

  const h0 = await page.$rect(".sf-divider");
  const from = { x: h0.x + h0.w / 2, y: h0.y + h0.h * 0.5 };
  const toX = canvas.x + canvas.w * 0.3;
  const steps = 6;
  await page.mouse("mousePressed", from.x, from.y, { buttons: 1, clickCount: 1 });
  for (let i = 1; i <= steps; i++) {
    const x = from.x + ((toX - from.x) * i) / steps;
    await page.mouse("mouseMoved", x, from.y, { buttons: 1 });
    await page.frames(1);
    // Every intermediate frame is one layout: both panes moved with the divider, together.
    assertConsistent(await layout(page), "columns", `drag step ${i}`);
  }
  await page.mouse("mouseReleased", toX, from.y, { buttons: 0, clickCount: 1 });
  await page.frames(2);
  const dragged = assertConsistent(await layout(page), "columns", "after the drag");
  const wantFrac = (toX - canvas.x) / canvas.w;
  t.diagnostic(`drag: frac ${start.d.frac} -> ${dragged.d.frac} (pointer at ${wantFrac.toFixed(3)})`);
  assert.ok(Math.abs(dragged.d.frac - wantFrac) < 0.03, `the fraction did not follow the pointer: ${dragged.d.frac} vs ${wantFrac}`);
  assert.ok(dragged.a.width < start.a.width - canvas.w * 0.1, "pane 1 did not narrow");
  assert.ok(dragged.b.width > start.b.width + canvas.w * 0.1, "pane 2 did not widen with it");
  assert.equal(await page.eval("document.querySelector('.sf-divider').getAttribute('aria-valuenow')"), String(Math.round(dragged.d.frac * 100)));
  assert.deepEqual(JSON.parse(await page.eval(FOLLOWS)), follows0, "a divider drag changed a pane's follow state");
  await page.shot(path.join(ART, `app-split-layout-${width}-dragged.png`));

  // ---- (2) rows ⇄ columns ----
  await paneAct(page, "flip");
  await page.waitFor("the split to stack as rows", `JSON.parse(${LAYOUT})?.dividers?.[0]?.dir === 'rows'`, { timeoutMs: 10000 });
  await page.frames(2);
  const rows = assertConsistent(await layout(page), "rows", "after the flip");
  assert.ok(Math.abs(rows.d.frac - dragged.d.frac) < 1e-9, "the flip lost the dragged fraction");
  assert.ok(rows.a.top < rows.b.top, "pane 1 is not the top band");
  await page.shot(path.join(ART, `app-split-layout-${width}-rows.png`));
  // Drag the horizontal divider down to two thirds.
  const hr = await page.$rect(".sf-divider");
  const toY = canvas.y + canvas.h * 0.6;
  await page.drag({ x: hr.x + hr.w * 0.5, y: hr.y + hr.h / 2 }, { x: hr.x + hr.w * 0.5, y: toY }, 6);
  await page.frames(2);
  const rowsDragged = assertConsistent(await layout(page), "rows", "after the row drag");
  assert.ok(rowsDragged.a.height > rows.a.height + 10, `the top pane did not grow with a downward drag: ${rows.a.height} -> ${rowsDragged.a.height}`);
  await paneAct(page, "flip");
  await page.waitFor("the split back to columns", `JSON.parse(${LAYOUT})?.dividers?.[0]?.dir === 'columns'`, { timeoutMs: 10000 });

  // ---- (3) close each pane, and which pane is active after ----
  // Three panes: 1 | 2 | 3. Freeze pane 1 and make it active; then close pane 3 with its own ×.
  // The pointer is on pane 3's corner, which pane 2 grows into, so pane 2 becomes active — not
  // pane 1, the first one, which a close used to pick whatever the user was looking at.
  await page.key("2"); // pane 2 active: its split gives 1 | 2 | 3
  await paneAct(page, "split");
  await page.waitFor("three panes", "document.querySelectorAll('.sf-pane-x').length === 3", { timeoutMs: 10000 });
  await assertPressable(page, "three columns");
  await page.key("1");
  await page.key("l", { code: "KeyL", keyCode: 76 }); // freeze pane 1
  const ids = JSON.parse(await page.eval(`JSON.stringify([...document.querySelectorAll('.sf-pane-x')].sort((a, b) => a.dataset.pane - b.dataset.pane).map((b) => b.dataset.paneId))`));
  await page.waitFor("pane 1 frozen", `(${FOLLOWS.replace(/^JSON\.stringify/, "")})[${JSON.stringify(ids[0])}] === false`, { timeoutMs: 10000 });
  assert.equal(await page.eval(ACTIVE), ids[0]);
  await page.click(`document.querySelector('.sf-pane-x[data-pane-id=${JSON.stringify(ids[2])}]')`);
  await page.waitFor("pane 3 closed", "document.querySelectorAll('.sf-pane-x').length === 2", { timeoutMs: 10000 });
  assert.equal(await page.eval(ACTIVE), ids[1], "after closing pane 3 the pane under the pointer (pane 2) is not active");
  assert.equal(JSON.parse(await page.eval(FOLLOWS))[ids[0]], false, "closing pane 3 changed frozen pane 1");
  await page.frames(2);
  await page.shot(path.join(ART, `app-split-layout-${width}-closed3.png`));

  // Close pane 1 (the frozen one) with ITS ×: the live pane is all that is left, and it is active.
  await page.click(`document.querySelector('.sf-pane-x[data-pane-id=${JSON.stringify(ids[0])}]')`);
  await page.waitFor("one pane left", "document.querySelectorAll('.sf-pane-x').length === 0", { timeoutMs: 10000 });
  assert.equal(await layout(page), null, "a single pane still states a split layout");
  assert.equal(await page.$count(".sf-divider"), 0);
  const left = JSON.parse(await page.eval(FOLLOWS));
  assert.deepEqual(Object.keys(left), [ids[1]], `the survivor is not pane 2: ${JSON.stringify(left)}`);

  // Split ⇕ from one pane: stacked, and the last pane closes from its own × too.
  await paneAct(page, "split-rows");
  await page.waitFor("a stacked split", "document.querySelector('.sf-divider.rows') !== null", { timeoutMs: 10000 });
  await page.frames(2);
  assertConsistent(await layout(page), "rows", "after Split ⇕");
  await assertPressable(page, "two rows");
  await page.shot(path.join(ART, `app-split-layout-${width}-split-rows.png`));
  const rowIds = JSON.parse(await page.eval(`JSON.stringify([...document.querySelectorAll('.sf-pane-x')].sort((a, b) => a.dataset.pane - b.dataset.pane).map((b) => b.dataset.paneId))`));
  await page.click(`document.querySelector('.sf-pane-x[data-pane-id=${JSON.stringify(rowIds[1])}]')`);
  await page.waitFor("back to one pane", "document.querySelectorAll('.sf-pane-x').length === 0", { timeoutMs: 10000 });

  // A layout is view state: nothing here touched the front end (docs/23 §10.4).
  assert.deepEqual(page.requests.filter((r) => CONTROL.test(r.url)).map((r) => r.url), [],
    "a split-layout control reached a device route");
  assert.deepEqual(page.exceptions, [], "uncaught exception");
});
