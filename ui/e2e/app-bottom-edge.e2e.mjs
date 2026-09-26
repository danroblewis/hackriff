// T-1027 (user 2026-09-25 via the supervisor): nothing spans the bottom edge of the map.
//
//   "I really want that bottom bar gone in the UI. ... The bottom 'Selected — nothing yet: click a
//    signal or drag a region' bar also has an opaque background ..."
//
// docs/23 §10 P1: overlays are minimal, closeable and translucent; the map is the page; NOTHING OPAQUE
// SPANS AN EDGE. T-1026 made the detail card (the sheet that used to be the "Selected" strip) hidden
// until a feature is clicked, T-994 retired the Outputs dock, and the panes' clearance of both went
// with them. This spec holds the result at the page level, at 1280x800 and at 400 px, on the first
// paint of the real app over the replayed fixture:
//   (1) the canvas's last row IS the page's last pixel row — its box ends at `innerHeight` and the
//       panes are not inset above it (`data-inset-bottom` 0), so no unpainted band is left there;
//   (2) no element other than the canvas and the layers laid over it reaches into the bottom edge
//       zone (the last EDGE_PX rows) across half the width or more — the retired strip, a dock, a
//       sheet peek would each be one, floating margin and all;
//   (3) along rows of that zone, what is on top is the canvas at almost every point: a small
//       floating control may sit there, a band may not;
//   (4) the retired strip's empty-state hint lives in the Go-to glass's tooltip, not in a bar.
// Every action the strip offered is on the card itself (it WAS the card: `app-card.e2e.mjs` opens
// it by clicking a box, and the box context menu is T-994's `app-outputs-map.e2e.mjs`).
import test from "node:test";
import assert from "node:assert/strict";
import path from "node:path";
import { Browser } from "./harness.mjs";

const ORIGIN = process.env.HK_E2E_ORIGIN, TOKEN = process.env.HK_E2E_TOKEN;
const SHOTS = process.env.HK_E2E_SHOTS;

/** How far up from the last pixel row counts as "on the bottom edge", in CSS px. The strip this
 * retires, like the card it became, FLOATED a few px above the edge (a 10 px margin), so a band
 * that stops short of the last row by a margin is still a band along the edge. */
const EDGE_PX = 24;

/** Everything about the bottom edge, in one read. */
const EDGE = `(() => {
  const EDGE_PX = ${EDGE_PX};
  const canvas = document.querySelector('.sf-canvas');
  const c = canvas.getBoundingClientRect();
  const H = innerHeight, W = innerWidth;
  const name = (e) => e.tagName.toLowerCase() + (e.id ? '#' + e.id : '')
    + String(e.className?.baseVal ?? e.className ?? '').split(/\\s+/).filter(Boolean).map((k) => '.' + k).join('');
  const visible = (e) => { const s = getComputedStyle(e); return s.display !== 'none' && s.visibility !== 'hidden' && Number(s.opacity) > 0.05; };
  // (2) wide things on the bottom row that are not the canvas, a container it fills, or one of the
  // surface's overlay layers laid exactly over it (boxes, pins, labels, HUD: \`.sf-*\`, the map's
  // own box — whether they paint over the map is what (3) reads, point by point).
  const overMap = (r) => Math.abs(r.top - c.top) < 1 && Math.abs(r.bottom - c.bottom) < 1
    && Math.abs(r.left - c.left) < 1 && Math.abs(r.right - c.right) < 1;
  const wide = [...document.body.querySelectorAll('*')].filter((e) => {
    if (e === canvas || e.contains(canvas) || !visible(e)) return false;
    const r = e.getBoundingClientRect();
    // A layer of half the map's height or more (the floating-chrome host \`.map-ctl\`, inset 8 px) is a
    // layer, not a band; if it painted over the map, (3) would read it there.
    if (overMap(r) || r.height >= H * 0.5) return false;
    return r.width >= W * 0.5 && r.height > 0 && r.bottom >= H - EDGE_PX && r.top < H;
  }).map((e) => { const r = e.getBoundingClientRect();
    return { el: name(e), top: Math.round(r.top), h: Math.round(r.height), w: Math.round(r.width), bg: getComputedStyle(e).backgroundColor }; });
  // (3) the top element at points along rows of the bottom edge zone, the last pixel row included.
  const row = [];
  for (const y of [H - 1, H - EDGE_PX / 2, H - EDGE_PX]) for (let i = 0; i < 40; i++) {
    const x = (i + 0.5) * W / 40;
    const top = document.elementFromPoint(x, y);
    row.push(top === canvas ? 'canvas' : top ? name(top) : 'nothing');
  }
  const goto = document.querySelector('.map-ctl .map-goto');
  return { W, H, canvasTop: c.top, canvasBottom: c.bottom, canvasWidth: c.width,
    insetBottom: Number(canvas.dataset.insetBottom ?? 0) || 0, wide, row,
    gotoTitle: goto ? goto.getAttribute('title') : null };
})()`;

for (const [width, height] of [[1280, 800], [400, 800]]) {
  test(`at ${width}x${height} the canvas's last row is the page's last pixel and nothing spans the bottom edge`, async (t) => {
    const browser = await Browser.open();
    t.after(() => browser.close());
    const page = await browser.page(undefined, { width, height });
    const load = await page.goto(`${ORIGIN}/#token=${TOKEN}`);
    if (load !== "load") t.diagnostic(`the browser did not report \`load\` within its bound (${load}); waiting for the surface's own event`);
    await page.waitForSurfaceMounted({ timeoutMs: 60000 });
    await page.waitFor("the canvas and the floating Go-to",
      `!!document.querySelector('.sf-canvas') && document.querySelector('.sf-canvas').width > 200 &&
       !!document.querySelector('.map-ctl .map-goto')`, { timeoutMs: 60000 });
    // Let late chrome (the pills, the status stack, the fit pass) settle before reading the layout.
    await page.frames(60);
    if (SHOTS) await page.shot(path.join(SHOTS, `bottom-edge-${width}.png`));

    const e = await page.eval(EDGE);
    t.diagnostic(`edge: ${JSON.stringify(e)}`);
    assert.equal(e.W, width, "the viewport is not the size asked for");
    // (1)
    assert.ok(Math.abs(e.canvasBottom - e.H) < 0.5,
      `the canvas ends at ${e.canvasBottom}, not at the page's last pixel (${e.H}): something below it takes the bottom edge`);
    assert.ok(Math.abs(e.canvasWidth - e.W) < 0.5, `the canvas is ${e.canvasWidth} px wide, not full-bleed (${e.W})`);
    assert.equal(e.insetBottom, 0, `the panes stop ${e.insetBottom} px short of the canvas's bottom — an unpainted band along the edge`);
    // (2)
    assert.deepEqual(e.wide, [], `an element spans the bottom edge: ${JSON.stringify(e.wide)}`);
    // (3) Non-vacuity: the probe really sees the canvas along the edge (a covered row would name a band).
    const onCanvas = e.row.filter((n) => n === "canvas").length;
    assert.ok(onCanvas >= e.row.length * 0.75,
      `only ${onCanvas}/${e.row.length} points of the bottom edge zone are the map: ${JSON.stringify(e.row)}`);
    // (4)
    assert.match(String(e.gotoTitle), /click a signal.*shift\+drag.*region/,
      "the empty-state hint is not on the Go-to glass's tooltip");
    // And the retired wording is nowhere on the page.
    const text = await page.eval("document.body.innerText");
    assert.doesNotMatch(String(text), /nothing yet: click a signal/, "the retired strip's text is still on the page");
  });
}
