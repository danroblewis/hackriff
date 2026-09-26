// T-993 (MMAP): the app-shell top bar RETIRES over the map. docs/23 §10 P1 (the map's pixels are
// the map's) and P4 (small controls for big view changes). User, 2026-09-25 on staging: the top bar
// with "the Explore/Decode/History tab buttons, the Review button, the Theme button, and the nudge
// buttons" was still there after T-801.
//
// What each assertion is a property of, at 1280 x 800 and at 400 x 820:
//  1. NO BAR — the page's own layout: the canvas's top row is the page's first pixel row
//     (`.sf-canvas` top == 0), the old `<header class="bar">` takes no box, and no chrome element
//     outside the canvas spans the width across the top (the shape of a bar, whatever its class).
//  2. REACHABLE — a REAL click (the harness clicks at the element's centre, so anything on top of it
//     takes the click instead) on every former bar action, in at most two clicks:
//       Explore/Decode/History (1 click each, the floating pill, and the framed bar back in Decode);
//       Review (1 click: the top-right cluster's icon button opens the drawer);
//       Theme (2 clicks: the cluster's ⋯, then Theme — the button's own label changes);
//       a tuning nudge (1 click, beside Go-to) reaches the device through the one gated DeviceAction
//       path — against the MOCK SDR device, the only way a nudge is enabled without a radio.
//  3. CHIPS, NOT A BAR (T-1025) — the user, on T-993's chrome: "The top bar *looks* like it's an
//     overlay, but the background is opaque, so it's still effectively a top bar." So of the top
//     EDGE band (anything whose box starts within 24 px of the top): no element that PAINTS a
//     background is wider than a third of the window, the painted chips leave gaps between them,
//     a press in a gap lands on the canvas (the row takes no pointer), and the gap's PIXELS are
//     the canvas's, not chrome's — proved by hiding the chrome and re-photographing the SAME
//     rectangle (T-1031), never by sampling a second rectangle elsewhere on the map.
//  4. VIEW/DEVICE LINE — nothing but the nudge press reaches a device route.
//
// `HK_E2E_SHOTS=<dir>` saves a screenshot per width (and one of the ⋯ menu open).
import test, { after } from "node:test";
import assert from "node:assert/strict";
import path from "node:path";
import { Browser, census, pixelDiff } from "./harness.mjs";
import { startBackend } from "./backend.mjs";

const SHOTS = process.env.HK_E2E_SHOTS ?? null;
const CONTROL = /\/api\/control\/(center|rate|window|gains|bias_tee|baseband_filter)/;
/** T-1025: "no element wider than ~1/3 of the pane spans the top edge", with a pixel of slack. */
const innerThird = (w) => Math.round(w / 3) + 1;

// Its own backend on the MOCK device (lane base + 24; the other second-backend specs use +16), so
// the nudges are live and enabled; a plain `--replay` states them disabled and proves only that half.
let backendP = null;
const backend = () => (backendP ??= startBackend({ port: Number(process.env.HK_E2E_PORT ?? 8791) + 24, mockDevice: true }));
after(async () => { (await backendP?.catch(() => null))?.stop(); });

/** Visible, top-of-page elements outside the canvas that span most of the width: a bar, by shape. */
const BAR_SHAPED = `JSON.stringify([...document.querySelectorAll('body *')].filter((e) => {
  const r = e.getBoundingClientRect();
  if (!r.width || !r.height || r.top > 60 || r.height > 120 || r.width < innerWidth * 0.8) return false;
  const cs = getComputedStyle(e);
  if (cs.visibility === 'hidden' || cs.display === 'none') return false;
  // The canvas and its own ancestors are the map, not chrome over it.
  return !e.contains(document.querySelector('.sf-canvas'));
}).map((e) => e.tagName + '.' + String(e.className?.baseVal ?? e.className)))`;

/** A real click lands on it: on screen, >= 24 px, and it is what a press at its centre hits. */
const pressable = (sel) => `JSON.stringify([...document.querySelectorAll(${JSON.stringify(sel)})].map((el) => {
  const r = el.getBoundingClientRect();
  const top = r.width && r.height ? document.elementFromPoint(r.x + r.width / 2, r.y + r.height / 2) : null;
  return { sel: String(el.id || el.className), w: Math.round(r.width), h: Math.round(r.height),
    covered: top ? String(top.className?.baseVal ?? top.className ?? top.tagName) : 'nothing',
    ok: !!top && (top === el || el.contains(top)) && r.width >= 24 && r.height >= 24 &&
        r.left >= 0 && r.top >= 0 && r.right <= innerWidth && r.bottom <= innerHeight };
}).filter((b) => !b.ok))`;

/** Everything that PAINTS a background in the top-edge band (a box starting within 24 px of the
 * top), with the canvas's own ancestors excluded — they are the map, not chrome over it. Nested
 * paint inside a chip (a pressed mode button, the device's live dot) is included on purpose: the
 * width bound is about anything that reads as a surface across the top, whatever its nesting. */
const TOP_EDGE_PAINT = `JSON.stringify([...document.querySelectorAll('body *')].map((e) => {
  const r = e.getBoundingClientRect(), cs = getComputedStyle(e);
  const a = (/rgba?\\(([^)]*)\\)/.exec(cs.backgroundColor)?.[1] ?? '').split(',')[3];
  return { el: e, r, alpha: a === undefined ? (cs.backgroundColor === 'transparent' ? 0 : 1) : parseFloat(a),
           hidden: cs.visibility === 'hidden' || cs.display === 'none' };
}).filter((b) => b.r.width && b.r.height && b.r.top <= 24 && !b.hidden && b.alpha >= 0.05 &&
                 !b.el.contains(document.querySelector('.sf-canvas')))
  .map((b) => ({ sel: b.el.tagName + '.' + String(b.el.className?.baseVal ?? b.el.className ?? ''),
                 x: Math.round(b.r.left), w: Math.round(b.r.width), y: Math.round(b.r.top), h: Math.round(b.r.height) })))`;

/**
 * Hide the top-edge chrome (T-1031), so the SAME rectangle can be re-photographed showing only what
 * the canvas painted under it. Everything visible whose box starts in the top edge band and is not
 * an ancestor of the canvas is hidden at its chrome ROOT — the outermost ancestor that is still not
 * an ancestor of the canvas — so a re-render inside a chip while the shot is taken stays hidden,
 * and so does a background painted by a chrome element's `::before`, which no `querySelectorAll`
 * can see. (`TOP_EDGE_PAINT`'s alpha filter is deliberately NOT applied here: the question is which
 * pixels chrome contributes, and a wrapper that paints only through a pseudo-element reports no
 * background of its own.) What this cannot reach is a strip painted by `html`/`body` itself, whose
 * root is the canvas's own ancestor — assertion (1)'s bar-shape rule is what stands there.
 * `visibility` keeps every box, so nothing reflows and the gaps measured from the DOM still
 * describe the pixels. Returns how many roots it hid.
 */
const HIDE_TOP_CHROME = `(() => {
  const canvas = document.querySelector('.sf-canvas');
  const roots = new Set();
  for (const e of document.querySelectorAll('body *')) {
    const r = e.getBoundingClientRect(), cs = getComputedStyle(e);
    if (!r.width || !r.height || r.top > 24) continue;
    if (cs.visibility === 'hidden' || cs.display === 'none') continue;
    if (e.contains(canvas)) continue;
    let root = e;
    while (root.parentElement && root.parentElement !== document.body && !root.parentElement.contains(canvas)) {
      root = root.parentElement;
    }
    roots.add(root);
  }
  for (const el of roots) el.setAttribute('data-e2e-chrome-hidden', '');
  if (!document.querySelector('#e2e-hide-chrome')) {
    const s = document.createElement('style');
    s.id = 'e2e-hide-chrome';
    s.textContent = '[data-e2e-chrome-hidden] { visibility: hidden !important; }';
    document.head.append(s);
  }
  return roots.size;
})()`;

const SHOW_TOP_CHROME = `(() => {
  for (const el of document.querySelectorAll('[data-e2e-chrome-hidden]')) el.removeAttribute('data-e2e-chrome-hidden');
  document.querySelector('#e2e-hide-chrome')?.remove();
  return document.querySelectorAll('[data-e2e-chrome-hidden]').length;
})()`;

/** (3) CHIPS, NOT A BAR — run in BOTH themes, since the theme decides which of chip and canvas is
 * the darker and a guard that only holds in one of them is off half the time. */
async function chipsNotABar(t, page, W, theme) {
  const chips = JSON.parse(await page.eval(TOP_EDGE_PAINT));
  t.diagnostic(`at ${W} (${theme}) top-edge paint: ${JSON.stringify(chips)}`);
  assert.ok(chips.length >= 4, `the top edge holds ${chips.length} painted elements — the chips are missing`);
  assert.deepEqual(chips.filter((c) => c.w > innerThird(W)), [],
    `a painted element spans more than a third of the ${W} px window across the top edge`);

  // The gaps between them, from the painted boxes' union: canvas, by construction.
  const cov = chips.map((c) => [c.x, c.x + c.w]).sort((a, b) => a[0] - b[0]);
  const gaps = [];
  let reach = cov[0][1];
  for (const [x0, x1] of cov.slice(1)) {
    if (x0 - reach >= 4) gaps.push({ x: reach, w: x0 - reach });
    reach = Math.max(reach, x1);
  }
  t.diagnostic(`at ${W} (${theme}) gaps: ${JSON.stringify(gaps)}`);
  assert.ok(gaps.length >= 3, `only ${gaps.length} gaps between the top chips — they read as one strip`);

  // A press in a gap reaches the MAP, so the rows holding the chips take no pointer of their own.
  const rowY = Math.round(chips[0].y + chips[0].h / 2);
  const hits = JSON.parse(await page.eval(`JSON.stringify(${JSON.stringify(gaps)}.map((g) => {
    const el = document.elementFromPoint(g.x + g.w / 2, ${rowY});
    return { x: g.x, hit: el ? String(el.className?.baseVal ?? el.className ?? el.tagName) : 'nothing' };
  }).filter((h) => !/sf-canvas/.test(h.hit)))`));
  assert.deepEqual(hits, [], "a press in a gap between the top chips is swallowed by chrome instead of reaching the canvas");

  // And the gap's PIXELS are the canvas's, not chrome's. Asked of ONE place (T-1031): photograph
  // the band, hide the chrome, photograph the SAME band again. Where chrome paints, the pixels
  // change; where the map shows through, they do not — beyond the canvas's own motion between two
  // shots, which the second canvas-only shot measures at that very rectangle. The old form sampled
  // a second rectangle 60 px below the chips and compared luma with it; that rectangle is map, so
  // about one run in four it held a detection box, a guide line or unobserved grey and the guard
  // decided on the map's contents instead of on the chrome. Tolerance is not the fix and is not
  // widened: the reference moved to the same point.
  const shown = await page.shot();
  const hidden = await page.eval(HIDE_TOP_CHROME);
  const bare = await page.shot();
  const bareAgain = await page.shot();
  assert.equal(await page.eval(SHOW_TOP_CHROME), 0, "the top chrome stayed hidden");
  await page.frames(2);
  t.diagnostic(`at ${W} (${theme}) hid ${hidden} chrome roots for the pixel comparison`);
  assert.ok(hidden >= 1, "no chrome root to hide — the top-edge chips were not found");

  for (const g of gaps.filter((x) => x.w >= 6).slice(0, 4)) {
    const band = { y: rowY - 6, h: 12 };
    const gapRect = { x: g.x + 2, y: band.y, w: g.w - 4, h: band.h };
    const left = chips.filter((c) => c.x + c.w <= g.x + 2).sort((a, b) => b.x - a.x)[0];
    const chipRect = { x: left.x + 4, y: band.y, w: Math.max(4, Math.min(24, left.w - 8)), h: band.h };
    // Three numbers at the SAME rectangles: what hiding the chrome did to the gap, what it did to
    // the chip's own face, and what the canvas did on its own over one shot's interval.
    const gapChange = pixelDiff(shown, bare, gapRect);
    const chipChange = pixelDiff(shown, bare, chipRect);
    const motion = pixelDiff(bare, bareAgain, gapRect);
    t.diagnostic(`at ${W} (${theme}) gap@${g.x} change ${gapChange.meanAbs.toFixed(2)} ` +
      `(${(gapChange.changedShare * 100).toFixed(0)} %) vs chip ${left.sel} ${chipChange.meanAbs.toFixed(2)} ` +
      `(${(chipChange.changedShare * 100).toFixed(0)} %) vs canvas motion ${motion.meanAbs.toFixed(2)}; ` +
      `luma gap ${census(shown, gapRect).meanLuma.toFixed(1)} chip ${census(shown, chipRect).meanLuma.toFixed(1)}`);
    // The control that makes the comparison mean anything: hiding a chip must change the chip's own
    // face by more than the canvas moves by itself. Without it a hide that silently failed would
    // leave every difference at zero and the guard would pass saying nothing.
    assert.ok(chipChange.meanAbs > motion.meanAbs,
      `hiding the chips changed the face of ${left.sel} by ${chipChange.meanAbs.toFixed(2)}, no more than the ` +
      `canvas moved by itself (${motion.meanAbs.toFixed(2)}) — the chips were not hidden, so this comparison proves nothing`);
    // And the claim itself. Distance decides, as before, but now between two measurements of the
    // same rectangle: the gap behaves like untouched map, not like a chip being removed.
    assert.ok(Math.abs(gapChange.meanAbs - motion.meanAbs) < Math.abs(gapChange.meanAbs - chipChange.meanAbs),
      `the gap at x=${g.x} (${theme}) changed by ${gapChange.meanAbs.toFixed(2)} when the top chrome was hidden, ` +
      `nearer the chip's own ${chipChange.meanAbs.toFixed(2)} than the canvas's own motion ${motion.meanAbs.toFixed(2)} ` +
      `— chrome paints in the gap, so the chips read as a bar`);
  }
}

for (const [W, H] of [[1280, 800], [400, 820]]) test(`at ${W} px the top bar is gone and every one of its actions is a click or two away`, async (t) => {
  const be = await backend();
  const browser = await Browser.open();
  t.after(() => browser.close());
  const page = await browser.page(undefined, { width: W, height: H });
  assert.equal(await page.goto(`${be.origin}/#token=${be.token}`), "load");
  await page.waitForSurfaceMounted({ timeoutMs: 240000 });
  await page.waitFor("the bar's controls to float on the map",
    `!!document.querySelector('.map-status .modes') && !!document.querySelector('.map-topright #review-btn')
     && !!document.querySelector('.map-nudge .nudge-btn') && !!document.querySelector('.map-more-body #theme-btn')`, { timeoutMs: 120000 });
  await page.frames(5);
  if (SHOTS) await page.shot(path.join(SHOTS, `top-chrome-${W}.png`));

  // (1) No bar.
  const geo = JSON.parse(await page.eval(`JSON.stringify({
    canvasTop: document.querySelector('.sf-canvas').getBoundingClientRect().top,
    bar: (() => { const r = document.querySelector('.app > .bar').getBoundingClientRect(); return [r.width, r.height]; })(),
    barDisplay: getComputedStyle(document.querySelector('.app > .bar')).display,
    sw: Math.max(document.documentElement.scrollWidth, document.body.scrollWidth), iw: innerWidth })`));
  t.diagnostic(`at ${W}: ${JSON.stringify(geo)}`);
  assert.equal(geo.canvasTop, 0, "the canvas's top row is not the page's first pixel row");
  assert.equal(geo.barDisplay, "none", "the old top bar still takes a box over the map");
  assert.deepEqual(geo.bar, [0, 0]);
  assert.deepEqual(JSON.parse(await page.eval(BAR_SHAPED)), [], "a chrome element spans the width across the top — a bar by another name");
  assert.ok(geo.sw <= geo.iw, `the page scrolls sideways at ${W} px`);

  // Every former bar control on the map face is pressable where it sits (Theme is inside ⋯, below).
  const FACE = ".map-status .mode, .map-topright #review-btn, .map-topright .map-more-btn, .map-nudge .nudge-btn, .map-goto input, .map-topright button:not([hidden])";
  assert.deepEqual(JSON.parse(await page.eval(pressable(FACE))), [], "a former top-bar control is off screen, too small or covered");
  // Go-to never slides under the cluster, nor the nudges under the pill.
  const overlap = (a, b) => page.eval(`(() => { const A = document.querySelector(${JSON.stringify(a)}).getBoundingClientRect(),
    B = document.querySelector(${JSON.stringify(b)}).getBoundingClientRect();
    return A.left < B.right && A.right > B.left && A.top < B.bottom && A.bottom > B.top; })()`);
  for (const [a, b] of [[".map-goto", ".map-topright"], [".map-nudge", ".map-status"], [".map-goto", ".map-status"], [".map-status", ".map-topright"], [".map-nudge", ".map-topright"]]) {
    assert.equal(await overlap(a, b), false, `${a} overlaps ${b}`);
  }

  // (3) CHIPS, NOT A BAR, in the theme this run started in (and again after Theme flips it, below).
  await chipsNotABar(t, page, W, await page.eval("document.documentElement.dataset.theme ?? 'default'"));

  // (4) Review: one click on the cluster's icon button opens the drawer; its own Close shuts it.
  await page.click("document.querySelector('.map-topright #review-btn')");
  await page.waitFor("the review drawer to open", "!document.querySelector('#review').hidden && !!document.querySelector('#review .rv-head button')", { timeoutMs: 5000 });
  await page.click("document.querySelector('#review .rv-head button')");
  await page.waitFor("the review drawer to close", "document.querySelector('#review').hidden", { timeoutMs: 5000 });

  // Theme: two clicks — ⋯, then Theme. The button states the theme it set.
  const before = await page.$text("#theme-btn");
  await page.click("document.querySelector('.map-more-btn')");
  await page.waitFor("the ⋯ menu to open", "!document.querySelector('#map-more-menu').hidden", { timeoutMs: 5000 });
  if (SHOTS) await page.shot(path.join(SHOTS, `top-chrome-${W}-more.png`));
  await page.click("document.querySelector('#map-more-menu #theme-btn')");
  await page.waitFor("the theme to change", `document.querySelector('#theme-btn').textContent !== ${JSON.stringify(before)}`, { timeoutMs: 5000 });
  await page.click("document.querySelector('#map-more-menu .map-more-close')");
  await page.waitFor("the ⋯ menu to close", "document.querySelector('#map-more-menu').hidden", { timeoutMs: 5000 });
  // (3, again) the chips hold in EVERY theme, not just the one the run happened to start in: Theme
  // cycles system -> dark -> light, so keep pressing it and re-check until the light one has been
  // seen too. The gap/chip contrast reverses between them, which is exactly why this repeats.
  const seen = new Set();
  for (let i = 0; i < 4; i++) {
    await page.frames(3);
    const theme = await page.eval("document.documentElement.dataset.theme ?? 'default'");
    if (!seen.has(theme)) { seen.add(theme); await chipsNotABar(t, page, W, theme); }
    if (seen.has("light") && seen.has("dark")) break;
    await page.click("document.querySelector('.map-more-btn')");
    await page.waitFor("the ⋯ menu to open", "!document.querySelector('#map-more-menu').hidden", { timeoutMs: 5000 });
    await page.click("document.querySelector('#map-more-menu #theme-btn')");
    await page.click("document.querySelector('#map-more-menu .map-more-close')");
    await page.waitFor("the ⋯ menu to close", "document.querySelector('#map-more-menu').hidden", { timeoutMs: 5000 });
  }
  assert.ok(seen.has("light") && seen.has("dark"), `the chip guard never saw both themes (saw ${[...seen]})`);

  // Modes: one click each. In Decode/History the framed shell's bar carries the switch back.
  await page.click("document.querySelector('.map-status .mode[data-mode=decode]')");
  await page.waitFor("Decode to show", "!document.querySelector('#view-decode').hidden && document.querySelector('#view-explore').hidden", { timeoutMs: 5000 });
  await page.click("document.querySelector('.app > .bar .mode[data-mode=history]')");
  await page.waitFor("History to show", "!document.querySelector('#view-history').hidden", { timeoutMs: 5000 });
  await page.click("document.querySelector('.app > .bar .mode[data-mode=explore]')");
  await page.waitFor("Explore to show with its floating switch back",
    "!document.querySelector('#view-explore').hidden && !!document.querySelector('.map-status .mode[data-mode=explore][aria-pressed=true]')", { timeoutMs: 5000 });
  assert.equal(await page.eval("document.querySelectorAll('#review-btn').length"), 1, "a control was copied, not moved");
  assert.deepEqual(page.requests.filter((r) => CONTROL.test(r.url)).map((r) => r.url), [],
    "Review, Theme or a mode switch reached a device route");

  // A nudge: one click beside Go-to, and it goes to the device (the one gated DeviceAction path).
  await page.waitFor("a nudge to be enabled (the mock device is live)",
    "!!document.querySelector('.map-nudge .nudge-btn[data-ideal]:not(:disabled)')", { timeoutMs: 60000 });
  await page.click("document.querySelector('.map-nudge .nudge-btn[data-ideal]:not(:disabled)')");
  const deadline = Date.now() + 15000;
  while (!page.requests.some((r) => CONTROL.test(r.url)) && Date.now() < deadline) await new Promise((r) => setTimeout(r, 100));
  assert.ok(page.requests.some((r) => CONTROL.test(r.url)), "the nudge press reached no device route");
  assert.deepEqual(page.exceptions, [], "uncaught exception");
});
