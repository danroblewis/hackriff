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

/**
 * **Has an animating overlay ARRIVED?** (T-958.)
 *
 * The sheet grows and shrinks from a fixed BOTTOM edge over a .28 s height transition
 * (`ui/src/app/chrome/sheet.css`), so its head — the close (×) and the grab handle — travels the
 * whole difference between two snap heights. Measured at 400 x 820: when the wait on
 * `dataset.snap` returns, the close button is at y 718 with the sheet still 56 px tall and its
 * target already 369 px; 500 ms later it has arrived at y 405. **313 px after the wait said "open".**
 *
 * That matters because [[Page.click]] reads the element's rect and then dispatches a real mouse
 * event at that point one round-trip later: aimed at a rect read mid-move, the press lands in the
 * body below the button and nothing happens. On a loaded box `app-phone` timed out on "the sheet's
 * close to collapse it" 1 run in 4 (T-933 worker) and 1 in 6 here, on unmodified main.
 *
 * The fix is to wait on what the page reports about ITSELF — never on a sleep, and never by raising
 * the deadline (docs/10 §3.6). Two reports, because either alone is true mid-move:
 *
 *  - `getAnimations()` is empty in the window between the style mutation and the style recalc that
 *    creates the transition — i.e. exactly when a test polls straight after the click;
 *  - the rendered height equals the target for the whole of a transition that has not started yet,
 *    and `el.style.height` is the height the product itself set from `snapHeights` (an element with
 *    no inline height, like a menu, is judged by `getAnimations()` alone).
 *
 * The deadline below bounds a HANG only: a green run returns as soon as the page agrees with itself.
 */
export const arrived = (sel) => `(() => { const e = document.querySelector(${JSON.stringify(sel)}); if (!e) return false;
  const want = parseFloat(e.style.height);
  return e.getAnimations().length === 0 &&
    (!Number.isFinite(want) || Math.abs(e.getBoundingClientRect().height - want) < 1); })()`;

/** Wait for `sel` to stop moving before clicking or measuring it — see [[arrived]]. */
export const settled = (page, sel, what, { timeoutMs = 5000 } = {}) =>
  page.waitFor(`${what} to finish moving`, arrived(sel), { timeoutMs });
