// T-919 (user P1, docs/23 §10.6 rule 1; §10.2): the surface's floating status, in the real app.
//
// T-918 floated the rows that used to sit UNDER the stage (spectrum trace, IQ-ring rules, fog,
// priors, the per-viewport level readout, the orientation note) into one bottom-left box over the
// canvas, to make the canvas full-bleed. That box could grow to ~560 x 184 px — a large PERMANENT
// overlay on the waterfall, which is what P1 forbids: an overlay exists to be CLOSED, and a panel's
// default state is its smallest.
//
// So, at a wide width and a phone width, in a browser, over the product's own server:
//   1. the canvas is still full-bleed (T-918's invariant, which this must not buy back);
//   2. the status is COLLAPSED by default and small — bounded in both axes, far under T-918's box;
//   3. the honesty statements §10.2 names are on the picture in EVERY state: the viewport's
//      level/tier readout and the colour-scale sentence are in the collapsed line, non-empty;
//   4. expanding says the rest in words — the retention bound and the IQ horizon among them — and
//      collapsing/dismissing gives those pixels back to the map;
//   5. the dismiss is visible and is what a click at its own centre lands on, and Escape closes it
//      through the one overlay stack (T-900);
//   6. and nothing in any of that reaches a device route (§10.4: the view is not the radio).
import test from "node:test";
import assert from "node:assert/strict";
import { Browser } from "./harness.mjs";

const ORIGIN = process.env.HK_E2E_ORIGIN, TOKEN = process.env.HK_E2E_TOKEN;
const CONTROL = /\/api\/control\/(center|rate|window|gains|bias_tee|baseband_filter)/;

/** The collapsed box's bound. T-918's box was measured at ~560 x 184; a status LINE is a fraction
 * of that, and the bound is on the axis that was the complaint — HEIGHT, the map rows it eats.
 * Measured with this change: 560 x 29.4 at 1440 px, 340 x 51.4 at 420 px (where the level and the
 * scale wrap to two rows rather than cutting the tier out of the level). The width bound is the
 * right-edge cluster's column, which the line must never run under at a phone width. */
const BOUND = { h: 56, w: 560 };

/** Everything the assertions below read, in one round trip: the two boxes, the state, and the text
 * of each statement (read as `textContent`, which a hidden element still answers — so "the words
 * exist but are not on the picture" is distinguishable from "the words are gone"). */
const READ = `JSON.stringify((() => {
  const st = document.querySelector('.sf-status');
  const body = document.querySelector('.sf-status-body');
  const line = document.querySelector('.sf-status-line');
  const box = (el) => { const r = el.getBoundingClientRect(); return { x: r.x, y: r.y, w: r.width, h: r.height }; };
  const text = (sel) => (document.querySelector(sel)?.textContent ?? '').trim();
  return {
    open: st.dataset.open, expanded: document.querySelector('.sf-status-toggle').getAttribute('aria-expanded'),
    bodyHidden: body.hidden, bodyArea: box(body).w * box(body).h,
    closeShown: !document.querySelector('.sf-status-close').hidden,
    status: box(st), line: box(line),
    level: text('.sf-status-line .hk-surface-level'),
    where: text('.sf-status-line .hk-surface-where'),
    range: text('.sf-range'),
    trace: text('.sf-trace'), ring: text('.sf-ring'), note: text('.sf-note'),
    // Which pieces the box's height is actually made of — so a failed bound says WHAT grew.
    parts: [...line.children, ...line.querySelectorAll('.hk-surface-viewport, .hk-surface-level-note')]
      .map((c) => ({ cls: c.className, ...box(c) })),
  };
})())`;

const pressable = (sel) => `(() => { const e = document.querySelector(${JSON.stringify(sel)}); if (!e) return false;
  const r = e.getBoundingClientRect(); if (!(r.width > 0 && r.height > 0)) return false;
  const top = document.elementFromPoint(r.x + r.width / 2, r.y + r.height / 2); return !!top && (top === e || e.contains(top)); })()`;

for (const width of [1440, 420]) test(`at ${width} px the status is a compact line that expands, closes and never hides the honesty statements`, async (t) => {
  const browser = await Browser.open();
  t.after(() => browser.close());
  const page = await browser.page(undefined, { width, height: 860 });
  assert.equal(await page.goto(`${ORIGIN}/#token=${TOKEN}`), "load");
  await page.waitForSurfaceMounted({ timeoutMs: 60000 });
  // The level readout is written per render frame from the pane the renderer just drew, so waiting
  // for it is waiting for a frame that actually drew a pane — the state every assertion here is about.
  await page.waitFor("a viewport row with its level stated",
    `!!document.querySelector('.sf-status-line .hk-surface-level')?.textContent &&
     !!document.querySelector('.sf-status-toggle')`, { timeoutMs: 90000 });
  await page.frames(3);

  // (1) T-918's invariant: the canvas is 100vw x 100vh; this box floats OVER it, taking none of it.
  const bleed = await page.$rect(".sf-canvas");
  assert.deepEqual([bleed.x, bleed.y, bleed.w, bleed.h].map(Math.round), [0, 0, width, 860],
    `the canvas is not full-bleed at ${width} px: ${JSON.stringify(bleed)}`);

  // (2) Collapsed is the default, and collapsed is small.
  const shut = JSON.parse(await page.eval(READ));
  t.diagnostic(`at ${width} px collapsed: ${JSON.stringify(shut)}`);
  assert.equal(shut.open, "false", "the status box is expanded on arrival");
  assert.equal(shut.expanded, "false");
  assert.equal(shut.bodyHidden, true, "the detail is not hidden by default");
  assert.equal(shut.bodyArea, 0, "the hidden detail still takes pixels from the map");
  assert.ok(shut.status.h > 0 && shut.status.h <= BOUND.h,
    `the collapsed status is ${shut.status.h.toFixed(1)} px tall (bound ${BOUND.h})`);
  assert.ok(shut.status.w <= Math.min(BOUND.w, width - 80),
    `the collapsed status is ${shut.status.w.toFixed(1)} px wide (bound ${Math.min(BOUND.w, width - 80)})`);
  // ...and the scale sentence is not squeezed to a stub by the level beside it.
  const readoutW = shut.parts.find((p) => p.cls === "sf-readout")?.w ?? 0;
  assert.ok(readoutW >= 120, `the colour-scale readout is ${readoutW.toFixed(1)} px wide — too narrow to read`);

  // (3) §10.2: the tier/level statement and the colour scale are ON THE PICTURE while collapsed —
  // legible, not merely present in the DOM (a zero-area line would be hiding them by another name).
  assert.match(shut.level, /\S/, "the collapsed line does not state the level/tier it was drawn at");
  assert.match(shut.range, /dB/, `the collapsed line does not state the colour scale: ${JSON.stringify(shut.range)}`);
  assert.ok(shut.line.w > 0 && shut.line.h > 0, "the always-visible line is not drawn");
  assert.equal(await page.eval(`(() => { const e = document.querySelector('.sf-status-line .hk-surface-level');
    const r = e.getBoundingClientRect(); const top = document.elementFromPoint(r.x + 2, r.y + r.height / 2);
    return !!top && (top === e || e.contains(top) || e.parentElement.contains(top)); })()`), true,
    "the level statement is covered by something else");
  // The dismiss belongs to the OPEN overlay: there is nothing to close while it is already a line.
  assert.equal(shut.closeShown, false, "a dismiss is offered for an overlay that is not open");

  // (4) Expanding says the rest in words, including the two capture rules (T-506).
  assert.equal(await page.eval(pressable(".sf-status-toggle")), true, "the expand control is not pressable");
  await page.click("document.querySelector('.sf-status-toggle')");
  await page.waitFor("the detail to open", `document.querySelector('.sf-status').dataset.open === 'true'`, { timeoutMs: 5000 });
  await page.frames(2);
  const open = JSON.parse(await page.eval(READ));
  t.diagnostic(`at ${width} px expanded: ${JSON.stringify({ status: open.status, ring: open.ring.slice(0, 120) })}`);
  assert.equal(open.bodyHidden, false);
  assert.equal(open.expanded, "true");
  assert.ok(open.bodyArea > 0, "the expanded detail draws nothing");
  assert.ok(open.status.h > shut.status.h, "expanding did not add anything to the box");
  assert.match(open.ring, /retention bound/, "the expanded status does not state the retention bound in words");
  assert.match(open.ring, /oldest IQ|IQ ring/, "the expanded status does not state the IQ horizon in words");
  assert.match(open.level, /\S/, "expanding lost the level/tier statement");
  assert.match(open.range, /dB/, "expanding lost the colour-scale statement");

  // (5) The dismiss is visible, pressable, and gives those pixels back — to the LINE, not to nothing.
  assert.equal(await page.eval(pressable(".sf-status-close")), true, "the status detail's close (×) is not visible and pressable");
  await page.click("document.querySelector('.sf-status-close')");
  await page.waitFor("the detail to close", `document.querySelector('.sf-status').dataset.open === 'false'`, { timeoutMs: 5000 });
  await page.frames(2);
  const again = JSON.parse(await page.eval(READ));
  assert.equal(again.bodyArea, 0, "the dismissed detail still takes pixels from the map");
  assert.ok(again.status.h <= BOUND.h, `after the dismiss the box is ${again.status.h.toFixed(1)} px tall`);
  assert.equal(again.closeShown, false, "the dismiss is still offered for a closed overlay");
  assert.match(again.level, /\S/, "the dismiss took the level/tier statement off the picture");
  assert.match(again.range, /dB/, "the dismiss took the colour-scale statement off the picture");

  // ...and Escape closes it too, through the one stack (T-900) — one press, from re-opened.
  await page.click("document.querySelector('.sf-status-toggle')");
  await page.waitFor("the detail to re-open", `document.querySelector('.sf-status').dataset.open === 'true'`, { timeoutMs: 5000 });
  await page.key("Escape");
  await page.waitFor("Escape to close the detail", `document.querySelector('.sf-status').dataset.open === 'false'`, { timeoutMs: 5000 });
  const last = JSON.parse(await page.eval(READ));
  assert.ok(last.status.h <= BOUND.h && last.line.h > 0, `after Escape: ${JSON.stringify(last.status)}`);

  // (6) None of it is a device command.
  assert.deepEqual(page.requests.filter((r) => CONTROL.test(r.url)).map((r) => r.url), [],
    "expanding or closing the status reached a device route");
  assert.deepEqual(page.exceptions, [], "uncaught exception");
});
