// T-882: the app's surface has no toolbar row; its controls float over the canvas
// (`ui/src/app/chrome/map-controls.ts`). These are the selectors and the one helper the specs share
// to reach them, so a spec says "split the viewport", not how the menu is built.

/** Open the viewport menu (top-right) and press one of its items: "split", "close" or "whole".
 * A real click on both, so an item a user cannot press fails here. */
export async function paneAct(page, act) {
  await page.click("document.querySelector('.map-pane-btn')");
  await page.waitFor("the viewport menu to open", "!document.querySelector('#map-pane-menu').hidden", { timeoutMs: 5000 });
  await page.click(`document.querySelector('#map-pane-menu [data-pane-act="${act}"]')`);
}

/** The retired `.sf-live` button's state, read off the FAB that replaced it: true when following. */
export const FOLLOWING = "document.querySelector('.map-fab').classList.contains('following')";

/** Every control T-882 rehomed from the toolbar row, plus the cluster it joined, closed state. */
export const CLOSED = ".map-goto input, .map-topright button, .map-zoom-in, .map-zoom-out, .map-fab";
/** Inside the viewport menu: Split, Close, Whole surface, Record IQ. */
export const PANE_ITEMS = "#map-pane-menu button";
/** Inside the layers menu: Signals (the detections overlay), Trace (view-wide) and the colour scale. */
export const LAYER_ITEMS = "#map-layers input[data-layer], #map-layers input[data-view-layer], #map-layers input[data-scale]";

/** T-528's hit test: each matched control, scrolled into view inside its own menu, must be what a
 * click at its centre lands on, and at least 16 px on a side. A menu's checkbox or radio is pressed
 * through its whole `<label>` row, so that row is the target measured. Returns the ones that are not. */
export const unclickable = (sel) => `JSON.stringify([...document.querySelectorAll(${JSON.stringify(sel)})].map((input) => {
  const el = input.closest('label') ?? input;
  el.scrollIntoView({ block: 'nearest', inline: 'nearest' });
  const r = el.getBoundingClientRect();
  const top = document.elementFromPoint(r.x + r.width / 2, r.y + r.height / 2);
  const id = input.dataset.paneAct ?? input.dataset.layer ?? input.dataset.viewLayer ?? input.dataset.scale ?? input.className ?? input.tagName;
  return { id: String(id), w: Math.round(r.width), h: Math.round(r.height),
           covered: top ? String(top.className?.baseVal ?? top.className ?? top.tagName) : 'nothing',
           ok: !!top && (top === el || el.contains(top)) && r.width >= 16 && r.height >= 16 && r.right <= innerWidth && r.bottom <= innerHeight };
}).filter((b) => !b.ok))`;

/** Hit-test every rehomed control: the closed cluster, then each menu opened in turn (and closed
 * again). Returns `{ closed, pane, layers, counts }` — the unpressable ones per group, and how many
 * were tested, so an empty result cannot come from matching nothing. */
export async function rehomedHitTest(page) {
  const count = (sel) => page.eval(`document.querySelectorAll(${JSON.stringify(sel)}).length`);
  const closed = JSON.parse(await page.eval(unclickable(CLOSED)));
  const counts = { closed: await count(CLOSED) };
  await page.click("document.querySelector('.map-pane-btn')");
  await page.waitFor("the viewport menu to open", "!document.querySelector('#map-pane-menu').hidden", { timeoutMs: 5000 });
  const pane = JSON.parse(await page.eval(unclickable(PANE_ITEMS)));
  counts.pane = await count(PANE_ITEMS);
  await page.click("document.querySelector('.map-pane-btn')");
  await page.click("document.querySelector('.map-layers-btn')");
  await page.waitFor("the layers menu to open", "!document.querySelector('#map-layers').hidden", { timeoutMs: 5000 });
  const layers = JSON.parse(await page.eval(unclickable(LAYER_ITEMS)));
  counts.layers = await count(LAYER_ITEMS);
  await page.click("document.querySelector('.map-layers-btn')");
  await page.waitFor("both menus to close",
    "document.querySelector('#map-layers').hidden && document.querySelector('#map-pane-menu').hidden", { timeoutMs: 5000 });
  return { closed, pane, layers, counts };
}
