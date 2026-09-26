// T-1002 (MMAP split view): **detections queried per pane**, in the real app, over the product's
// own server, at a desktop width and a phone width.
//
// The user's case: "look at one signal from the past and the current waterfall". Panes have their
// own centre and span in both axes, but the inventory did not: one window (`state.live.view` +
// `state.time`, the mirror of whichever pane was last touched) built the Candidate/Confirmed
// queries and every pane drew the same rows. Freezing pane 1 on a past signal therefore re-scoped
// pane 2's live boxes to pane 1's past window.
//
// What this proves in a browser, which the unit tier cannot:
//   1. with a split open, ONE poll asks `/api/inventory` about TWO windows — pane 1's frozen past
//      and pane 2's live edge — and keeps doing so as the live edge advances;
//   2. pane 2's own window (its status row's `t0`/`t1`, written per frame) goes on advancing while
//      pane 1 sits in the past, and nothing about pane 1 pulls it back;
//   3. the lists NAME the pane they are showing, and follow the pane that was pressed;
//   4. none of it reaches a device route — choosing and scrubbing a pane is view state.
import test from "node:test";
import assert from "node:assert/strict";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { Browser } from "./harness.mjs";
import { paneAct } from "./app-chrome.mjs";

const ORIGIN = process.env.HK_E2E_ORIGIN, TOKEN = process.env.HK_E2E_TOKEN;
const CONTROL = /\/api\/control\/(center|rate|window|gains|bias_tee|baseband_filter)/;
const ART = path.join(path.dirname(fileURLToPath(import.meta.url)), "artifacts");

/** What the Explore lists say about which pane they are of. */
const LISTS = `JSON.stringify((() => {
  const h = document.querySelector('.side-inv .h');
  const q = (s) => document.querySelector(s);
  return {
    heading: h.textContent.replace(/\\s+/g, ' ').trim(),
    pane: h.dataset.pane ?? null,
    confirmed: q('.side-inv .tab[data-tab="confirmed"]').getAttribute('aria-label'),
    candidate: q('.side-inv .tab[data-tab="candidate"]').getAttribute('aria-label'),
  };
})())`;

/** Each pane's own window, off its own status row (`data-t0-ns`/`data-t1-ns`, written per FRAME
 * for EVERY pane — not the active one's readout). */
const WINDOWS = `JSON.stringify(Object.fromEntries([...document.querySelectorAll('.hk-surface-viewport[data-viewport="pane"]')]
  .map((r) => [r.querySelector('.hk-surface-id').textContent,
    { t0: Number(r.dataset.t0Ns), t1: Number(r.dataset.t1Ns), following: r.dataset.following === 'true' }])))`;

/** A point inside the canvas, in page px, within [x0, x1] and away from the floating chrome. */
const clearPoint = (x0, x1) => `JSON.stringify((() => {
  const c = document.querySelector('.sf-canvas'), r = c.getBoundingClientRect();
  for (const fy of [0.45, 0.35, 0.55, 0.3, 0.6, 0.25, 0.65]) for (const fx of [0.5, 0.35, 0.65, 0.25, 0.75]) {
    const x = ${x0} + (${x1} - ${x0}) * fx, y = r.y + r.height * fy;
    if (document.elementFromPoint(x, y) === c) return { x, y };
  }
  return null; })())`;

/** The `t1` (and `t0`) of every `/api/inventory?...state=candidate` read the page has made. */
const candidateWindows = (page, since = 0) => page.requests
  .filter((r) => r.url.includes("/api/inventory?") && r.url.includes("state=candidate") && r.startedMs >= since)
  .map((r) => {
    const q = new URLSearchParams(r.url.slice(r.url.indexOf("?") + 1));
    return { t0: Number(q.get("t0")), t1: Number(q.get("t1")), at: r.startedMs };
  })
  .filter((w) => Number.isFinite(w.t1));

for (const [width, height] of [[1280, 800], [400, 820]]) test(`at ${width} px: each pane's detections are queried for its own window, and the lists name the pane`, async (t) => {
  const browser = await Browser.open();
  t.after(() => browser.close());
  const page = await browser.page(undefined, { width, height });
  assert.equal(await page.goto(`${ORIGIN}/#token=${TOKEN}`), "load");
  await page.waitForSurfaceMounted({ timeoutMs: 60000 });
  await page.waitFor("the floating cluster to mount", "!!document.querySelector('.map-pane-btn')", { timeoutMs: 30000 });
  await page.waitFor("a pane row with its own window stated",
    "Number(document.querySelector('.hk-surface-viewport[data-viewport=\"pane\"]')?.dataset.t1Ns) > 0", { timeoutMs: 90000 });
  await page.waitFor("the Explore lists to mount",
    "!!document.querySelector('.side-inv .tab[data-tab=\"candidate\"]')", { timeoutMs: 30000 });

  // One pane: nothing to disambiguate, so the lists say nothing extra (docs/23 §10.6 P1).
  const one = JSON.parse(await page.eval(LISTS));
  t.diagnostic(`one pane: ${JSON.stringify(one)}`);
  assert.equal(one.pane, null, "the lists name a pane when there is only one to be looking at");
  assert.deepEqual({ c: one.confirmed, k: one.candidate }, { c: "Confirmed", k: "Candidates" });
  assert.doesNotMatch(one.heading, /pane/, `the heading names a pane with one pane open: ${one.heading}`);
  assert.equal(new Set(candidateWindows(page).map((w) => w.t1)).size <= 2, true,
    "one pane asked about more than two distinct windows before anything was split");

  // Split. The new (right-hand) pane is active, and the lists say so — both the heading and the
  // tab labels, the same naming the canvas's outline uses (T-1000).
  await paneAct(page, "split");
  await page.waitFor("the lists to name the new pane",
    "document.querySelector('.side-inv .h').dataset.pane === '2'", { timeoutMs: 15000 });
  const two = JSON.parse(await page.eval(LISTS));
  t.diagnostic(`split: ${JSON.stringify(two)}`);
  assert.deepEqual({ c: two.confirmed, k: two.candidate },
    { c: "Confirmed · pane 2 of 2", k: "Candidates · pane 2 of 2" }, "the tabs do not name the pane they list");
  assert.match(two.heading, /pane 2 of 2/, `the heading does not name the pane: ${two.heading}`);

  // Freeze pane 1 in the past: press it (it becomes active — and the lists follow), then `L` to
  // stop it following, then drag it back in time. Pane 2 keeps following the live edge.
  const canvas = await page.$rect(".sf-canvas");
  const left = JSON.parse(await page.eval(clearPoint(canvas.x + 4, canvas.x + canvas.w * 0.45)));
  assert.ok(left, "no clear point on pane 1 to press");
  await page.mouse("mousePressed", left.x, left.y, { buttons: 1, clickCount: 1 });
  await page.mouse("mouseReleased", left.x, left.y, { buttons: 0, clickCount: 1 });
  await page.waitFor("the lists to follow the pane that was pressed",
    "document.querySelector('.side-inv .h').dataset.pane === '1'", { timeoutMs: 15000 });
  const onOne = JSON.parse(await page.eval(LISTS));
  assert.deepEqual({ c: onOne.confirmed, k: onOne.candidate },
    { c: "Confirmed · pane 1 of 2", k: "Candidates · pane 1 of 2" }, "the lists did not follow the press");
  await page.key("Escape");
  // Make room on the time axis first, and say why: a following pane opens on the whole observed
  // extent, so `PaneModel.normalise` clamps its centre and a backward pan is a no-op (surface-nav's
  // premise). Alt+wheel zooms time alone and a zoom is not a pause.
  for (let i = 0; i < 3; i++) await page.wheel(left, -240, { alt: true });
  await page.frames(3);
  await page.key("l", { code: "KeyL", keyCode: 76 });
  const ids = Object.keys(JSON.parse(await page.eval(WINDOWS)));
  const activeId = await page.eval("document.querySelector('.sf-active-pane').dataset.paneId");
  const otherId = ids.find((i) => i !== activeId);
  assert.ok(activeId && otherId, `two pane rows expected: ${JSON.stringify(ids)}`);
  await page.waitFor("pane 1 to stop following",
    `(${WINDOWS.replace(/^JSON\.stringify/, "")})[${JSON.stringify(activeId)}].following === false`, { timeoutMs: 15000 });
  // …and back in time, so its window is unmistakably not the live one. A pan is view arithmetic in
  // time: it never commands the radio (asserted at the end).
  // UP the time axis is back in time (surface-nav: a 160 px scrub pauses, a drag down toward the
  // edge re-follows). Well beyond the snap-back zone, so pane 1 stays where it was put.
  await page.drag({ x: left.x, y: left.y }, { x: left.x, y: left.y - 160 }, 12);
  await page.frames(4);

  const split = JSON.parse(await page.eval(WINDOWS));
  t.diagnostic(`windows after freezing pane 1: ${JSON.stringify(split)}`);
  assert.equal(split[otherId].following, true, "pane 2 stopped following when pane 1 was frozen");
  assert.ok(split[activeId].t1 < split[otherId].t1, `pane 1 is not behind pane 2: ${JSON.stringify(split)}`);

  // THE ACCEPTANCE: from here on, one poll asks about TWO windows — pane 1's frozen past and pane
  // 2's live edge — and pane 2's own window goes on advancing while pane 1 sits still.
  const mark = Date.now();
  // Node-side, because `page.requests` is recorded here and not in the page: wait for two more
  // polls' worth of candidate reads to arrive.
  const deadline = Date.now() + 40000;
  for (;;) {
    const w = candidateWindows(page, mark);
    if (w.length >= 4 && new Set(w.map((x) => x.t1)).size >= 2) break;
    assert.ok(Date.now() < deadline,
      `after 40 s the page had asked about ${JSON.stringify(candidateWindows(page, mark))} — two panes must be two windows`);
    await new Promise((r) => setTimeout(r, 500));
  }
  const asked = candidateWindows(page, mark);
  const ends = [...new Set(asked.map((w) => w.t1))].sort((a, b) => a - b);
  t.diagnostic(`candidate windows asked about: ${JSON.stringify(ends)}`);
  assert.ok(ends.length >= 2, `one window was asked about for two panes: ${JSON.stringify(ends)}`);
  // Pane 1's window is asked about repeatedly at the SAME instant (it is frozen); pane 2's moves.
  const counts = new Map();
  for (const w of asked) counts.set(w.t1, (counts.get(w.t1) ?? 0) + 1);
  const frozen = [...counts.entries()].filter(([, n]) => n >= 2).map(([t1]) => t1);
  assert.ok(frozen.length >= 1, `no frozen pane's window was re-asked: ${JSON.stringify([...counts])}`);
  assert.ok(Math.max(...ends) > Math.min(...frozen), "the live pane's window never advanced past the frozen one's");

  const after = JSON.parse(await page.eval(WINDOWS));
  assert.equal(after[activeId].t1, split[activeId].t1, "the frozen pane's window moved on its own");
  assert.ok(after[otherId].t1 >= split[otherId].t1, "the live pane's window stopped advancing");

  // And the naming is on SCREEN, not only in the DOM: the candidate pill raises the sheet on that
  // list, inside the sheet's own box, with the heading naming the pane it is a list of.
  await page.click(`document.querySelector('.map-inv .map-pill[data-list="candidate"]')`);
  await page.waitFor("the sheet to open on the candidate list",
    `document.querySelector('.sheet')?.dataset.snap !== 'peek' && !!document.querySelector('.sheet .side-inv')`,
    { timeoutMs: 15000 });
  await page.frames(4);
  const sheet = JSON.parse(await page.eval(`JSON.stringify((() => {
    const h = document.querySelector('.sheet .side-inv .h');
    const r = h.getBoundingClientRect();
    return { text: h.textContent.replace(/\s+/g, ' ').trim(), pane: h.dataset.pane ?? null, w: r.width, h: r.height };
  })())`));
  t.diagnostic(`the lists, in the sheet: ${JSON.stringify(sheet)}`);
  assert.equal(sheet.pane, "1", "the lists in the sheet do not name the pane they are of");
  assert.match(sheet.text, /pane 1 of 2/);
  assert.ok(sheet.w > 80 && sheet.h > 8, `the heading is not visible: ${JSON.stringify(sheet)}`);
  await page.shot(path.join(ART, `app-pane-inventory-${width}-split.png`));

  // Choosing, freezing and scrubbing a pane is view state: nothing here reaches the front end.
  assert.deepEqual(page.requests.filter((r) => CONTROL.test(r.url)).map((r) => r.url), [],
    "scoping the lists to a pane reached a device route");
  assert.deepEqual(page.exceptions, [], "uncaught exception");
});
