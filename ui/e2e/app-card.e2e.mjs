// T-1026 (user 2026-09-25 via the supervisor): the detail card is a Google-Maps PLACE CARD, in the
// real app over the replayed fixture, at 1280x800 and at 400 px.
//
//   "The bottom right accordion panel is kind of dumb to show all the time, it should be hidden all
//    the time until someone clicks on something, like on Google Maps how someone clicks something and
//    it opens the detailed view, and if they click on the back of the map it goes away, or if they
//    click on another interest point it changes."
//
// The journey, in the order the user described it:
//   (1) the first paint has NO card — nothing of it on screen, and nothing of it along the bottom edge;
//   (2) clicking a detection's BOX opens the card on that box's identity (the same centre frequency the
//       box's own accessible name states), pressed OFF THE BOX'S CENTRE so what is proved is a polygon
//       hit test (docs/23 §10.6 rule 6) rather than a click on a point;
//   (3) clicking another feature SWAPS the card's content — the card is never closed and re-opened in
//       between (a MutationObserver on `hidden` records every transition, and there must be none);
//   (4) Escape closes it, and (5) a click on bare map closes it — both leaving every pixel to the map;
//   (6) an inventory pill (T-997) opens it ON ITS LIST with no selection invented.
// Nothing above may reach a device route: selection is view state (docs/23 §10.4).
//
// The unit tier (`ui/test/app-card.test.ts`) proves the state machine, the dismiss gestures and the
// wiring over a fake DOM with a spy client. What needs a browser, and is only here, is the real hit
// test: which element a click at a point lands on, and that a click where no feature is lands on the
// map itself.
import test from "node:test";
import assert from "node:assert/strict";
import path from "node:path";
import { Browser } from "./harness.mjs";
import { settled } from "./app-chrome.mjs";

const ORIGIN = process.env.HK_E2E_ORIGIN, TOKEN = process.env.HK_E2E_TOKEN;
const SHOTS = process.env.HK_E2E_SHOTS;
const CONTROL = /\/api\/control\/(center|rate|window|gains|bias_tee|baseband_filter)/;
// The minimap strip along the canvas's bottom edge (`MINIMAP_PX` in `app/centre/surface.ts`): a click
// there is a click on the minimap, not on bare map.
const MINIMAP_PX = 110;

/** Everything about the card, in one read. */
const CARD = `JSON.stringify((() => {
  const el = document.querySelector('.sheet');
  if (!el) return { mounted: false };
  const r = el.getBoundingClientRect();
  return { mounted: true, hidden: !!el.hidden, open: el.dataset.open ?? null, snap: el.dataset.snap ?? null,
    h: Math.round(r.height), y: Math.round(r.y), bottom: Math.round(r.bottom),
    title: document.querySelector('.sheet-title')?.textContent ?? null,
    bigf: document.querySelector('.focus .detail .bigf')?.textContent ?? null,
    tab: document.querySelector('.side-inv .tab[aria-selected="true"]')?.dataset.tab ?? null };
})())`;
const card = async (page) => JSON.parse(await page.eval(CARD));

/**
 * **One in-page function chooses the box AND the point to press it at, together** (T-1032).
 *
 * The first version asked for a box whose whole rectangle was inside the canvas and clear of the
 * minimap strip, then computed a press point from that rectangle afterwards. Both halves were wrong
 * under load, in ways that cost three different reds:
 *
 *  - **An ongoing signal's box grows to the live edge by design** (ADR-0017/0019: `[start, end?]`,
 *    ongoing until an end is found), so its bottom edge reaches the bottom of the canvas — where the
 *    minimap strip is. Requiring the whole box to be inside therefore asks a product invariant not to
 *    hold: the spec passed only while the fixture's signals were still young. Measured on a box at
 *    load 7: three confirmed pins, every one rejected, and 120 s of waiting for a box that would
 *    never appear. What a click needs is ONE point, so one point is what is required.
 *  - **A rectangle read in one `eval` and hit-tested in the next is two layouts** (`e2e/README.md`).
 *    The box moves every frame — it scrolls with the time axis — so the point is chosen and checked
 *    in the same evaluation, against the browser's own hit test.
 *
 * The point is inside the box's visible part, off the box's centre in both axes where the box is big
 * enough for that to mean anything (the polygon hit test, docs/23 §10.6 rule 6), and it is a point
 * the canvas is under — so the press goes through the surface's own hit test rather than landing on
 * chrome or on the open card. `exclude` skips a pin by id (the swap needs a DIFFERENT feature);
 * `only` restricts it to one id (step 5 re-opens the card on the SAME feature it was closed from).
 */
const pickBox = ({ exclude = null, only = null } = {}) => `(() => {
  const cv = document.querySelector('.sf-canvas');
  if (!cv) return null;
  const c = cv.getBoundingClientRect();
  const dpr = window.devicePixelRatio || 1;
  const y1 = c.bottom - ${MINIMAP_PX} / dpr - 2;   // above the minimap strip: a click there is the minimap
  const skip = ${JSON.stringify(exclude)}, only = ${JSON.stringify(only)};
  const canvasAt = (x, y) => document.elementFromPoint(x, y)?.classList.contains('sf-canvas');
  const pins = [...document.querySelectorAll('.sf-pins .sf-pin')];
  // **A point inside ONE feature only.** Detection boxes overlap — candidates churning over the same
  // energy is the normal case (and an overlap is itself an error signal the backend re-analyses) —
  // and the surface's own hit test resolves an ambiguous point by ITS priority, not by which pin the
  // test had in mind. A press meant for a feature must therefore be inside that feature and outside
  // every other one, or the spec is asking "select this" while the product hears "select whichever".
  // Measured at 400 px: four presses inside the second box, every one of them answered with the FIRST
  // box still selected, because its 15 px column overlapped the point.
  const alone = (x, y, self) => !pins.some((o) => {
    if (o === self) return false;
    const r = o.getBoundingClientRect();
    return r.width > 0 && r.height > 0 && x >= r.x - 2 && x <= r.right + 2 && y >= r.y - 2 && y <= r.bottom + 2;
  });
  let best = null;
  for (const p of document.querySelectorAll('.sf-pins .sf-pin.detection')) {
    if (skip && p.dataset.pin === skip) continue;
    if (only && p.dataset.pin !== only) continue;
    const r = p.getBoundingClientRect();
    if (r.width <= 0 || r.height <= 0) continue;
    const area = p.classList.contains('area');
    const q = { id: p.dataset.pin, label: p.getAttribute('aria-label'), area,
      x: r.x, y: r.y, w: r.width, h: r.height, cx: r.x + r.width / 2, cy: r.y + r.height / 2 };
    if (!area) {
      // A box too small to draw is generalized to a symbol (docs/23 §10.6 rule 6): its own point is
      // all there is to press, so it is taken only if the canvas is under it.
      if (q.cx < c.x + 2 || q.cx > c.right - 2 || q.cy < c.y + 2 || q.cy > y1) continue;
      if (!canvasAt(q.cx, q.cy) || !alone(q.cx, q.cy, p)) continue;
      q.press = { x: q.cx, y: q.cy, offCentre: false };
    } else {
      // The part of the box that is on the canvas and clear of the minimap — an ongoing box's own
      // bottom is the live edge, which is under that strip.
      const vx0 = Math.max(r.x, c.x + 2), vx1 = Math.min(r.right, c.right - 2);
      const vy0 = Math.max(r.y, c.y + 2), vy1 = Math.min(r.bottom, y1);
      if (vx1 - vx0 < 4 || vy1 - vy0 < 4) continue;
      // Off the centre in both axes when the visible part is big enough to have an off-centre point
      // at all, and a point the canvas is genuinely under: the x candidates walk in from the left
      // edge, the y candidates down from the top, each 3 px clear of the edge it comes from.
      const xs = [], ys = [];
      for (let f = 0.2; f <= 0.8001; f += 0.15) {
        xs.push(vx0 + Math.max(3, Math.min((vx1 - vx0) * f, (vx1 - vx0) - 3)));
        ys.push(vy0 + Math.max(3, Math.min((vy1 - vy0) * f, (vy1 - vy0) - 3)));
      }
      let at = null;
      for (const y of ys) { for (const x of xs) { if (canvasAt(x, y) && alone(x, y, p)) { at = { x, y }; break; } } if (at) break; }
      if (!at) continue;
      q.press = { x: at.x, y: at.y,
        offCentre: (vx1 - vx0 > 10 && Math.abs(at.x - q.cx) > 1) || (vy1 - vy0 > 10 && Math.abs(at.y - q.cy) > 1) };
    }
    // Nearest the live edge — the topmost box, as before.
    if (!best || q.y < best.y) best = q;
  }
  return best;
})()`;

/** [[pickBox]], as a value in the node process. `null` = no detection is pressable right now. */
const boxToPress = async (page, opts = {}) =>
  JSON.parse(await page.eval(`JSON.stringify(${pickBox(opts)})`));

/** A point on the canvas that is BARE MAP: no pin/box hit area, no floating chrome, no card, not the
 * minimap strip — and the browser's own hit test agrees the canvas is what is there. */
const BARE_AT = `(() => {
  const c = document.querySelector('.sf-canvas').getBoundingClientRect();
  const dpr = window.devicePixelRatio || 1;
  const boxes = [...document.querySelectorAll('.sf-pins .sf-pin, .map-ctl > *, .sheet, .sf-readout, .sf-chrome, .sf-note, .sf-status-line, .sf-scale, .sf-maptip, .research:not([hidden])')]
    .map((e) => e.getBoundingClientRect()).filter((r) => r.width > 0 && r.height > 0);
  const free = (x, y) => !boxes.some((r) => x >= r.x - 8 && x <= r.right + 8 && y >= r.y - 8 && y <= r.bottom + 8);
  const y1 = c.bottom - ${MINIMAP_PX} / dpr - 8;
  for (let fy = 0.75; fy > 0.1; fy -= 0.05) {
    for (let fx = 0.5; fx < 0.98; fx += 0.04) {
      const x = c.x + c.width * fx, y = c.y + (y1 - c.y) * fy;
      if (!free(x, y)) continue;
      const top = document.elementFromPoint(x, y);
      if (top && top.classList.contains('sf-canvas')) return { x, y };
    }
  }
  return null;
})()`;

/**
 * **Every time the card BECAME hidden since `watchCard`** — so a "swap" that secretly closed and
 * re-opened the card is caught (the user asked for it to CHANGE, not blink).
 *
 * Read from each record's `oldValue`, one entry per record, not from the element's state when the
 * observer's callback runs (T-1032). `MutationObserver` delivers a microtask after the mutations, so a
 * close-and-re-open done in ONE task — `sheet.hide(); sheet.show()` in a selection handler, which is
 * exactly the shape this regression would take — arrives as two records in one callback, and a
 * callback that reads `el.hidden` then sees only the final state: not hidden, nothing recorded, guard
 * silently vacuous (measured: the defect re-injected, and the spec stayed green). With
 * `attributeOldValue`, `hidden` absent reads back as `null`, so `oldValue === null` IS the
 * absent-to-present transition, per record, whatever the callback happens to be delivered.
 */
const watchCard = (page) => page.eval(`(() => {
  const el = document.querySelector('.sheet');
  window.__cardHidden = [];
  new MutationObserver((recs) => { for (const r of recs) window.__cardHidden.push(r.oldValue === null); })
    .observe(el, { attributes: true, attributeFilter: ['hidden'], attributeOldValue: true });
  return true;
})()`);
const cardHiddenLog = (page) => page.eval("JSON.stringify(window.__cardHidden ?? [])").then(JSON.parse);

/** For a failure message. */
const STATE = `JSON.stringify({
  pins: [...document.querySelectorAll('.sf-pins .sf-pin')].map((p) => [p.className, p.dataset.pin?.slice(0, 8)]),
  pills: document.querySelector('.map-inv')?.textContent ?? null,
  rows: [...document.querySelectorAll('.side-inv .row[data-id]')].map((r) => r.dataset.id.slice(0, 8)),
  chrome: document.querySelector('.sf-chrome')?.textContent?.slice(0, 200) ?? null,
  where: document.querySelector('.sf-where')?.textContent?.slice(0, 200) ?? null, // T-996's one-line readout
  note: document.querySelector('.sf-note')?.textContent?.slice(0, 200) ?? null,
})`;

const mhz = (s) => (s ?? "").match(/[\d.]+/)?.[0] ?? null;

/**
 * **The card's own answer to "which feature is this about", read in ONE evaluation with the box's**
 * (T-1032). The selected pin carries `aria-pressed="true"` (`surface/pins.ts`), so the identity of
 * what the card is showing is an id, not a formatted number — and the frequency strings the user
 * compares (the box's accessible name and the card's big frequency) are read in the same evaluation
 * as each other.
 *
 * That last part is the whole of the `99.7754 vs 99.7755` red: both are `toFixed(4)` of ONE centre
 * (`pins.ts`'s `fmtMHz`, `explore/format.ts`'s `fmtMHz`), so a disagreement is never a rounding
 * difference — it is two reads of a centre that closed-loop refinement moved by 100 Hz in between
 * (the product's own "tune from the processed output" rule). Read together, and polled until the two
 * renderings of one row agree, the assertion is about the card naming the box it was opened from; it
 * still goes red for a card that names a DIFFERENT feature, which no waiting can fix.
 */
const AGREE = `JSON.stringify((() => {
  const el = document.querySelector('.sheet');
  const p = document.querySelector('.sf-pins .sf-pin.detection[aria-pressed="true"]');
  return { hidden: !el || !!el.hidden, id: p?.dataset.pin ?? null, label: p?.getAttribute('aria-label') ?? null,
    title: document.querySelector('.sheet-title')?.textContent ?? null,
    bigf: document.querySelector('.focus .detail .bigf')?.textContent ?? null };
})())`;
const agree = async (page) => JSON.parse(await page.eval(AGREE));

/**
 * **Press a detection's box until the card is open ON THAT BOX, re-choosing the box each attempt.**
 *
 * Candidates churn by design — "detection is fast, continuous and self-cleaning", confidence decays
 * and a quiet region's candidates expire — so a box chosen one round trip ago may not be there when
 * the press lands, and the press then goes to bare map (which closes the card: the very gesture step
 * 5 asserts). The loop therefore re-picks a pressable box each time and is satisfied only by the
 * card being open with that box's own id selected; the deadline bounds a hang, nothing else.
 *
 * **How long that deadline has to be was measured, not guessed.** A probe of this fixture's own
 * replay sampled the drawn features every 5 s for 3 minutes: 3 confirmed rows the whole time, but the
 * pins they draw came and went — present 3–4 at a time, absent for 5 s, 10 s and once **35 s** in a
 * row while the inventory still held all three. So "no feature is pressable right now" is a normal
 * state of this product, and the bound below is a multiple of the longest outage measured rather than
 * a number chosen to make a red go away. The steps that can do without a detection say so (`soft`)
 * and fall back to a marked region.
 */
async function openOnBox(page, t, { exclude = null, only = null, soft = false, timeoutMs = 45000 } = {}) {
  const t0 = Date.now();
  let tried = 0, lastBox = null, lastSeen = null;
  while (Date.now() - t0 < timeoutMs) {
    const box = await boxToPress(page, { exclude, only });
    if (!box) { await page.frames(2); continue; }   // nothing pressable this frame
    lastBox = box;
    tried++;
    await page.mouse("mouseMoved", box.press.x, box.press.y);
    await page.mouse("mousePressed", box.press.x, box.press.y, { buttons: 1, clickCount: 1 });
    await page.mouse("mouseReleased", box.press.x, box.press.y, { buttons: 0, clickCount: 1 });
    // The sample that proves it is the one this returns: `id`, the pin's own label and the card's big
    // frequency, read TOGETHER, so the caller can assert on two renderings of one row that existed at
    // one instant (a row whose pin is gone a poll later cannot be re-read — see `agreesWithBox`).
    const r = await page.waitForValue(`the card to open on ${box.label}`, AGREE,
      (s) => { const v = JSON.parse(s); return !v.hidden && v.id === box.id; }, { timeoutMs: 2500, everyMs: 120 });
    lastSeen = JSON.parse(r.value);
    if (r.ok) {
      t.diagnostic(`press ${tried} at ${JSON.stringify(box.press)} opened the card on ${box.label}`);
      return { ...box, sample: lastSeen };
    }
  }
  if (only || soft) return null;   // the caller asked for ONE feature, or will handle "none"
  throw new Error(`pressing inside a detection box never opened the card on it after ${tried} presses ` +
    `(last box ${JSON.stringify(lastBox)}, last card ${JSON.stringify(lastSeen)}); state: ${await page.eval(STATE)}`);
}

/**
 * **Do the card and the box state one centre?** — over every sample in which BOTH were rendered.
 *
 * A feature's pin is not guaranteed to stay in the DOM while the card is open: detections churn, and
 * a row the backend re-analyses (an overlap it resolves, an interval it revokes) loses its pin while
 * the card goes on showing the selection. So this polls for agreement but judges only the samples
 * where the pin and the card were both there, starting with the one `openOnBox` returned — never
 * requiring the pin to survive a wait. One agreeing sample is the claim; all co-present samples
 * disagreeing is the defect, and is reported with both numbers.
 */
async function agreesWithBox(page, box, { timeoutMs = 5000 } = {}) {
  const both = [];
  const take = (v) => { if (v.id === box.id && v.label !== null && v.bigf !== null) both.push(v); };
  take(box.sample);
  const r = await page.waitForValue("the card and the box it was opened from to state one centre", AGREE,
    (s) => { const v = JSON.parse(s); take(v); return v.id === box.id && v.label !== null && mhz(v.bigf) === mhz(v.label); },
    { timeoutMs, everyMs: 150 });
  return { ok: both.some((v) => mhz(v.bigf) === mhz(v.label)), both, polls: r.polls };
}

/** Mark a region with the one gesture that never changes meaning (shift+drag, docs/23 §10.4): a
 * feature with its own box, and the only other feature available when the window holds a single
 * pressable detection. */
async function markRegion(page) {
  const r = await page.$rect(".sf-canvas");
  const dpr = await page.eval("window.devicePixelRatio || 1");
  const bottom = r.y + r.h - MINIMAP_PX / dpr - 40;
  // The drag starts on the MAP: at phone width T-996's left column (Retune + widths, the offer)
  // spans the whole width a quarter of the way down, so the start moves below it — the first point
  // the canvas is what a press lands on.
  const x0 = r.x + r.w * 0.2;
  const y0 = await page.eval(`(() => { for (let y = ${r.y + r.h * 0.25}; y < ${bottom - 60}; y += 4)
    if (document.elementFromPoint(${x0}, y)?.classList.contains('sf-canvas')) return y; return ${r.y + r.h * 0.25}; })()`);
  await page.drag({ x: x0, y: y0 }, { x: r.x + r.w * 0.34, y: bottom }, 10, { shift: true });
  await page.waitFor("the card to open on the selected region",
    `/Selected region/.test(document.querySelector('.sheet-title')?.textContent ?? '')`, { timeoutMs: 20000 });
}

/**
 * **Press a control, having first checked the browser agrees the control is what is at that point.**
 *
 * `page.click` reads a rect in one evaluation and presses in the next, and the card is an element
 * whose height the product re-applies whenever the map's chrome is re-measured (`sheet.ts`'s
 * `relayout`, called when the minimap re-fits) — so under load its head, and the × in it, can move
 * between the two. The press then lands in the body below the button and NOTHING happens, which is
 * the `5 s wait for the card's x` red: not the close handler being lost, but the press missing it.
 * `app-chrome.mjs`'s `arrived` is the same fix for the animated case and is used first here.
 *
 * So: settle, then measure and hit-test the point in ONE evaluation, press, and require the effect
 * the control has. A press that misses is re-measured and repeated; a control that takes the press
 * and does nothing runs out of attempts and fails, so a genuinely lost handler is still red.
 */
async function pressControl(page, t, sel, what, doneExpr, { attempts = 4 } = {}) {
  let last = null;
  for (let i = 0; i < attempts; i++) {
    await settled(page, ".sheet", `the card before pressing ${what}`, { timeoutMs: 8000 }).catch(() => {});
    const at = JSON.parse(await page.eval(`JSON.stringify((() => {
      const e = document.querySelector(${JSON.stringify(sel)});
      if (!e || e.hidden) return null;
      const r = e.getBoundingClientRect();
      if (r.width <= 0 || r.height <= 0) return null;
      const x = r.x + r.width / 2, y = r.y + r.height / 2;
      const top = document.elementFromPoint(x, y);
      return { x, y, on: !!top && (top === e || e.contains(top)), over: top ? (top.className || top.tagName) : null };
    })())`));
    last = at;
    if (!at || !at.on) { t.diagnostic(`${what}: ${at ? `${at.over} is at the control's centre` : "the control is not on screen"}; re-measuring`); await page.frames(2); continue; }
    await page.mouse("mouseMoved", at.x, at.y);
    await page.mouse("mousePressed", at.x, at.y, { buttons: 1, clickCount: 1 });
    await page.mouse("mouseReleased", at.x, at.y, { buttons: 0, clickCount: 1 });
    const r = await page.waitForValue(what, `!!(${doneExpr})`, (v) => v === true, { timeoutMs: 3000, everyMs: 100 });
    if (r.ok) return at;
    t.diagnostic(`${what}: the press at ${JSON.stringify(at)} had no effect yet; re-measuring and pressing again`);
  }
  throw new Error(`${what}: ${attempts} presses of ${sel} (last ${JSON.stringify(last)}) never had their effect ` +
    `— \`${doneExpr}\` stayed false; card ${JSON.stringify(await agree(page))}`);
}

for (const [width, height] of [[1280, 800], [400, 800]]) {
  test(`at ${width}x${height} the detail card is hidden until a feature is clicked, and closes on bare map`, async (t) => {
    const browser = await Browser.open();
    t.after(() => browser.close());
    const page = await browser.page(undefined, { width, height });
    const shot = async (name) => { if (SHOTS) await page.shot(path.join(SHOTS, `card-${width}-${name}.png`)); };
    const openApp = async () => {
      // The load event is DIAGNOSED, not asserted on: `Page.goto` reports "timeout" when the browser
      // has not fired `load` within 30 s, which on a box at load 42 it did not — a statement about the
      // machine, not about the product (docs/10 §3.6). What the page must do is MOUNT ITS SURFACE, and
      // that is asserted below on the page's own event; it cannot be satisfied by a page that failed
      // to load, so nothing is given up by taking the stronger claim as the one to fail on.
      const load = await page.goto(`${ORIGIN}/#token=${TOKEN}`);
      if (load !== "load") t.diagnostic(`the browser did not report \`load\` within its bound (${load}); waiting for the surface's own event`);
      // T-907: the surface's own mounted/failed event, before any other wait.
      await page.waitForSurfaceMounted({ timeoutMs: 60000 });
      await page.waitFor("the canvas, the card's host and the inventory pills",
        `!!document.querySelector('.sf-canvas') && document.querySelector('.sf-canvas').width > 200 &&
         !!document.querySelector('.sheet') && !!document.querySelector('.map-inv .map-pill')`, { timeoutMs: 60000 });
    };
    await openApp();

    // ---- (1) the default paint has no card ----
    const first = await card(page);
    t.diagnostic(`default: ${JSON.stringify(first)}`);
    assert.equal(first.hidden, true, "the map opened with the detail card already on it");
    assert.equal(first.open, "false");
    assert.equal(first.h, 0, "a hidden card still takes pixels — `.sheet[hidden]` is not `display: none`");
    // Nothing of it is on the bottom edge: the point where its strip used to sit is the map.
    const edge = await page.eval(`(() => { const c = document.querySelector('.sf-canvas').getBoundingClientRect();
      const top = document.elementFromPoint(c.x + c.width - 100, c.bottom - 80);
      return top ? (top.className || top.tagName) : 'nothing'; })()`);
    assert.doesNotMatch(String(edge), /sheet/, `the card still covers the bottom edge (${edge})`);
    await shot("1-no-card");

    // ---- (2) clicking a detection box opens the card on that box ----
    // A pressable box: one with a point inside it that is on the canvas and clear of the minimap —
    // NOT one whose whole rectangle is inside the canvas, which an ongoing signal's box never is
    // (it grows to the live edge; see `pickBox`).
    //
    // The lane's readiness page already waits for observed coverage before any spec starts, but a
    // page that bootstrapped while its server was still warming opens on the WHOLE 1 MHz – 6 GHz
    // surface — `surface/preview.ts`'s documented fallback for a `/api/coverage` answer that reports
    // nothing observed ("the view opens on the whole surface, because nothing said where the radio
    // looked"). On that view every feature of a 2.4 MHz capture is a 1 px generalized symbol against
    // the left edge, under the map's own left column: there is nothing to press, by construction and
    // for ever. Measured at load 77: eight detection pins, every one of them unpressable, and 120 s
    // of waiting for a box that would never arrive.
    //
    // So the wait is the harness's own answer to the same problem (`waitForSurfaceHistory` reloads
    // `/surface.html` until the census reports observed coverage): RELOAD until the page opens on the
    // observed extent — never re-navigate the view, since the window a page opens on is a product
    // claim of its own (the T-376 rule) and not this spec's to set.
    //
    // The box AND the hit test that says a press there is a map click are read in ONE evaluation, and
    // asserted on THAT sample: a box that was pressable when the wait returned has scrolled, grown or
    // been re-analysed away by the time of a second read ("a wait answers about one frame; the read
    // after it is a different frame" — `e2e/README.md`), which is two of the reds this ticket is for.
    const PICKED = `JSON.stringify((() => { const q = ${pickBox()}; if (!q) return null;
      return { ...q, canvasAtPress: document.elementFromPoint(q.press.x, q.press.y)?.classList.contains('sf-canvas') === true }; })())`;
    let box = null;
    for (let load = 0; load < 2 && !box; load++) {
      const r = await page.waitForValue("blind detection to draw a pressable box on the canvas",
        PICKED, (v) => JSON.parse(v) !== null, { timeoutMs: 30000, everyMs: 1000 });
      if (!r.ok) {
        t.diagnostic(`no pressable feature after ${r.ms} ms (${r.polls} polls); state: ${await page.eval(STATE)}`);
        t.diagnostic("reloading: a page that bootstrapped before its server had coverage opened on the whole surface");
        await openApp();
        continue;
      }
      const picked = JSON.parse(r.value);
      // The canvas is what a click at that point lands on (the hit areas do not take the gesture —
      // T-809), so the press goes through the surface's own polygon hit test rather than a hit area's
      // own handler. Asserted on the SAME sample the point came from.
      assert.equal(picked.canvasAtPress, true,
        `the canvas is not under ${JSON.stringify(picked.press)} inside ${picked.label}, so this press would not be a map hit test`);
      box = await openOnBox(page, t, { soft: true, timeoutMs: 30000 });
      if (!box) t.diagnostic("every feature drawn went away before the card opened on one; waiting for the next");
    }
    if (!box) {
      throw new Error("no detection box could be pressed into opening the card, over two loads of the page; " +
        `state: ${await page.eval(STATE)}`);
    }
    t.diagnostic(`box: ${JSON.stringify(box)}`);
    await page.waitFor("the card to name the selected signal and show its detail",
      `/^Selected signal/.test(document.querySelector('.sheet-title')?.textContent ?? '') &&
       !!document.querySelector('.focus .detail .bigf')`, { timeoutMs: 15000 });
    await settled(page, ".sheet", "the card's opening");
    const shown = await card(page);
    t.diagnostic(`card: ${JSON.stringify(shown)}`);
    assert.equal(shown.snap, "half", "the card opens at half, so what was clicked is visible");
    assert.ok(shown.h > 100, `the card is on screen (${shown.h} px tall)`);
    // The card is about the box that was clicked — the box's own id is the pressed pin (`openOnBox`)
    // — and the two state ONE centre frequency, read together and polled until they agree: the card
    // and the box are two renderings of one row, and refinement moves that row's centre while the
    // test runs (see [[AGREE]]).
    assert.equal(box.sample.id, box.id, `the card is about ${box.sample.label}, not the box clicked (${box.label})`);
    const same = await agreesWithBox(page, box);
    assert.ok(same.ok, `the card and the box it was opened from never stated one centre frequency — ` +
      `${same.both.length} sample(s) with both on screen, the first four: ` +
      JSON.stringify(same.both.slice(0, 4).map((v) => [v.bigf, v.label])));
    if (box.press.offCentre) t.diagnostic("the press was inside the box but off its centre: a polygon hit test");
    await shot("2-card");

    // ---- (3) another feature swaps the card's content, without closing it ----
    // The press has to land on a feature that is STILL THERE: a candidate that expired between the
    // pick and the press leaves bare map under the pointer, which closes the card — a missed press,
    // not a failed swap. The two are told apart by what the card does: closed and stayed closed is a
    // miss (re-open and press another feature); closed and re-opened is the defect this step exists
    // for, and the `hidden` log below says which happened.
    let swapped = null, misses = 0;
    for (let attempt = 0; attempt < 3 && !swapped; attempt++) {
      let cur = await agree(page);
      if (cur.hidden || cur.id === null) {
        // A missed press closed the card: re-open it on whatever is drawn now. If nothing is drawn,
        // this attempt has nothing to swap FROM, so it waits for the next one rather than pretending
        // a region mark was a swap.
        const re = await openOnBox(page, t, { soft: true, timeoutMs: 30000 });
        if (!re) { t.diagnostic("no feature is drawn to re-open the card from; waiting for the next"); continue; }
        box = re;
        cur = await agree(page);
      }
      await settled(page, ".sheet", "the card before the swap");
      await watchCard(page);
      const other = await boxToPress(page, { exclude: cur.id });
      if (!other) {
        // One pressable detection in this window: the other feature is a marked region.
        await markRegion(page);
        swapped = "a marked region (this window held one pressable detection)";
        break;
      }
      await page.mouse("mouseMoved", other.press.x, other.press.y);
      await page.mouse("mousePressed", other.press.x, other.press.y, { buttons: 1, clickCount: 1 });
      await page.mouse("mouseReleased", other.press.x, other.press.y, { buttons: 0, clickCount: 1 });
      // Only the POSITIVE outcome ends the attempt — the card open on the other feature. A card that
      // closed and re-opened on it satisfies this too, and is then failed by the `hidden` log below,
      // which is the defect this step is for; accepting "closed" as an outcome here would have let
      // that defect read as a missed press and be retried away.
      const r = await page.waitForValue(`the card to swap to ${other.label}`, AGREE,
        (v) => { const x = JSON.parse(v); return !x.hidden && x.id === other.id; }, { timeoutMs: 8000, everyMs: 120 });
      if (r.ok) { swapped = other.label; break; }
      misses++;
      t.diagnostic(`the second feature was not under the press when it landed (card ${r.value}): ` +
        `re-choosing a feature that is still drawn`);
    }
    assert.ok(swapped, `no second feature could be pressed while it was still drawn (${misses} misses)`);
    t.diagnostic(`swapped to ${JSON.stringify(swapped)}`);
    assert.deepEqual((await cardHiddenLog(page)).filter((h) => h === true), [],
      "clicking another feature CLOSED the card and re-opened it, instead of changing its content");
    await shot("3-swapped");

    // ---- (4) Escape closes it ----
    await page.key("Escape");
    await page.waitFor("Escape to close the card", `document.querySelector('.sheet').hidden === true`, { timeoutMs: 5000 });
    const afterEsc = await card(page);
    assert.equal(afterEsc.h, 0, "Escape left some of the card on screen");
    await shot("4-escape");

    // ---- (5) a click on bare map closes it ----
    // The SAME feature re-opens it (closing cleared the selection), when that feature is still drawn:
    // a candidate that expired while step 4 ran is not evidence about re-opening, so the fallback is
    // another feature and the diagnostic says which happened.
    const again = await openOnBox(page, t, { only: box.id, timeoutMs: 15000 });
    if (again) { t.diagnostic(`the same feature re-opened the card (${again.label})`); box = again; }
    else {
      t.diagnostic(`the feature just closed from (${box.label}) is no longer drawn — re-opening on another`);
      const other = await openOnBox(page, t, { soft: true, timeoutMs: 20000 });
      // No detection is pressable at all: a marked region opens the card instead. What this step is
      // about is the DISMISSAL — a click on the back of the map closes the card — and that is the same
      // claim whichever feature opened it. (Measured: a window whose only detections had just been
      // re-analysed drew no boxes at all for over a minute.)
      if (other) box = other;
      else { t.diagnostic("no detection is pressable — opening the card on a marked region instead"); await markRegion(page); }
    }
    // T-958's rule, and the reason this failed at 400 px first time round: a point is only bare map
    // once the card has ARRIVED. Mid-rise its box is still most of the way down the screen, so a
    // point chosen then is canvas at that instant and under the card by the time the press lands.
    //
    // And the boxes SCROLL with the time axis, so a point that was bare map one round trip ago can
    // have a feature over it when the press lands — which would select, not dismiss. So the point is
    // re-chosen and re-pressed, exactly as [[pressControl]] does for a control: a card that refuses
    // to close on a real bare-map click still runs out of attempts and fails.
    let closed = false, bare = null;
    const pressed = [];
    for (let i = 0; i < 4 && !closed; i++) {
      await settled(page, ".sheet", "the card before the bare-map press");
      bare = JSON.parse(await page.eval(`JSON.stringify(${BARE_AT})`));   // hit-tested in that same evaluation
      if (!bare) { t.diagnostic("no point of bare map on the canvas this frame"); await page.frames(2); continue; }
      pressed.push(bare);
      await page.mouse("mouseMoved", bare.x, bare.y);
      await page.mouse("mousePressed", bare.x, bare.y, { buttons: 1, clickCount: 1 });
      await page.mouse("mouseReleased", bare.x, bare.y, { buttons: 0, clickCount: 1 });
      const r = await page.waitForValue("the click on bare map to close the card",
        "document.querySelector('.sheet').hidden === true", (v) => v === true, { timeoutMs: 3000, everyMs: 100 });
      closed = r.ok;
      if (!closed) t.diagnostic(`the press at ${JSON.stringify(bare)} did not close the card (${JSON.stringify(await agree(page))})`);
    }
    assert.ok(bare, "no point of bare map on the canvas to click");
    t.diagnostic(`bare map at ${JSON.stringify(pressed)}`);
    assert.ok(closed, `a click on the back of the map did not close the card (pressed ${JSON.stringify(pressed)}, ` +
      `card ${JSON.stringify(await agree(page))})`);
    assert.equal((await card(page)).h, 0, "a click on the back of the map left some of the card on screen");
    // ...and it gave the columns back: the card is no longer what is at the point it covered.
    assert.equal(await page.eval(`(() => { const top = document.elementFromPoint(${bare.x}, ${bare.y});
      return !!top && !document.querySelector('.sheet').contains(top); })()`), true,
      "the closed card is still the element at the point it covered");
    await shot("5-bare-map");

    // ---- (6) an inventory pill opens the card on its list ----
    for (const list of ["candidate", "confirmed"]) {
      await pressControl(page, t, `.map-inv .map-pill[data-list="${list}"]`, `the ${list} pill to open the card on its list`,
        `document.querySelector('.sheet').hidden === false &&
         document.querySelector('.side-inv .tab[data-tab="${list}"]')?.getAttribute('aria-selected') === 'true'`);
      await settled(page, ".sheet", `the card's rise for ${list}`);
      const c = await card(page);
      assert.equal(c.tab, list);
      assert.match(c.title ?? "", list === "candidate" ? /^Candidate signals/ : /^Confirmed signals/,
        `the card names the list it opened on (${c.title})`);
      // The list is inside the card's own box, not somewhere behind it.
      const inside = JSON.parse(await page.eval(`JSON.stringify((() => {
        const s = document.querySelector('.sheet').getBoundingClientRect();
        const inv = document.querySelector('.sheet .side-inv')?.getBoundingClientRect();
        return inv ? { top: Math.round(inv.top), sheetTop: Math.round(s.top), sheetBottom: Math.round(s.bottom) } : null; })())`));
      assert.ok(inside && inside.top >= inside.sheetTop && inside.top < inside.sheetBottom,
        `the ${list} list is not inside the card (${JSON.stringify(inside)})`);
    }
    await shot("6-list");
    // Its × closes it, from the list state as much as from a selection.
    await pressControl(page, t, ".sheet .sheet-close", "the × to close the card",
      "document.querySelector('.sheet').hidden === true");

    assert.deepEqual(page.requests.filter((r) => CONTROL.test(r.url)).map((r) => r.url), [],
      "opening, swapping or closing the card reached a device route");
    assert.deepEqual(page.exceptions, [], "uncaught exception");
  });
}
